//! HTTP surface: `POST /v1/rag/query` (embed -> retrieve -> ground -> answer).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use sqlx::PgPool;
use uuid::Uuid;

use crate::retrieve::{self, Filters, Source, Tuning};
use crate::state::AppState;

/// Defensive upper bound on synthesized text (answers are 1-3 sentences).
const MAX_TTS_CHARS: usize = 2000;

#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    pub query: String,
    #[serde(default)]
    pub top_k: Option<i64>,
    #[serde(default)]
    pub filters: Option<QueryFilters>,
    /// When true AND a speaker filter is present, route to the exhaustive non-semantic
    /// listing ("everything Bob said") instead of the top-k semantic search.
    #[serde(default)]
    pub exhaustive: Option<bool>,
    /// Select an agent for the single-shot path. Absent or `"recordings"` -> the existing
    /// grounded behaviour; `"reflection"` -> the speaker-scoped reflection digest path
    /// (the voice "how have I been" case). Unknown id -> 400.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Caller's UTC offset in seconds for rendering relative times in local civil time; absent
    /// falls back to `ANALYSIS_TZ_OFFSET_SECS`. Mirrors `ChatRequest::tz_offset_secs`.
    #[serde(default)]
    pub tz_offset_secs: Option<i64>,
    /// Who is asking (CONTEXT, never a retrieval filter — the `playback` precedent). Absent
    /// from old clients / ignored by old servers (serde skips unknown fields).
    #[serde(default)]
    pub caller: Option<CallerContext>,
}

/// Who is asking, as asserted by the client. `owner_verified` is a boolean claim by the
/// bearer-authenticated device (the Android app sets it only after its on-device voiceprint
/// check passes against the enrolled owner) — the Vosk speaker space is NOT the backend's
/// TitaNet space, so no embedding crosses the wire; the claim shares the same trust boundary
/// as the bearer token itself. It unlocks identity phrasing and the owner prompt line; it is
/// NEVER merged into retrieval filters.
#[derive(Debug, Default, Deserialize)]
pub struct CallerContext {
    /// Client kind; `"voice"` marks a spoken client whose answers are read aloud by TTS
    /// (personas get the spoken-style suffix).
    #[serde(default)]
    pub kind: Option<String>,
    /// The on-device owner voice check passed for THIS utterance.
    #[serde(default)]
    pub owner_verified: bool,
    /// The asking device (context for logs/future deictic use; not a filter).
    #[serde(default)]
    pub device_id: Option<String>,
}

impl CallerContext {
    /// Is this a spoken client (answers go to TTS)?
    pub(crate) fn is_voice(&self) -> bool {
        self.kind.as_deref() == Some("voice")
    }
}

impl QueryRequest {
    /// The tz offset to render times with: the caller's, else the configured default.
    pub(crate) fn tz_offset(&self, st: &AppState) -> i64 {
        self.tz_offset_secs.unwrap_or(st.cfg.analysis_tz_offset_secs)
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct QueryFilters {
    pub device_id: Option<String>,
    pub after_unix_nanos: Option<i64>,
    pub before_unix_nanos: Option<i64>,
    /// Explicit speaker ids (uuids as strings). Takes precedence over `speaker_name`.
    pub speaker_id: Option<Vec<String>>,
    /// Speaker display name; resolved globally (cross-device) to ids. Unknown -> no
    /// sources; ambiguous -> union of all matching ids.
    pub speaker_name: Option<String>,
    /// Explicit person (face) ids (uuids as strings). Takes precedence over `person_name`.
    /// Used by the `people` agent ("when did I see Bob").
    pub person_id: Option<Vec<String>>,
    /// Person display name; resolved globally to ids (same contract as `speaker_name`).
    pub person_name: Option<String>,
    /// Explicit license-plate ids (uuids as strings). Takes precedence over `plate_text`.
    /// Used by the `plates` agent ("when did I see plate ABC123").
    pub plate_id: Option<Vec<String>>,
    /// A raw plate string; resolved to ids by NORMALIZED match (exact + pg_trgm fuzzy). Unknown ->
    /// no sources; matches several catalog rows -> union of all matching ids.
    pub plate_text: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct QueryResponse {
    pub answer: String,
    pub sources: Vec<Source>,
}

pub async fn rag_query(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, (StatusCode, String)> {
    check_auth(&headers, &st)?;
    hushai_backend::observe::counter("hushai_rag_requests_total", &[("endpoint", "query")]);

    if req.query.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "query must not be empty".into()));
    }

    // Resolve the agent: absent -> default grounded; unknown id -> 400.
    let agent = match req.agent_id.as_deref() {
        None => crate::agents::default(),
        Some(id) => crate::agents::get(id)
            .ok_or((StatusCode::BAD_REQUEST, format!("unknown agent: {id}")))?,
    };

    // Deterministic caller-identity answer ("what's my name"), before any retrieval/LLM. Only on
    // the grounded default (a specialized agent explicitly chosen keeps its own behaviour); a
    // stranger (no owner_verified) still gets a graceful, non-confirming reply.
    if agent.kind == crate::agents::AgentKind::Grounded && is_identity_query(&req.query) {
        let owner_verified = req.caller.as_ref().is_some_and(|c| c.owner_verified);
        let name = resolve_owner_name(&st).await.map_err(internal)?;
        return Ok(Json(QueryResponse {
            answer: render_identity(name.as_deref(), owner_verified),
            sources: vec![],
        }));
    }

    // Recency ("what did we last discuss") → summarize the latest gap-grouped conversation instead
    // of semantic top-k. Grounded default only; specialized agents keep their own routing.
    if agent.kind == crate::agents::AgentKind::Grounded && is_recency_query(&req.query) {
        return recency_query(&st, &req).await;
    }

    // "What did X and Y talk about" → the persisted conversation catalog (0025). Fires only
    // when ≥1 catalog voice name resolves (the phrase alone must not hijack "what did they
    // talk about"); no resolved names falls through to the normal paths.
    if agent.kind == crate::agents::AgentKind::Grounded
        && is_participants_conversation_query(&req.query)
    {
        let pids = crate::speakers::resolve_names_in_text(&st.pool, &req.query)
            .await
            .map_err(internal)?;
        if !pids.is_empty() {
            return participants_query(&st, &req, &pids).await;
        }
    }

    if agent.kind == crate::agents::AgentKind::Reflection {
        return reflection_query(&st, &req.query, req.filters.unwrap_or_default(), agent).await;
    }
    if agent.kind == crate::agents::AgentKind::Objects {
        return objects_query(&st, &req).await;
    }
    if agent.kind == crate::agents::AgentKind::People {
        return people_query(&st, &req).await;
    }
    if agent.kind == crate::agents::AgentKind::Plates {
        return plates_query(&st, &req).await;
    }

    let top_k = req.top_k.unwrap_or(st.cfg.top_k_default).clamp(1, 50);
    let tz = req.tz_offset(&st); // capture before `req.filters` is moved out below
    let qf = req.filters.unwrap_or_default();

    let speaker_id = resolve_speaker_filter(&st.pool, qf.speaker_id, qf.speaker_name)
        .await
        .map_err(internal)?;

    let filters = Filters {
        device_id: qf.device_id.clone(),
        after_unix_nanos: qf.after_unix_nanos,
        before_unix_nanos: qf.before_unix_nanos,
        speaker_id: speaker_id.clone(),
    };

    // Route: exhaustive attribution ("everything Bob said") vs topical semantic search.
    // Exhaustive only when explicitly requested AND a non-empty speaker filter is present;
    // it skips the query embedding entirely (no vector ranking) and the distance prune.
    let want_exhaustive = req.exhaustive.unwrap_or(false)
        && filters.speaker_id.as_ref().is_some_and(|v| !v.is_empty());

    let mut sources = if want_exhaustive {
        retrieve::list_by_speaker(
            &st.pool,
            filters.speaker_id.as_deref().unwrap_or_default(),
            filters.device_id.as_deref(),
            filters.after_unix_nanos,
            filters.before_unix_nanos,
            top_k.max(50),
        )
        .await
        .map_err(internal)?
    } else {
        // Embed the query in the same 1024-dim space, then pgvector NN retrieval.
        let embedding = st.embedder.embed_one(&req.query).await.map_err(internal)?;
        let tuning = Tuning {
            ef_search: st.cfg.hnsw_ef_search,
            statement_timeout_ms: st.cfg.query_timeout_ms,
        };
        retrieve::nearest(&st.pool, &embedding, top_k, &tuning, &filters)
            .await
            .map_err(internal)?
    };

    // Drop weak matches so we neither ground on nor cite irrelevant passages. (Exhaustive
    // rows have distance 0.0, so they survive this unchanged.)
    sources.retain(|s| s.distance <= st.cfg.distance_threshold);
    // Then drop hits much weaker than the best one — the absolute cutoff alone lets an
    // unrelated conversation's marginal hit through, and expansion below would amplify it
    // into that entire conversation.
    retrieve::prune_rel_margin(&mut sources, st.cfg.prune_rel_margin);

    // Conversation-neighborhood expansion (0025): pruned hits widen into their persisted
    // conversation's surrounding sentences and the answer prompt renders per-conversation
    // sections, so two concurrent conversations can never blend into one answer. Kill
    // switch + budgets in config; unthreaded hits keep the flat single-sentence behaviour.
    let mut convo_group_lens: Option<Vec<usize>> = None;
    if st.cfg.expand_enabled
        && !want_exhaustive
        && sources.iter().any(|s| s.conversation_id.is_some())
    {
        let groups = retrieve::expand_to_conversations(
            &st.pool,
            &sources,
            st.cfg.expand_window_secs.saturating_mul(1_000_000_000),
            st.cfg.expand_max_sentences_per_convo,
            st.cfg.expand_max_total_chars,
        )
        .await
        .map_err(internal)?;
        convo_group_lens = Some(groups.iter().map(|g| g.len()).collect());
        sources = groups.into_iter().flatten().collect();
    }

    // Resolve speaker names once for attribution, then ask the LLM for a grounded answer
    // (it declines when sources is empty).
    let ids: Vec<String> = sources
        .iter()
        .filter_map(|s| s.speaker_id.clone())
        .collect();
    let names = crate::speakers::name_map(&st.pool, &ids)
        .await
        .map_err(internal)?;
    // Attach human-readable speaker names + relative time to each source so the prompt (and
    // the returned citations) carry natural language instead of UUIDs / nanoseconds.
    retrieve::enrich_for_display(
        &mut sources,
        &names,
        Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX),
        tz,
    );
    // Same-segment vision annotation (who was on camera / objects / plates) for the citations and
    // the prompt — env-gated + best-effort, matching the chat path.
    if st.cfg.context_vision_enrich_enabled {
        if let Err(e) = crate::context::enrich_sources_with_vision(&st.pool, &mut sources, 3).await {
            tracing::warn!(error = format!("{e:#}"), "vision enrichment skipped");
        }
    }
    // Grouped rendering only when the expansion actually found >1 conversation; a single
    // group (or no threading) keeps the flat prompt byte-identical.
    let answer = match &convo_group_lens {
        Some(lens) if lens.len() > 1 => {
            let groups = retrieve::regroup_sources(&sources, lens);
            st.llm
                .answer_grouped(&req.query, &groups, &names)
                .await
                .map_err(internal)?
        }
        _ => st
            .llm
            .answer(&req.query, &sources, &names)
            .await
            .map_err(internal)?,
    };

