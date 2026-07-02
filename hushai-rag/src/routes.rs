//! HTTP surface: `POST /v1/rag/query` (embed -> retrieve -> ground -> answer).

use axum::Json;
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use sqlx::PgPool;

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
    let answer = st
        .llm
        .answer(&req.query, &sources, &names)
        .await
        .map_err(internal)?;

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

/// Resolve the owner's person id(s) for "who was I with" — `OWNER_PERSON_ID` wins over
/// `OWNER_PERSON_NAME` (resolved against the catalog). Empty = the caller declines gracefully.
pub(crate) async fn resolve_owner_person(st: &AppState) -> anyhow::Result<Vec<String>> {
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
/// someone else. With no request speaker, fall back to the configured OWNER
/// (`OWNER_SPEAKER_ID` wins over `OWNER_SPEAKER_NAME`). Empty result = the caller declines.
pub(crate) async fn resolve_target_speaker(
    st: &AppState,
    speaker_id: Option<Vec<String>>,
    speaker_name: Option<String>,
) -> anyhow::Result<Vec<String>> {
    if let Some(ids) = speaker_id {
        if !ids.is_empty() {
            return Ok(ids);
        }
    }
    if let Some(name) = speaker_name {
        let uuids = crate::speakers::resolve_name(&st.pool, &name).await?;
        return Ok(uuids.iter().map(|u| u.to_string()).collect());
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
        is_co_occurrence_query, is_deictic_video_query, is_speaker_roster_query,
        normalize_object_label,
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
            "what did Mendel say yesterday",
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
}
