//! HTTP surface: `POST /v1/rag/query` (embed -> retrieve -> ground -> answer).

use axum::Json;
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::{Deserialize, Serialize};

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

    let top_k = req.top_k.unwrap_or(st.cfg.top_k_default).clamp(1, 50);
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
        st.cfg.analysis_tz_offset_secs,
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

    // Exhaustive: list every sighting of the exact COCO class (no recall cliff). Otherwise the
    // open-vocab semantic path (CLIP text NN over scene_objects, including the whole-frame rows).
    let mut sources = if req.exhaustive.unwrap_or(false) {
        let label = normalize_object_label(&req.query);
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
    for s in &mut sources {
        s.time_label =
            crate::humanize::humanize_time(s.start_unix_nanos, now, st.cfg.analysis_tz_offset_secs);
    }

    let answer = st
        .llm
        .answer_objects(&req.query, &sources)
        .await
        .map_err(internal)?;
    Ok(Json(QueryResponse { answer, sources }))
}

/// Reduce a natural phrase to a bare object class for the exact-class exhaustive path: lowercase,
/// strip trailing punctuation + a leading article, and naively singularize ("cars" -> "car").
fn normalize_object_label(query: &str) -> String {
    let lower = query
        .trim()
        .trim_end_matches(['?', '.', '!', ','])
        .to_lowercase();
    let lower = lower.trim();
    let stripped = lower
        .strip_prefix("a ")
        .or_else(|| lower.strip_prefix("an "))
        .or_else(|| lower.strip_prefix("the "))
        .unwrap_or(lower)
        .trim();
    if stripped.len() > 1 && stripped.ends_with('s') {
        stripped[..stripped.len() - 1].to_string()
    } else {
        stripped.to_string()
    }
}

/// Shown when "who was I with" can't resolve the owner (no request person, no configured owner).
pub(crate) const PEOPLE_NO_OWNER: &str = "I'm not sure which face is yours yet. Open the People screen, name your own face, and set \
     OWNER_PERSON_NAME so I can tell who you were with — then ask me again.";

/// Single-shot PERSON answer (the "when did I see Bob" / "who was I with" path). Routing:
///   - an explicit person filter (id/name) OR a catalog name mentioned in the query → exhaustive
///     per-person sightings (`list_by_person`);
///   - otherwise → "who was I with": same-segment co-occurrence around the configured owner.
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

    let explicit = resolve_person_filter(
        &st.pool,
        qf.and_then(|f| f.person_id.clone()),
        qf.and_then(|f| f.person_name.clone()),
    )
    .await
    .map_err(internal)?;

    let mut sources = match explicit {
        Some(ids) => {
            // Targeted "when did I see X". Unknown name -> empty ids -> empty sightings -> decline.
            retrieve::list_by_person(&st.pool, &ids, device_id.as_deref(), after, before, limit)
                .await
                .map_err(internal)?
        }
        None => {
            // No explicit filter: try names mentioned in the free-text query first.
            let mentioned = crate::persons::resolve_names_in_text(&st.pool, &req.query)
                .await
                .map_err(internal)?;
            if !mentioned.is_empty() {
                let ids: Vec<String> = mentioned.iter().map(|u| u.to_string()).collect();
                retrieve::list_by_person(&st.pool, &ids, device_id.as_deref(), after, before, limit)
                    .await
                    .map_err(internal)?
            } else {
                // "Who was I with": co-occurrence around the configured owner.
                let owner = resolve_owner_person(st).await.map_err(internal)?;
                if owner.is_empty() {
                    return Ok(Json(QueryResponse {
                        answer: PEOPLE_NO_OWNER.to_string(),
                        sources: vec![],
                    }));
                }
                retrieve::list_co_occurring_persons(
                    &st.pool,
                    &owner,
                    device_id.as_deref(),
                    after,
                    before,
                    limit,
                )
                .await
                .map_err(internal)?
            }
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
        st.cfg.analysis_tz_offset_secs,
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
        if presented != Some(expected.as_str()) {
            return Err((
                StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn internal(e: anyhow::Error) -> (StatusCode, String) {
    tracing::error!(error = format!("{e:#}"), "rag request failed");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}