    Ok(Json(QueryResponse { answer, sources }))
}

/// Shown when the reflection agent can't resolve whose conversations to analyze (no
/// request speaker, and no configured owner). Returned verbatim instead of an LLM call.
pub(crate) const REFLECTION_NO_TARGET: &str = "I'm not sure which voice is yours yet. Open the Voices screen, name your own voice, and \
     set the owner so I can analyze your conversations — then ask me again.";

/// Single-shot reflection answer (the voice "how have I been" path). Computes a
/// deterministic life digest over the target speaker, then has the LLM narrate it.
async fn reflection_query(
    st: &AppState,
    question: &str,
    qf: QueryFilters,
    agent: &crate::agents::Agent,
) -> Result<Json<QueryResponse>, (StatusCode, String)> {
    let target = resolve_target_speaker(st, qf.speaker_id.clone(), qf.speaker_name.clone())
        .await
        .map_err(internal)?;
    if target.is_empty() {
        return Ok(Json(QueryResponse {
            answer: REFLECTION_NO_TARGET.to_string(),
            sources: vec![],
        }));
    }

    let days = agent
        .default_window_days
        .unwrap_or(st.cfg.analysis_window_days_default);
    let window =
        crate::analytics::AnalysisWindow::resolve(qf.after_unix_nanos, qf.before_unix_nanos, days);
    let dcfg = crate::analytics::DigestConfig::from_rag(&st.cfg);

    // Single-shot (voice) — no question embedding, so no on-topic semantic excerpts.
    let digest = crate::analytics::compute_digest(&st.pool, &target, window, &dcfg, None)
        .await
        .map_err(internal)?;
    let digest_text = crate::analytics::render_digest(&digest);
    let mut sources = digest.excerpts;

    let ids: Vec<String> = sources
        .iter()
        .filter_map(|s| s.speaker_id.clone())
        .collect();
    let names = crate::speakers::name_map(&st.pool, &ids)
        .await
        .map_err(internal)?;
    retrieve::enrich_for_display(
        &mut sources,
        &names,
        Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX),
        st.cfg.analysis_tz_offset_secs,
    );
    let model = st.cfg.reflection_llm_model.as_deref().or(agent.model);
    let answer = st
        .llm
        .reflect(question, &digest_text, &sources, &names, model)
        .await
        .map_err(internal)?;
    Ok(Json(QueryResponse { answer, sources }))
}

/// Single-shot open-vocabulary OBJECT answer (the "when did I see a car / a red mug" path).
/// Embeds the phrase with the CLIP TEXT tower and nearest-neighbours over `scene_objects` (the
/// OpenCLIP space — never `person_segments`), or, when `exhaustive` is set, lists every sighting of
/// the exact object class. Returns 503 when the CLIP text tower isn't loaded.
async fn objects_query(
    st: &AppState,
    req: &QueryRequest,
) -> Result<Json<QueryResponse>, (StatusCode, String)> {
    let clip = st.clip.clone().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "object retrieval is not enabled on this server".to_string(),
    ))?;

    let qf = req.filters.as_ref();
    let device_id = qf.and_then(|f| f.device_id.clone());
    let after = qf.and_then(|f| f.after_unix_nanos);
    let before = qf.and_then(|f| f.before_unix_nanos);
    let top_k = req
        .top_k
        .unwrap_or(st.cfg.object_top_k_default)
        .clamp(1, 50);
    let filters = Filters {
        device_id: device_id.clone(),
        after_unix_nanos: after,
        before_unix_nanos: before,
        speaker_id: None,
    };

    // Exhaustive: list every sighting of the EXACT COCO class (no recall cliff) — but only when the
    // query resolves to a real COCO label. A non-COCO phrase ("a spaceship", "people walking") falls
    // through to the open-vocab semantic path instead of silently returning nothing.
    let exhaustive_label = if req.exhaustive.unwrap_or(false) {
        normalize_object_label(&req.query)
    } else {
        None
    };
    let mut sources = if let Some(label) = exhaustive_label {
        retrieve::list_by_object_class(
            &st.pool,
            &[label],
            device_id.as_deref(),
            after,
            before,
            top_k.max(50),
        )
        .await
        .map_err(internal)?
    } else {
        // Open-vocab semantic path (CLIP text NN over scene_objects, incl. the whole-frame rows) —
        // also the FALLBACK when an exhaustive query names a non-COCO class.
        let q = req.query.clone();
        // CLIP text embedding is CPU-bound ONNX work — off the async runtime.
        let embedding = tokio::task::spawn_blocking(move || clip.embed_text(&q))
            .await
            .map_err(|e| internal(anyhow::anyhow!("clip text task join: {e}")))?
            .map_err(internal)?;
        let tuning = Tuning {
            ef_search: st.cfg.hnsw_ef_search,
            statement_timeout_ms: st.cfg.query_timeout_ms,
        };
        retrieve::nearest_objects(&st.pool, &embedding, top_k, &tuning, &filters, true)
            .await
            .map_err(internal)?
    };

    // CLIP cosine is looser than text embeddings — prune with the object-specific threshold so a
    // nonsense phrase ("a spaceship") returns nothing rather than the least-bad frame. Exhaustive
    // rows have distance 0.0 and survive.
    sources.retain(|s| s.distance <= st.cfg.object_distance_threshold);

    // Humanize the sighting time (no speaker enrichment for objects).
    let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
    let tz = req.tz_offset(&st);
    for s in &mut sources {
        s.time_label = crate::humanize::humanize_time(s.start_unix_nanos, now, tz);
    }

    let answer = st
        .llm
        .answer_objects(&req.query, &sources)
        .await
        .map_err(internal)?;
    Ok(Json(QueryResponse { answer, sources }))
}

/// The 80 COCO class names the object detector writes to `scene_objects` (the values of
/// hushai-worker's `coco91_class_names`). Used to resolve an exhaustive query to a REAL label.
fn coco_labels() -> &'static std::collections::HashSet<&'static str> {
    static S: std::sync::OnceLock<std::collections::HashSet<&'static str>> =
        std::sync::OnceLock::new();
    S.get_or_init(|| {
        [
            "person", "bicycle", "car", "motorcycle", "airplane", "bus", "train", "truck", "boat",
            "traffic light", "fire hydrant", "stop sign", "parking meter", "bench", "bird", "cat",
            "dog", "horse", "sheep", "cow", "elephant", "bear", "zebra", "giraffe", "backpack",
            "umbrella", "handbag", "tie", "suitcase", "frisbee", "skis", "snowboard", "sports ball",
            "kite", "baseball bat", "baseball glove", "skateboard", "surfboard", "tennis racket",
            "bottle", "wine glass", "cup", "fork", "knife", "spoon", "bowl", "banana", "apple",
            "sandwich", "orange", "broccoli", "carrot", "hot dog", "pizza", "donut", "cake", "chair",
            "couch", "potted plant", "bed", "dining table", "toilet", "tv", "laptop", "mouse",
            "remote", "keyboard", "cell phone", "microwave", "oven", "toaster", "sink",
            "refrigerator", "book", "clock", "vase", "scissors", "teddy bear", "hair drier",
            "toothbrush",
        ]
        .into_iter()
        .collect()
    })
}

/// Common plurals/synonyms that don't fall out of naive singularization → exact COCO label.
fn object_alias(s: &str) -> Option<&'static str> {
    Some(match s {
        "people" | "persons" | "human" | "humans" | "man" | "woman" | "men" | "women" | "guy"
        | "guys" | "somebody" | "someone" => "person",
        "buses" => "bus",
        "knives" => "knife",
        "wine glasses" | "wineglass" | "wineglasses" => "wine glass",
        "television" | "televisions" | "tvs" | "tv set" => "tv",
        "phone" | "phones" | "cellphone" | "cellphones" | "cell phones" | "mobile phone"
        | "smartphone" | "smartphones" => "cell phone",
        "sofa" | "sofas" | "couches" => "couch",
        "laptops" => "laptop",
        "plants" | "potted plants" => "potted plant",
        _ => return None,
    })
}

/// Resolve a natural phrase to a bare COCO class for the exact-class exhaustive path, or `None` if it
/// isn't a COCO class (so the caller falls back to the semantic path instead of returning nothing).
/// Fixes the old naive single-'s' strip that mangled compound/irregular labels (people→peopl,
/// buses→buse, scissors→scissor) and silently emptied the exhaustive answer.
fn normalize_object_label(query: &str) -> Option<String> {
    let lower = query
        .trim()
        .trim_end_matches(['?', '.', '!', ','])
        .to_lowercase();
    let s = {
        let l = lower.trim();
        l.strip_prefix("a ")
            .or_else(|| l.strip_prefix("an "))
            .or_else(|| l.strip_prefix("the "))
            .unwrap_or(l)
            .trim()
    };
    // Exact COCO label (covers singulars that end in 's' like "scissors"/"skis").
    if coco_labels().contains(s) {
        return Some(s.to_string());
    }
    // Known irregular plural / synonym.
    if let Some(l) = object_alias(s) {
        return Some(l.to_string());
    }
    // Regular plural → singular, but ONLY if it lands on a real label ("cars"→"car", "dogs"→"dog").
    for cand in [s.strip_suffix("es"), s.strip_suffix('s')].into_iter().flatten() {
        if coco_labels().contains(cand) {
            return Some(cand.to_string());
        }
    }
    None
}

/// Scan a free-text QUESTION (a whole sentence) for a COCO object class, checking bigrams first
/// (so "wine glass"/"cell phone"/"stop sign" win over their parts) then unigrams. Used by the chat
/// Objects arm to answer "how many times did I see a car" from the deterministic presence rollup —
/// `normalize_object_label` alone only resolves a query that IS the object phrase, not a sentence.
pub(crate) fn find_object_class_in_query(query: &str) -> Option<String> {
    let lower = query.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    for w in words.windows(2) {
        if let Some(l) = normalize_object_label(&format!("{} {}", w[0], w[1])) {
            return Some(l);
        }
    }
    for w in &words {
        if let Some(l) = normalize_object_label(w) {
            return Some(l);
        }
    }
    None
}

/// Shown when "who was I with" can't resolve the owner (no request person, no configured owner).
pub(crate) const PEOPLE_NO_OWNER: &str = "I'm not sure which face is yours yet. Open the People screen, name your own face, and set \
     OWNER_PERSON_NAME so I can tell who you were with — then ask me again.";

/// Single-shot PERSON answer. Routing lives in [`resolve_people_sources`]:
///   - an explicit person filter (id/name) OR a catalog name mentioned in the query → exhaustive
///     per-person sightings (`list_by_person`);
///   - first-person "who was I with" → same-segment co-occurrence around the configured owner;
///   - otherwise → the roster of everyone seen ("who have you seen so far").
async fn people_query(
    st: &AppState,
    req: &QueryRequest,
) -> Result<Json<QueryResponse>, (StatusCode, String)> {
    let qf = req.filters.as_ref();
    let device_id = qf.and_then(|f| f.device_id.clone());
    let after = qf.and_then(|f| f.after_unix_nanos);
    let before = qf.and_then(|f| f.before_unix_nanos);
    let limit = req
        .top_k
        .unwrap_or(st.cfg.person_top_k_default)
        .clamp(1, 200);

    let mut sources = match resolve_people_sources(
        st,
        &req.query,
        qf.and_then(|f| f.person_id.clone()),
        qf.and_then(|f| f.person_name.clone()),
        device_id.as_deref(),
        after,
        before,
        limit,
    )
    .await
    .map_err(internal)?
    {
        PeopleSources::Found(s) => s,
        PeopleSources::NeedsOwner => {
            return Ok(Json(QueryResponse {
                answer: PEOPLE_NO_OWNER.to_string(),
                sources: vec![],
            }));
        }
    };

    // Person attribution display: resolve names, number distinct unnamed faces, humanize the time.
    let ids: Vec<String> = sources
        .iter()
        .filter_map(|s| s.speaker_id.clone())
        .collect();
    let names = crate::persons::name_map(&st.pool, &ids)
        .await
        .map_err(internal)?;
    enrich_persons_for_display(
        &mut sources,
        &names,
        Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX),
        req.tz_offset(&st),
    );

    let answer = st
        .llm
        .answer_people(&req.query, &sources, &names)
        .await
        .map_err(internal)?;
    Ok(Json(QueryResponse { answer, sources }))
}

/// Single-shot RECENCY answer (the "what did we last discuss" path). Resolves the window (explicit
/// filters win, else a `timeparse` phrase like "yesterday", else unbounded → the newest activity),
/// pulls the most recent gap-grouped conversation via `retrieve::latest_conversation`, enriches for
/// display (speaker names + humanized time), and has the LLM summarize it. Mirrors `people_query`.
pub(crate) async fn recency_query(
    st: &AppState,
    req: &QueryRequest,
) -> Result<Json<QueryResponse>, (StatusCode, String)> {
    let tz = req.tz_offset(&st);
    let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
    let qf = req.filters.as_ref();
    let device_id = qf.and_then(|f| f.device_id.clone());
    // Window precedence: explicit filters win; else a natural-language phrase; else unbounded.
    let parsed = crate::timeparse::window_in_query(&req.query, now, tz);
    let after = qf.and_then(|f| f.after_unix_nanos).or(parsed.map(|(a, _)| a));
    let before = qf.and_then(|f| f.before_unix_nanos).or(parsed.map(|(_, b)| b));
    let gap_nanos = st.cfg.conversation_gap_secs.max(1) * 1_000_000_000;

    let mut sources = retrieve::latest_conversation(
        &st.pool,
        device_id.as_deref(),
        after,
        before,
        gap_nanos,
        st.cfg.recency_scan_limit,
        st.cfg.recency_max_sentences,
        st.cfg.recency_max_chars,
    )
    .await
    .map_err(internal)?;

    let ids: Vec<String> = sources.iter().filter_map(|s| s.speaker_id.clone()).collect();
    let names = crate::speakers::name_map(&st.pool, &ids)
        .await
        .map_err(internal)?;
    retrieve::enrich_for_display(&mut sources, &names, now, tz);

    let answer = st
        .llm
        .answer_recency(&req.query, &sources, &names)
        .await
        .map_err(internal)?;
    Ok(Json(QueryResponse { answer, sources }))
}

/// Conversation-grouped sources for a PARTICIPANTS question ("what did X and Y talk
/// about"): newest conversations from the 0025 catalog whose `speaker_ids` contain every
/// resolved participant, each expanded to its ordered transcript. Oldest-first for
/// narration. Empty when nothing threaded matches (the grouped prompt then declines —
/// unthreaded history is reachable via the speaker filter / exhaustive paths instead).
pub(crate) async fn participants_conversation_groups(
    st: &AppState,
    participant_ids: &[Uuid],
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
) -> anyhow::Result<Vec<Vec<Source>>> {
    let metas = retrieve::list_conversations(
        &st.pool,
        device_id,
        after,
        before,
        Some(participant_ids),
        st.cfg.summary_max_convos.max(1) as i64,
    )
    .await?;
    let mut groups = Vec::new();
    for m in &metas {
        let t = retrieve::conversation_transcript(
            &st.pool,
            m.conversation_id,
            st.cfg.recency_max_sentences,
        )
        .await?;
        if !t.is_empty() {
            groups.push(t);
        }
    }
    groups.reverse(); // newest-first catalog order → oldest-first narration
    Ok(groups)
}

/// Single-shot participants answer (the `/v1/rag/query` mirror of the chat branch).
pub(crate) async fn participants_query(
    st: &AppState,
    req: &QueryRequest,
    participant_ids: &[Uuid],
) -> Result<Json<QueryResponse>, (StatusCode, String)> {
    let tz = req.tz_offset(&st);
    let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
    let qf = req.filters.as_ref();
    let parsed = crate::timeparse::window_in_query(&req.query, now, tz);
    let after = qf.and_then(|f| f.after_unix_nanos).or(parsed.map(|(a, _)| a));
    let before = qf.and_then(|f| f.before_unix_nanos).or(parsed.map(|(_, b)| b));
    let device_id = qf.and_then(|f| f.device_id.clone());

    let mut groups =
        participants_conversation_groups(st, participant_ids, device_id.as_deref(), after, before)
            .await
            .map_err(internal)?;

    // Enrich FLAT (global unnamed ordinals), then re-group by the recorded lengths.
    let lens: Vec<usize> = groups.iter().map(|g| g.len()).collect();
    let mut flat: Vec<Source> = groups.drain(..).flatten().collect();
    let ids: Vec<String> = flat.iter().filter_map(|s| s.speaker_id.clone()).collect();
    let names = crate::speakers::name_map(&st.pool, &ids)
        .await
        .map_err(internal)?;
    retrieve::enrich_for_display(&mut flat, &names, now, tz);
    let groups = retrieve::regroup_sources(&flat, &lens);

    let answer = st
        .llm
        .answer_grouped(&req.query, &groups, &names)
        .await
        .map_err(internal)?;
    Ok(Json(QueryResponse {
        answer,
        sources: flat,
    }))
}

/// Query params for `GET /v1/rag/conversations`.
#[derive(Debug, Default, Deserialize)]
pub struct ConversationsListParams {
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub after_unix_nanos: Option<i64>,
    #[serde(default)]
    pub before_unix_nanos: Option<i64>,
    /// Restrict to conversations where this named voice spoke.
    #[serde(default)]
    pub speaker_name: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ConversationSummary {
    pub conversation_id: String,
    pub device_id: Option<String>,
    pub started_at_unix_nanos: i64,
    pub ended_at_unix_nanos: i64,
    pub status: String,
    /// Resolved display labels ("Alice", "unidentified speaker"...), stable order.
    pub participants: Vec<String>,
    pub sentence_count: i32,
}

/// `GET /v1/rag/conversations` — list threaded conversations (0025), newest first.
pub async fn list_conversations_route(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ConversationsListParams>,
) -> Result<Json<Vec<ConversationSummary>>, (StatusCode, String)> {
    check_auth(&headers, &st)?;
    let speaker_ids: Option<Vec<Uuid>> = match &params.speaker_name {
        Some(name) => {
            let ids = crate::speakers::resolve_name(&st.pool, name)
                .await
                .map_err(internal)?;
            if ids.is_empty() {
                return Ok(Json(vec![])); // unknown name matches nothing, never unfiltered
            }
            Some(ids)
        }
        None => None,
    };
    let metas = retrieve::list_conversations(
        &st.pool,
        params.device_id.as_deref(),
        params.after_unix_nanos,
        params.before_unix_nanos,
        speaker_ids.as_deref(),
        params.limit.unwrap_or(50),
    )
    .await
    .map_err(internal)?;
    let all_ids: Vec<String> = metas.iter().flat_map(|m| m.speaker_ids.clone()).collect();
    let names = crate::speakers::name_map(&st.pool, &all_ids)
        .await
        .map_err(internal)?;
    let out = metas
        .into_iter()
        .map(|m| {
            let participants = m
                .speaker_ids
                .iter()
                .map(|id| crate::speakers::display_label(Some(id), &names, None))
                .collect();
            ConversationSummary {
                conversation_id: m.conversation_id.to_string(),
                device_id: m.device_id,
                started_at_unix_nanos: m.started_at_unix_nanos,
                ended_at_unix_nanos: m.ended_at_unix_nanos,
                status: m.status,
                participants,
                sentence_count: m.sentence_count,
            }
        })
        .collect();
    Ok(Json(out))
}

/// `GET /v1/rag/conversations/{id}` — one conversation's ordered, enriched transcript.
pub async fn get_conversation_route(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<Source>>, (StatusCode, String)> {
    check_auth(&headers, &st)?;
    let mut sources = retrieve::conversation_transcript(&st.pool, id, 500)
        .await
        .map_err(internal)?;
    let ids: Vec<String> = sources.iter().filter_map(|s| s.speaker_id.clone()).collect();
    let names = crate::speakers::name_map(&st.pool, &ids)
        .await
        .map_err(internal)?;
    let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
    retrieve::enrich_for_display(&mut sources, &names, now, st.cfg.analysis_tz_offset_secs);
    Ok(Json(sources))
}

/// Person analogue of `retrieve::enrich_for_display`: set `speaker_name` via the PERSON label rules
/// (`persons::display_label` — "unidentified person N" / "an unrecognized face") and the humanized
/// `time_label`. (A face row carries `person_id::text` in `speaker_id`.)
pub(crate) fn enrich_persons_for_display(
    sources: &mut [Source],
    names: &std::collections::HashMap<String, String>,
    now_unix_nanos: i64,
    tz_offset_secs: i64,
) {
    let ordinals = crate::persons::assign_unnamed_ordinals(
        sources.iter().map(|s| s.speaker_id.as_deref()),
        names,
    );
    for s in sources.iter_mut() {
        let ordinal = s
            .speaker_id
            .as_deref()
            .and_then(|id| ordinals.get(id).copied());
        s.speaker_name = Some(crate::persons::display_label(
            s.speaker_id.as_deref(),
            names,
            ordinal,
        ));
        s.time_label =
            crate::humanize::humanize_time(s.start_unix_nanos, now_unix_nanos, tz_offset_secs);
    }
}

/// Outcome of routing a People-agent question to its sightings (shared by the single-shot query
/// path and the chat path, so the two can't drift).
pub(crate) enum PeopleSources {
    /// Real sightings to enrich + feed the LLM (possibly empty → the LLM says it saw no one).
    Found(Vec<Source>),
    /// A first-person "who was I with" question but the owner face is unknown — the caller renders
    /// the [`PEOPLE_NO_OWNER`] setup hint instead.
    NeedsOwner,
}

/// Is this a first-person "who was I with / around me" question (co-occurrence anchored on the
/// owner) rather than a general roster ("who have you seen", "who's been around")? Only the former
/// needs to know which face is the owner; everything else is answered from the full roster. Kept
/// deliberately narrow so a roster question never gets misrouted into the owner-required path.
pub(crate) fn is_co_occurrence_query(query: &str) -> bool {
    let q = query.to_lowercase();
    [
        "with me",
        "was i with",
        "were with me",
        "been with me",
        "around me",
        "near me",
        "next to me",
        "i was with",
        "i been with",
        "with whom",
        "accompany me",
        "accompanied me",
    ]
    .iter()
    .any(|p| q.contains(p))
}

/// Shown when a question points at "this video/camera" but no camera is scoped and there's more
/// than one — we can't know which one they mean, so we ask instead of answering across all.
pub(crate) const CAMERA_CLARIFY: &str = "You're searching across all cameras, so I'm not sure which video you mean. \
     Pick a camera from the dropdown above and ask again — or tell me a name and I'll say where and when \
     they were last seen across all of them.";

/// Does the question point at a SPECIFIC currently-viewed video/camera ("this video", "in the
/// clip", "on screen") rather than the whole archive? Deliberately narrow phrase match — the caller
/// only acts on it when no camera is scoped AND more than one camera exists.
pub(crate) fn is_deictic_video_query(query: &str) -> bool {
    let q = query.to_lowercase();
    [
        "this video",
        "this clip",
        "this camera",
        "this feed",
        "this footage",
        "this recording",
        "this stream",
        "in the video",
        "in the clip",
        "on screen",
        "on the screen",
        "on-screen",
        "currently watching",
        "currently playing",
        "right now on",
    ]
    .iter()
    .any(|p| q.contains(p))
}

/// Half-width of the time window anchored on the viewer's playhead for a deictic question
/// ("who was speaking in this clip"): ±120 s. Segments are ~2 s ingest units, so the containing
/// segment alone would usually hold one sentence and miss speakers seconds away; two minutes
/// either side matches what a human means by "this clip" without another DB round trip.
pub(crate) const DEICTIC_CLIP_WINDOW_NANOS: i64 = 120_000_000_000;

/// Is the question a SPEAKER-ROSTER ask ("who was speaking / talking", "whose voice") rather
/// than content attribution ("who said X" stays on the semantic path)? Deliberately narrow
/// phrase match, same idiom as [`is_deictic_video_query`]. Callers use it two ways: the
/// auto-router pre-routes these to `recordings` (a voice question — the LLM router's "who"
/// pattern drifts toward the people/faces agent), and the Grounded arm answers them
/// deterministically when the turn carries a bounded time window.
pub(crate) fn is_speaker_roster_query(query: &str) -> bool {
    let q = query.to_lowercase();
    [
        "who was speaking",
        "who is speaking",
        "who's speaking",
        "who was talking",
        "who is talking",
        "who's talking",
        "who spoke",
        "whose voice",
        "who do you hear",
        "who can you hear",
        "who did you hear",
    ]
    .iter()
    .any(|p| q.contains(p))
}

/// Is the question a CALLER-IDENTITY ask ("what's my name", "who am I")? Deliberately narrow
/// phrase match, same idiom as [`is_speaker_roster_query`]. Answered deterministically from the
/// caller context + resolved owner name (before any retrieval/LLM), so a voice-verified owner
/// gets their name back instead of the persona's "I don't have that in the recordings" decline.
/// Kept tight so an ordinary content question ("what did I say about my name") never trips it.
pub(crate) fn is_identity_query(query: &str) -> bool {
    let q = query.trim().to_lowercase();
    // These phrases are the whole ask, so `contains` is safe. Deliberately NOT including bare
    // "say my name" / "my name" — those match content questions ("did anyone say my name
    // yesterday", "what's the name of that place"), which must stay on the retrieval path.
    [
        "what's my name",
        "whats my name",
        "what is my name",
        "who am i",
        "do you know my name",
        "do you know who i am",
        "what am i called",
        "tell me my name",
    ]
    .iter()
    .any(|p| q.contains(p))
}

/// Shown to a caller-identity question when NO owner is configured anywhere (no DB "This is me"
/// mark, no OWNER_* env). Returned verbatim instead of an LLM call, mirroring `REFLECTION_NO_TARGET`.
pub(crate) const IDENTITY_NO_OWNER: &str = "I don't have your name on file yet. Open the Voices screen, name your own voice, and tap \
     \"This is me\" — then I'll know who you are.";

/// The caller-identity answer given the resolved owner name and whether the client's on-device
/// voice check verified the owner for this turn. Deterministic (no LLM): a verified owner is
/// greeted by name; a known-but-unverified caller is told whose device it is without a false
/// confirmation; no configured owner returns the setup hint.
pub(crate) fn render_identity(owner_name: Option<&str>, owner_verified: bool) -> String {
    match owner_name {
        Some(name) if owner_verified => format!("Your name is {name}."),
        Some(name) => format!("This device belongs to {name}, but I can't confirm it's you speaking."),
        None => IDENTITY_NO_OWNER.to_string(),
    }
}

/// Is the question a RECENCY ask ("what did we last discuss", "what were we just talking about")?
/// Deliberately narrow phrase match, same idiom as [`is_speaker_roster_query`]. Routes to the
/// gap-grouped `latest_conversation` summarizer instead of semantic top-k (which returns a lone
/// keyword-similar 2s snippet for a recency question). Kept tight so "when did I LAST see Bob"
/// (a People/count question) and "who spoke last" (a roster question) do NOT trip it.
pub(crate) fn is_recency_query(query: &str) -> bool {
    let q = query.to_lowercase();
    [
        "last discuss",
        "last discussed",
        "last talk about",
        "last talked about",
        "last conversation",
        "latest conversation",
        "most recent conversation",
        "recent conversation",
        "last chat",
        "what were we talking about",
        "what were we just talking about",
        "what did we talk about last",
        "what have we been talking about",
        "what did we just discuss",
    ]
    .iter()
    .any(|p| q.contains(p))
}

/// Is the question about a CONVERSATION BETWEEN named participants ("what did X and Y talk
/// about", "the conversation between X and Y")? Same narrow-phrase idiom as
/// [`is_recency_query`]. Only meaningful when ≥1 catalog voice name resolves in the text
/// (the caller checks `speakers::resolve_names_in_text` — the `is_profile_query` guard
/// pattern); the phrase alone must not hijack "what did they talk about" from the
/// recency/window paths. Answered from the persisted conversation catalog (0025):
/// conversations whose `speaker_ids` contain every resolved participant.
pub(crate) fn is_participants_conversation_query(query: &str) -> bool {
    let q = query.to_lowercase();
    [
        "conversation between",
        "conversations between",
        "conversation with",
        "talk about with",
        "talked about with",
        " and ", // "what did X and Y talk about / discuss" — requires the name check
    ]
    .iter()
    .any(|p| q.contains(p))
        && ["talk", "talked", "talking", "discuss", "discussed", "discussing", "conversation", "say", "said"]
            .iter()
            .any(|p| q.contains(p))
}

/// Is the question "how many PEOPLE (distinct humans) were seen" — a distinct-person count over
/// the roster — rather than a frequency question about one subject ("how many TIMES did I see
/// Bob", which stays on the per-person presence rollup)? Same narrow-phrase idiom as
/// [`is_speaker_roster_query`]. The People arm answers it deterministically from the distinct
/// roster instead of letting the single-person rollup misfire ("Morgan was seen 62 times").
pub(crate) fn is_people_count_query(query: &str) -> bool {
    let q = query.to_lowercase();
    // Frequency phrasings are about ONE subject, never a distinct-people count.
    if q.contains("how many times") || q.contains("number of times") || q.contains("how often") {
        return false;
    }
    if !(q.contains("how many") || q.contains("number of")) {
        return false;
    }
    ["people", "persons", "faces", "visitors", "guests", "individuals"]
        .iter()
        .any(|n| q.contains(n))
}

/// Is the question about how MUCH footage exists ("how many minutes of video do we have today",
/// "how much audio was recorded") — pure segment arithmetic, answered deterministically from the
/// `segments` table (`stats::footage_stats`), never by semantic retrieval (which has nothing to
/// retrieve and declines — the observed "I don't have information about that" failure).
pub(crate) fn is_footage_stats_query(query: &str) -> bool {
    let q = query.to_lowercase();
    // "how many times did the video show X" is a frequency question, and "how many people were
    // on the recording" is a distinct-people count — neither is a footage total.
    if q.contains("how many times") || q.contains("number of times") || is_people_count_query(query) {
        return false;
    }
    // The footage noun (or a duration unit) must sit RIGHT AFTER the quantity marker —
    // "how much video", "how many minutes of footage", "total hours of video". A footage
    // word merely elsewhere in the sentence ("how many packages arrived, according to the
    // RECORDINGS?") is a content question and must stay on the retrieval path.
    const NOUNS: &[&str] = &[
        "video", "videos", "vid", "vids", "footage", "recording", "recordings", "audio",
        "minute", "minutes", "min", "mins", "hour", "hours", "hr", "hrs",
    ];
    let words: Vec<&str> = q
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect();
    for (i, pair) in words.windows(2).enumerate() {
        let qty_len = match pair {
            ["how", "many"] | ["how", "much"] => 2,
            [w, _] if *w == "total" => 1,
            _ => continue,
        };
        // The 1-2 tokens after the marker must include a footage noun / duration unit.
        let start = i + qty_len;
        if words[start..(start + 2).min(words.len())]
            .iter()
            .any(|w| NOUNS.contains(w))
        {
            return true;
        }
    }
    false
}

/// Is the question a WINDOW-SUMMARY ask ("what have we spoken about today", "what was discussed
/// this morning") — summarize ALL of the window's conversations — as opposed to a recency ask
/// ("what did we JUST discuss" = only the latest one)? Recency's tight phrases win on overlap so
/// "what did we talk about last" keeps its existing path. Without this, a broad summary question
/// falls to semantic top-k over ~2s fragments and answers with disconnected snippet garbage.
pub(crate) fn is_window_summary_query(query: &str) -> bool {
    if is_recency_query(query) {
        return false;
    }
    let q = query.to_lowercase();
    [
        "what have we spoken about",
        "what did we speak about",
        "what have we talked about",
        "what did we talk about",
        "what have we discussed",
        "what did we discuss",
        "what was discussed",
        "what was talked about",
        "what was spoken about",
        "summarize today",
        "summarize yesterday",
        "summarize this morning",
        "summarize this afternoon",
        "summarize the day",
        "summary of today",
        "summary of the day",
        "what conversations",
        "which conversations",
    ]
    .iter()
    .any(|p| q.contains(p))
}

/// Is the question a PROFILE ask about a specific known person/voice ("tell me about Casey",
/// "what do you know about Bob", "who is Judith")? Phrase-only — the CALLER must also resolve a
/// catalog name in the text before acting (so "tell me about our last conversation" never lands
/// here; recency and roster pre-routes are checked first anyway). Surfaces the accumulated
/// entity profile ("running memory") alongside normal retrieval.
pub(crate) fn is_profile_query(query: &str) -> bool {
    let q = query.to_lowercase();
    [
        "tell me about",
        "what do you know about",
        "what do we know about",
        "who is ",
        "who's ",
        "describe ",
        "give me a rundown on",
        "what's the story with",
    ]
    .iter()
    .any(|p| q.contains(p))
}

/// Count of registered cameras (devices). Used to decide whether "this video" is ambiguous: with a
/// single camera there's nothing to clarify. Cheap; the catalog is tiny.
pub(crate) async fn camera_count(pool: &PgPool) -> anyhow::Result<i64> {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM devices")
        .fetch_one(pool)
        .await?;
    Ok(n)
}

/// Route a People-agent question to its sightings, in precedence order (the single source of truth
/// for both the query and chat paths):
///   1. explicit person filter (id/name) → that person's exhaustive sightings;
///   2. a catalog name mentioned in the free text → that person's sightings;
///   3. first-person "who was I with" → co-occurrence around the owner (or `NeedsOwner` if unset);
///   4. otherwise → the ROSTER of everyone seen ("who have you seen so far").
///
/// (4) is the key fix: a no-name question used to fall straight into (3) and decline when no owner
/// was configured, even though "who have you seen" needs no owner at all.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_people_sources(
    st: &AppState,
    query: &str,
    person_id: Option<Vec<String>>,
    person_name: Option<String>,
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
    limit: i64,
) -> anyhow::Result<PeopleSources> {
    if let Some(ids) = resolve_person_filter(&st.pool, person_id, person_name).await? {
        // Targeted "when did I see X". Unknown name -> empty ids -> empty sightings -> the LLM declines.
        return Ok(PeopleSources::Found(
            retrieve::list_by_person(&st.pool, &ids, device_id, after, before, limit).await?,
        ));
    }
    let mentioned = crate::persons::resolve_names_in_text(&st.pool, query).await?;
    // "Was I with <name>?" — a co-occurrence question that NAMES someone → co-presence intersection
    // (segments where the owner was ALSO present), not that person's solo sightings (which would
    // imply togetherness that may not have happened). Needs the owner; declines if unset.
    if is_co_occurrence_query(query) && !mentioned.is_empty() {
        let owner = resolve_owner_person(st).await?;
        if owner.is_empty() {
            return Ok(PeopleSources::NeedsOwner);
        }
        let others: Vec<String> = mentioned.iter().map(|u| u.to_string()).collect();
        return Ok(PeopleSources::Found(
            retrieve::list_co_presence_pair(&st.pool, &owner, &others, device_id, after, before, limit)
                .await?,
        ));
    }
    if !mentioned.is_empty() {
        let ids: Vec<String> = mentioned.iter().map(|u| u.to_string()).collect();
        return Ok(PeopleSources::Found(
            retrieve::list_by_person(&st.pool, &ids, device_id, after, before, limit).await?,
        ));
    }
    if is_co_occurrence_query(query) {
        let owner = resolve_owner_person(st).await?;
        if owner.is_empty() {
            return Ok(PeopleSources::NeedsOwner);
        }
        return Ok(PeopleSources::Found(
            retrieve::list_co_occurring_persons(&st.pool, &owner, device_id, after, before, limit)
                .await?,
        ));
    }
    // Roster: "who have you seen (so far)" — no name, not first-person → everyone seen, recent first.
    Ok(PeopleSources::Found(
        retrieve::list_recent_persons(&st.pool, device_id, after, before, limit).await?,
    ))
}

/// Resolve a person filter with the same strict precedence as `resolve_speaker_filter`:
/// explicit `person_id` wins; else `person_name` -> ids (unknown -> `Some(vec![])` matches nothing);
/// else `None` (no explicit person — the caller falls back to free-text names / co-occurrence).
pub(crate) async fn resolve_person_filter(
    pool: &PgPool,
    person_id: Option<Vec<String>>,
    person_name: Option<String>,
) -> anyhow::Result<Option<Vec<String>>> {
    match (person_id, person_name) {
        (Some(ids), _) => Ok(Some(ids)),
        (None, Some(name)) => {
            let uuids = crate::persons::resolve_name(pool, &name).await?;
            Ok(Some(uuids.iter().map(|u| u.to_string()).collect()))
        }
        (None, None) => Ok(None),
    }
}

/// Single-shot license-PLATE answer (the "when did I see a car with plate ABC123" path). Routing:
///   - an explicit plate filter (id/text) OR a plate-shaped token found in the free-text query →
///     exhaustive per-plate sightings (`list_by_plate`);
///   - otherwise → no sightings (the LLM declines): unlike `people`, plates have no "who was I with"
///     owner anchor — a plate's identity is its string, so without one there's nothing to list.
async fn plates_query(
    st: &AppState,
    req: &QueryRequest,
) -> Result<Json<QueryResponse>, (StatusCode, String)> {
    let qf = req.filters.as_ref();
    let device_id = qf.and_then(|f| f.device_id.clone());
    let after = qf.and_then(|f| f.after_unix_nanos);
    let before = qf.and_then(|f| f.before_unix_nanos);
    let limit = req
        .top_k
        .unwrap_or(st.cfg.plate_top_k_default)
        .clamp(1, 200);

    let explicit = resolve_plate_filter(
        &st.pool,
        qf.and_then(|f| f.plate_id.clone()),
        qf.and_then(|f| f.plate_text.clone()),
    )
    .await
    .map_err(internal)?;

    let mut sources = match explicit {
        Some(ids) => {
            // Targeted "when did I see plate X". Unknown plate -> empty ids -> empty sightings ->
            // the LLM declines.
            retrieve::list_by_plate(&st.pool, &ids, device_id.as_deref(), after, before, limit)
                .await
                .map_err(internal)?
        }
        None => {
            // No explicit filter: resolve plate-shaped tokens mentioned in the free-text query.
            let mentioned = crate::plates::resolve_plates_in_text(&st.pool, &req.query)
                .await
                .map_err(internal)?;
            if mentioned.is_empty() {
                Vec::new()
            } else {
                let ids: Vec<String> = mentioned.iter().map(|u| u.to_string()).collect();
                retrieve::list_by_plate(&st.pool, &ids, device_id.as_deref(), after, before, limit)
                    .await
                    .map_err(internal)?
            }
        }
    };

    // Plate attribution display: resolve plate labels, humanize the sighting time.
    let ids: Vec<String> = sources
        .iter()
        .filter_map(|s| s.speaker_id.clone())
        .collect();
    let names = crate::plates::label_map(&st.pool, &ids)
        .await
        .map_err(internal)?;
    enrich_plates_for_display(
        &mut sources,
        &names,
        Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX),
        req.tz_offset(&st),
    );

    let answer = st
        .llm
        .answer_plates(&req.query, &sources, &names)
        .await
        .map_err(internal)?;
    Ok(Json(QueryResponse { answer, sources }))
}

/// Plate analogue of `retrieve::enrich_for_display`: set `speaker_name` via the PLATE label rules
/// (`plates::display_label` — "plate ABC123" / "Mom's car" / "an unreadable plate") and the
/// humanized `time_label`. (A plate row carries `plate_id::text` in `speaker_id`; a label not in the
/// map falls back to "an unreadable plate".)
pub(crate) fn enrich_plates_for_display(
    sources: &mut [Source],
    names: &std::collections::HashMap<String, String>,
    now_unix_nanos: i64,
    tz_offset_secs: i64,
) {
    for s in sources.iter_mut() {
        let label = s
            .speaker_id
            .as_deref()
            .and_then(|id| names.get(id).cloned())
            .unwrap_or_else(|| crate::plates::UNREADABLE_PLATE.to_string());
        s.speaker_name = Some(label);
        s.time_label =
            crate::humanize::humanize_time(s.start_unix_nanos, now_unix_nanos, tz_offset_secs);
    }
}

/// Resolve a plate filter with the same strict precedence as `resolve_person_filter`: explicit
/// `plate_id` wins; else `plate_text` -> ids via normalized exact+fuzzy match (unknown ->
/// `Some(vec![])` matches nothing); else `None` (no explicit plate — the caller falls back to
/// plate-shaped tokens in the free-text query).
pub(crate) async fn resolve_plate_filter(
    pool: &PgPool,
    plate_id: Option<Vec<String>>,
    plate_text: Option<String>,
) -> anyhow::Result<Option<Vec<String>>> {
    match (plate_id, plate_text) {
        (Some(ids), _) => Ok(Some(ids)),
        (None, Some(text)) => {
            let uuids = crate::plates::resolve_plate_text(pool, &text).await?;
            Ok(Some(uuids.iter().map(|u| u.to_string()).collect()))
        }
        (None, None) => Ok(None),
    }
}

/// Resolve the owner's person id(s) for "who was I with". Precedence: the DB owner mark
/// (0023, a "This is me" tap in the People UI — most recent user intent, works without env
/// config) wins over `OWNER_PERSON_ID`, which wins over `OWNER_PERSON_NAME` (resolved
/// against the catalog). Empty = the caller declines gracefully.
pub(crate) async fn resolve_owner_person(st: &AppState) -> anyhow::Result<Vec<String>> {
    if let Some((id, _)) = crate::persons::owner(&st.pool).await? {
        return Ok(vec![id.to_string()]);
    }
    if let Some(id) = &st.cfg.owner_person_id {
        return Ok(vec![id.clone()]);
    }
    if let Some(name) = &st.cfg.owner_person_name {
        let uuids = crate::persons::resolve_name(&st.pool, name).await?;
        return Ok(uuids.iter().map(|u| u.to_string()).collect());
    }
    Ok(vec![])
}

#[derive(Debug, Deserialize)]
pub struct TtsRequest {
    pub text: String,
}

/// `POST /v1/tts` — synthesize `text` to speech and return a 16-bit PCM WAV.
///
/// Kept separate from `/v1/rag/query` so the answer text can render immediately
/// while audio is fetched/played, and so TTS is independently testable. Returns
/// 503 when TTS isn't enabled/loaded; the client then just shows the text.
pub async fn tts_synthesize(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TtsRequest>,
) -> Result<Response, (StatusCode, String)> {
    check_auth(&headers, &st)?;

    let text = req.text.trim();
    if text.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "text must not be empty".into()));
    }
    if text.chars().count() > MAX_TTS_CHARS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("text too long (max {MAX_TTS_CHARS} chars)"),
        ));
    }

    let tts = st.tts.clone().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "TTS is not enabled on this server".to_string(),
    ))?;

    // Synthesis is CPU-bound/blocking — keep it off the async runtime threads.
    let text = text.to_string();
    let wav = tokio::task::spawn_blocking(move || tts.synthesize_wav(&text))
        .await
        .map_err(|e| internal(anyhow::anyhow!("TTS task join error: {e}")))?
        .map_err(internal)?;

    Ok(([(header::CONTENT_TYPE, "audio/wav")], wav).into_response())
}

/// Resolve a speaker filter with strict precedence (the pinned failure-mode contract,
/// shared by `/query` and `/chat` so the behaviour can never diverge):
///   - explicit `speaker_id` wins (name ignored);
///   - else `speaker_name` -> ids: unknown -> `Some(vec![])` (matches nothing, NOT
///     unfiltered, NOT an error); ambiguous -> union of all matching ids;
///   - else `None` (no speaker filter).
pub(crate) async fn resolve_speaker_filter(
    pool: &PgPool,
    speaker_id: Option<Vec<String>>,
    speaker_name: Option<String>,
) -> anyhow::Result<Option<Vec<String>>> {
    match (speaker_id, speaker_name) {
        (Some(ids), _) => Ok(Some(ids)),
        (None, Some(name)) => {
            let uuids = crate::speakers::resolve_name(pool, &name).await?;
            Ok(Some(uuids.iter().map(|u| u.to_string()).collect()))
        }
        (None, None) => Ok(None),
    }
}

/// Resolve the reflection target speaker. Precedence: an explicit request speaker (id or
/// name) wins and SHORT-CIRCUITS — it never silently becomes the owner, so an unknown
/// requested name resolves to empty (→ a graceful "no data" decline) rather than analyzing
/// someone else. With no request speaker, fall back to the OWNER: the DB owner mark (0023,
/// "This is me") wins over `OWNER_SPEAKER_ID`, which wins over `OWNER_SPEAKER_NAME`.
/// Empty result = the caller declines.
pub(crate) async fn resolve_target_speaker(
    st: &AppState,
    speaker_id: Option<Vec<String>>,
    speaker_name: Option<String>,
) -> anyhow::Result<Vec<String>> {
    // An explicit speaker_id SHORT-CIRCUITS — even an empty list (a request that scoped to
    // "this speaker" and resolved to nobody must decline, NOT silently analyze the owner).
    // Mirrors `resolve_speaker_filter`'s `Some(_) => Ok(Some(ids))`.
    if let Some(ids) = speaker_id {
        return Ok(ids);
    }
    if let Some(name) = speaker_name {
        let uuids = crate::speakers::resolve_name(&st.pool, &name).await?;
        return Ok(uuids.iter().map(|u| u.to_string()).collect());
    }
    if let Some((id, _)) = crate::speakers::owner(&st.pool).await? {
        return Ok(vec![id.to_string()]);
    }
    if let Some(id) = &st.cfg.owner_speaker_id {
        return Ok(vec![id.clone()]);
    }
    if let Some(name) = &st.cfg.owner_speaker_name {
        let uuids = crate::speakers::resolve_name(&st.pool, name).await?;
        return Ok(uuids.iter().map(|u| u.to_string()).collect());
    }
    Ok(vec![])
}

/// The owner's human display name, for caller-identity answers and the owner prompt line.
/// Precedence mirrors `resolve_target_speaker`'s owner chain (so "what's my name" and reflection
/// agree on WHO the owner is), then falls through to the person (face) owner as a last resort (a
/// rig may have named only the face): DB speaker owner name → CURRENT DB name of env
/// `OWNER_SPEAKER_ID` → env `OWNER_SPEAKER_NAME` → DB person owner name → env `OWNER_PERSON_NAME`.
/// `OWNER_SPEAKER_ID` (a stable id) is resolved to its live DB name BEFORE the env NAME literal, so
/// renaming that voice reflects immediately (the env NAME is only a last-resort literal, used when
/// no id/DB owner links to a current name). `None` = no owner configured anywhere.
pub(crate) async fn resolve_owner_name(st: &AppState) -> anyhow::Result<Option<String>> {
    if let Some((_, Some(name))) = crate::speakers::owner(&st.pool).await? {
        return Ok(Some(name));
    }
    if let Some(id) = &st.cfg.owner_speaker_id {
        let names = crate::speakers::name_map(&st.pool, std::slice::from_ref(id)).await?;
        if let Some(name) = names.get(id) {
            return Ok(Some(name.clone()));
        }
    }
    if let Some(name) = &st.cfg.owner_speaker_name {
        return Ok(Some(name.clone()));
    }
    if let Some((_, Some(name))) = crate::persons::owner(&st.pool).await? {
        return Ok(Some(name));
    }
    if let Some(name) = &st.cfg.owner_person_name {
        return Ok(Some(name.clone()));
    }
    Ok(None)
}

/// Optional bearer auth, enforced only when `RAG_TOKEN` is configured.
pub(crate) fn check_auth(headers: &HeaderMap, st: &AppState) -> Result<(), (StatusCode, String)> {
    if let Some(expected) = &st.cfg.rag_token {
        let presented = headers
            .get(AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        // Constant-time compare so the static RAG_TOKEN isn't recoverable byte-by-byte via
        // a timing side channel — matches the backend's `subtle`-based auth (auth.rs:69).
        // The length check isn't itself secret; ct_eq is constant-time for equal lengths.
        let ok = match presented {
            Some(tok) => {
                tok.len() == expected.len()
                    && bool::from(tok.as_bytes().ct_eq(expected.as_bytes()))
            }
            None => false,
        };
        if !ok {
            return Err((
                StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn internal(e: anyhow::Error) -> (StatusCode, String) {
    // Log the full chain server-side, but return a static body — `{e:#}` leaks sqlx
    // table/column names, SQL fragments, Ollama URLs and filesystem paths to any caller.
    tracing::error!(error = format!("{e:#}"), "rag request failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal error".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        IDENTITY_NO_OWNER, is_co_occurrence_query, is_deictic_video_query, is_footage_stats_query,
        is_identity_query, is_people_count_query, is_recency_query, is_speaker_roster_query,
        is_window_summary_query, normalize_object_label, render_identity,
    };

    #[test]
    fn object_label_resolves_plurals_and_irregulars() {
        let n = |q: &str| normalize_object_label(q);
        // regular plurals + articles + punctuation
        assert_eq!(n("cars").as_deref(), Some("car"));
        assert_eq!(n("a dog").as_deref(), Some("dog"));
        assert_eq!(n("the laptops?").as_deref(), Some("laptop"));
        // irregular plurals / compounds the OLD naive single-'s' strip mangled
        assert_eq!(n("people").as_deref(), Some("person"));
        assert_eq!(n("buses").as_deref(), Some("bus"));
        assert_eq!(n("knives").as_deref(), Some("knife"));
        assert_eq!(n("wine glasses").as_deref(), Some("wine glass"));
        assert_eq!(n("phones").as_deref(), Some("cell phone"));
        // singular labels that END in 's' must NOT be truncated
        assert_eq!(n("scissors").as_deref(), Some("scissors"));
        assert_eq!(n("skis").as_deref(), Some("skis"));
        // non-COCO phrases return None → caller falls back to the semantic path (not empty results)
        assert_eq!(n("a spaceship"), None);
        assert_eq!(n("people walking around"), None);
    }

    #[test]
    fn identity_questions_are_detected() {
        for q in [
            "What's my name?",
            "whats my name",
            "What is my name",
            "Who am I?",
            "do you know my name",
            "Do you know who I am?",
            "tell me my name",
            "what am I called",
        ] {
            assert!(is_identity_query(q), "should be identity: {q:?}");
        }
    }

    #[test]
    fn content_questions_are_not_identity() {
        // A content question that merely mentions "name" must NOT trip the identity path.
        for q in [
            "what did I say about my name change",
            "who is Bob",
            "what's the name of that restaurant",
            "did anyone say my name yesterday",
            "what did we last discuss",
        ] {
            assert!(!is_identity_query(q), "should not be identity: {q:?}");
        }
    }

    #[test]
    fn identity_answer_depends_on_verification_and_owner() {
        // Verified + known name -> greeted by name.
        assert_eq!(render_identity(Some("Morgan"), true), "Your name is Morgan.");
        // Known name but unverified -> non-confirming reply (no false "you are X").
        let unverified = render_identity(Some("Morgan"), false);
        assert!(unverified.contains("Morgan"));
        assert!(unverified.contains("can't confirm"));
        // No owner configured -> the setup hint, regardless of verification.
        assert_eq!(render_identity(None, true), IDENTITY_NO_OWNER);
        assert_eq!(render_identity(None, false), IDENTITY_NO_OWNER);
    }

    #[test]
    fn recency_questions_are_detected() {
        for q in [
            "What did we last discuss?",
            "what did we last talk about",
            "what were we talking about",
            "tell me about our last conversation",
            "what was the most recent conversation about",
            "what did we just discuss",
        ] {
            assert!(is_recency_query(q), "should be recency: {q:?}");
        }
    }

    #[test]
    fn non_recency_questions_are_not_recency() {
        // "last" appears but the question is a People/count or roster ask, not a recency summary.
        for q in [
            "when did I last see Bob",
            "who spoke last",
            "what did I say about the invoice",
            "who was speaking",
            "how many cars did I see",
        ] {
            assert!(!is_recency_query(q), "should not be recency: {q:?}");
        }
    }

    #[test]
    fn speaker_roster_questions_are_detected() {
        for q in [
            "Who was speaking in this video clip",
            "who is talking right now?",
            "Who's speaking?",
            "who spoke in this clip",
            "whose voice is that",
            "who can you hear in this recording",
        ] {
            assert!(is_speaker_roster_query(q), "should be roster: {q:?}");
        }
    }

    #[test]
    fn content_attribution_is_not_a_roster_question() {
        // "Who said X" is content attribution — the semantic path already handles it; a roster
        // rewrite would answer the wrong question ("who spoke at all" vs "who said THIS").
        for q in [
            "who said we should buy the house",
            "who mentioned the invoice",
            "what did Morgan say yesterday",
            "who did I see in this video",
        ] {
            assert!(!is_speaker_roster_query(q), "should not be roster: {q:?}");
        }
    }

    #[test]
    fn deictic_video_questions_are_detected() {
        for q in [
            "who did we see in this video",
            "who is in this clip?",
            "what's on screen right now",
            "anyone in this camera?",
            "what do you see in the video",
            "who is in the clip currently playing",
        ] {
            assert!(is_deictic_video_query(q), "should be deictic: {q:?}");
        }
    }

    #[test]
    fn archive_wide_questions_are_not_deictic() {
        // These ask across the whole archive, not a specific open video → must NOT clarify.
        for q in [
            "who have you seen so far?",
            "when did I see a car",
            "what did I talk about yesterday",
            "how have I been lately",
            "did you see plate ABC123",
        ] {
            assert!(!is_deictic_video_query(q), "should not be deictic: {q:?}");
        }
    }

    #[test]
    fn roster_questions_are_not_co_occurrence() {
        // "Who have you seen so far?" and friends must NOT route to the owner-anchored path —
        // this is the bug: they used to fall through to co-occurrence and decline with no owner.
        for q in [
            "Who have you seen so far?",
            "who have you seen",
            "Who's been around?",
            "who did you see today",
            "list everyone you've seen",
            "people you have seen",
        ] {
            assert!(!is_co_occurrence_query(q), "should be a roster question: {q:?}");
        }
    }

    #[test]
    fn first_person_with_questions_are_co_occurrence() {
        for q in [
            "Who was I with yesterday?",
            "who was around me",
            "who was near me at lunch",
            "show me who has been with me",
            "with whom did I meet",
            "who accompanied me",
        ] {
            assert!(is_co_occurrence_query(q), "should be co-occurrence: {q:?}");
        }
    }

    #[test]
    fn people_count_questions_are_detected() {
        for q in [
            "How many people have we seen in the last 10 minutes?",
            "how many people did you see today",
            "how many different faces were on camera",
            "how many visitors came by",
            "number of people seen this morning",
        ] {
            assert!(is_people_count_query(q), "should be people-count: {q:?}");
        }
    }

    #[test]
    fn single_subject_frequency_is_not_a_people_count() {
        for q in [
            "how many times did I see Bob",
            "how often does the mail person come",
            "number of times that person was here",
            "who have you seen so far",
            "how many cars were on camera",
        ] {
            assert!(!is_people_count_query(q), "should not be people-count: {q:?}");
        }
    }

    #[test]
    fn footage_stats_questions_are_detected() {
        for q in [
            "how many min of vid do we have today?",
            "How many minutes of video do we have from today?",
            "how much footage was recorded yesterday",
            "how much audio do you have",
            "total hours of video this week",
            "how many recordings do we have",
        ] {
            assert!(is_footage_stats_query(q), "should be footage-stats: {q:?}");
        }
    }

    #[test]
    fn content_questions_are_not_footage_stats() {
        for q in [
            "what do the recordings say about money",
            "how many times did the video show a car",
            "how many people were on the recording", // people-count wins over the footage noun
            "what was in the video today",
            "who was speaking in this clip",
            // A footage word elsewhere in a CONTENT question must not trip the stats path
            // (found live: this answered "No footage was recorded for that period").
            "How many packages arrived this week, according to the recordings?",
            "how many deliveries were mentioned in the video",
        ] {
            assert!(!is_footage_stats_query(q), "should not be footage-stats: {q:?}");
        }
    }

    #[test]
    fn window_summary_questions_are_detected() {
        for q in [
            "What have we spoken about today?",
            "what did we talk about this morning",
            "what was discussed yesterday",
            "summarize today",
            "what conversations happened today",
            "What did we discuss today?",
        ] {
            assert!(is_window_summary_query(q), "should be window-summary: {q:?}");
        }
    }

    #[test]
    fn recency_and_content_questions_are_not_window_summary() {
        for q in [
            "what did we last discuss",          // recency wins
            "what did we just discuss",          // recency wins
            "what were we talking about",        // recency wins
            "when did I last see Bob",           // people/count
            "what do the recordings say about money",
        ] {
            assert!(!is_window_summary_query(q), "should not be window-summary: {q:?}");
        }
    }
}
