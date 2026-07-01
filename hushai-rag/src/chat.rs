//! Multi-turn chat over recordings: `POST /v1/rag/chat` (SSE token streaming) plus the
//! session/transcript read endpoints that let the webapp restore a conversation on reload.
//!
//! Design notes:
//!   - DB-backed sessions (`chat_sessions` + `chat_messages`): conversations and their
//!     citations survive reloads, and are the foundation for persistent per-agent windows.
//!   - Retrieval is re-anchored on the LATEST user message every turn (embed it, reuse the
//!     exact `retrieve::nearest` pipeline as `/v1/rag/query`). Prior turns are given to the
//!     LLM only as chat history for coreference — NOT folded into the retrieval query.
//!     Consequence (deliberate RAG-chat tradeoff): citations are correct for the current
//!     turn; the model sees prior *answers* but only this turn's freshly-retrieved sources.
//!     A future query-condensation step (gated by a config flag) would slot in at the
//!     "embed the message" line below without restructuring anything else.
//!   - SSE event order: `session` (so the client learns an auto-created id), then `sources`
//!     (retrieval finishes before generation, so citation chips render first), then a run of
//!     `token` deltas, then `done` (assistant turn persisted) — or `error`.

use std::convert::Infallible;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Sse;
use axum::response::sse::{Event, KeepAlive};
use chrono::{DateTime, Utc};
use futures_util::{Stream, StreamExt};
use rig::completion::Message;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::agents::AgentKind;
use crate::retrieve::{self, Filters, Source, Tuning};
use crate::routes::{
    PEOPLE_NO_OWNER, PeopleSources, QueryFilters, REFLECTION_NO_TARGET, check_auth,
    enrich_persons_for_display, enrich_plates_for_display, internal, resolve_people_sources,
    resolve_plate_filter, resolve_speaker_filter, resolve_target_speaker,
};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// Continue this conversation. When absent, a new session is created and its id is
    /// returned in the first SSE `session` event.
    #[serde(default)]
    pub session_id: Option<Uuid>,
    /// Only honoured when creating a new session (binding is immutable afterwards).
    #[serde(default)]
    pub agent_id: Option<String>,
    pub message: String,
    /// Per-request retrieval overrides; merged over the agent's default scope (request wins).
    #[serde(default)]
    pub filters: Option<QueryFilters>,
    #[serde(default)]
    pub top_k: Option<i64>,
    /// Caller's UTC offset in seconds (e.g. -14400 for EDT) for rendering "today/yesterday at
    /// h:MM PM". The browser sends its live offset so spoken times match the user's local clock;
    /// absent (e.g. non-browser callers) falls back to `ANALYSIS_TZ_OFFSET_SECS`.
    #[serde(default)]
    pub tz_offset_secs: Option<i64>,
}

/// `POST /v1/rag/chat` — stream a grounded, multi-turn answer as Server-Sent Events.
pub async fn rag_chat(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, String)> {
    check_auth(&headers, &st)?;
    hushai_backend::observe::counter("hushai_rag_requests_total", &[("endpoint", "chat")]);

    let message = req.message.trim().to_string();
    if message.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "message must not be empty".into()));
    }
    if message.chars().count() > st.cfg.chat_max_message_chars {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "message too long (max {} chars)",
                st.cfg.chat_max_message_chars
            ),
        ));
    }

    // Resolve the session + its immutable agent binding (existing) or create one (new).
    let (session_id, agent_id) = match req.session_id {
        Some(sid) => {
            let aid = session_agent(&st.pool, sid)
                .await
                .map_err(internal)?
                .ok_or((StatusCode::NOT_FOUND, "chat session not found".to_string()))?;
            (sid, aid)
        }
        None => {
            let aid = req
                .agent_id
                .as_deref()
                .and_then(crate::agents::get)
                .unwrap_or_else(crate::agents::default)
                .id
                .to_string();
            let sid = create_session(&st.pool, &aid, &message)
                .await
                .map_err(internal)?;
            (sid, aid)
        }
    };
    let agent = crate::agents::get(&agent_id).unwrap_or_else(crate::agents::default);

    // Load prior turns BEFORE persisting the new user message (so the just-sent message
    // isn't double-counted as history). Trailing window bounds the LLM context.
    let history_rows = load_history(&st.pool, session_id, st.cfg.chat_history_turns * 2)
        .await
        .map_err(internal)?;
    // The last turn or two as plain text, for the auto-router (so a follow-up like "Mendel" after
    // a clarifying question routes with context).
    let recent_context: String = history_rows
        .iter()
        .rev()
        .take(2)
        .rev()
        .map(|(role, content)| format!("{role}: {content}"))
        .collect::<Vec<_>>()
        .join("\n");
    let history: Vec<Message> = history_rows
        .into_iter()
        .map(|(role, content)| {
            if role == "assistant" {
                Message::assistant(content)
            } else {
                Message::user(content)
            }
        })
        .collect();

    // Unified assistant: classify each message and dispatch to the right capability. A session
    // bound to a concrete agent keeps that agent (manual override / older sessions).
    let agent = if agent.id == crate::agents::AUTO_AGENT_ID {
        let routed = st
            .llm
            .classify_agent(&message, &recent_context)
            .await
            .map_err(internal)?;
        tracing::info!(routed_to = %routed, "auto-router selected capability");
        crate::agents::get(routed).unwrap_or_else(crate::agents::default)
    } else {
        agent
    };

    // Persist the user turn immediately: a crash mid-answer still records it, and seq stays
    // gap-free.
    insert_message(&st.pool, session_id, "user", &message, None, &agent_id)
        .await
        .map_err(internal)?;

    // Merge agent default scope under per-request filters (request wins per field), then
    // build this turn's context: a grounded retrieval OR (reflection) an analytics digest.
    let qf = req.filters.unwrap_or_default();
    let df = &agent.default_filters;
    // Render times in the caller's local civil time (browser offset), falling back to the env default.
    let tz = req.tz_offset_secs.unwrap_or(st.cfg.analysis_tz_offset_secs);

    // For reflection-with-no-target we skip the LLM entirely and stream a setup hint.
    let mut precomputed_answer: Option<String> = None;
    let mut reflection_digest: Option<String> = None;
    let mut sources: Vec<Source>;
    let names;

    // "This video" with no camera scoped + more than one camera -> ask which one instead of
    // silently answering across everything. Reflection isn't camera-scoped, so it never triggers.
    let scope_is_all = qf.device_id.is_none() && df.device_id.is_none();
    let clarify_camera = scope_is_all
        && matches!(
            agent.kind,
            AgentKind::People | AgentKind::Objects | AgentKind::Plates | AgentKind::Grounded
        )
        && crate::routes::is_deictic_video_query(&message)
        && crate::routes::camera_count(&st.pool).await.map_err(internal)? > 1;

    if clarify_camera {
        precomputed_answer = Some(crate::routes::CAMERA_CLARIFY.to_string());
        sources = vec![];
        names = std::collections::HashMap::new();
    } else {
    match agent.kind {
        AgentKind::Reflection => {
            let target = resolve_target_speaker(
                &st,
                qf.speaker_id.clone(),
                qf.speaker_name.clone().or_else(|| df.speaker_name.clone()),
            )
            .await
            .map_err(internal)?;
            if target.is_empty() {
                precomputed_answer = Some(REFLECTION_NO_TARGET.to_string());
                sources = vec![];
                names = std::collections::HashMap::new();
            } else {
                let days = agent
                    .default_window_days
                    .unwrap_or(st.cfg.analysis_window_days_default);
                let window = crate::analytics::AnalysisWindow::resolve(
                    qf.after_unix_nanos.or(df.after_unix_nanos),
                    qf.before_unix_nanos.or(df.before_unix_nanos),
                    days,
                );
                let dcfg = crate::analytics::DigestConfig::from_rag(&st.cfg);
                // Embed the message so the digest can include on-topic excerpts.
                let embedding = st.embedder.embed_one(&message).await.map_err(internal)?;
                let digest = crate::analytics::compute_digest(
                    &st.pool,
                    &target,
                    window,
                    &dcfg,
                    Some(&embedding),
                )
                .await
                .map_err(internal)?;
                reflection_digest = Some(crate::analytics::render_digest(&digest));
                sources = digest.excerpts;
                let ids: Vec<String> = sources
                    .iter()
                    .filter_map(|s| s.speaker_id.clone())
                    .collect();
                names = crate::speakers::name_map(&st.pool, &ids)
                    .await
                    .map_err(internal)?;
            }
        }
        AgentKind::Grounded => {
            let device_id = qf.device_id.or_else(|| df.device_id.clone());
            let after = qf.after_unix_nanos.or(df.after_unix_nanos);
            let before = qf.before_unix_nanos.or(df.before_unix_nanos);
            let speaker_name = qf.speaker_name.or_else(|| df.speaker_name.clone());
            let speaker_id = resolve_speaker_filter(&st.pool, qf.speaker_id, speaker_name)
                .await
                .map_err(internal)?;
            let top_k = req
                .top_k
                .or(agent.default_top_k)
                .unwrap_or(st.cfg.top_k_default)
                .clamp(1, 50);
            // Retrieve on the latest message (identical to /query's semantic branch).
            let embedding = st.embedder.embed_one(&message).await.map_err(internal)?;
            let tuning = Tuning {
                ef_search: st.cfg.hnsw_ef_search,
                statement_timeout_ms: st.cfg.query_timeout_ms,
            };
            let filters = Filters {
                device_id,
                after_unix_nanos: after,
                before_unix_nanos: before,
                speaker_id,
            };
            let mut s = retrieve::nearest(&st.pool, &embedding, top_k, &tuning, &filters)
                .await
                .map_err(internal)?;
            s.retain(|x| x.distance <= st.cfg.distance_threshold);
            let ids: Vec<String> = s.iter().filter_map(|x| x.speaker_id.clone()).collect();
            names = crate::speakers::name_map(&st.pool, &ids)
                .await
                .map_err(internal)?;
            sources = s;
        }
        AgentKind::Objects => {
            // Open-vocab object retrieval. Answered synchronously (precomputed) — objects have no
            // speaker prompt, so they bypass the streaming speaker path.
            names = std::collections::HashMap::new();
            match st.clip.clone() {
                None => {
                    precomputed_answer = Some(
                        "I can't look through what you saw on camera right now — the visual search \
                         model isn't loaded on the server."
                            .to_string(),
                    );
                    sources = vec![];
                }
                Some(clip) => {
                    let device_id = qf.device_id.or_else(|| df.device_id.clone());
                    let after = qf.after_unix_nanos.or(df.after_unix_nanos);
                    let before = qf.before_unix_nanos.or(df.before_unix_nanos);
                    let top_k = req
                        .top_k
                        .or(agent.default_top_k)
                        .unwrap_or(st.cfg.object_top_k_default)
                        .clamp(1, 50);
                    let filters = Filters {
                        device_id,
                        after_unix_nanos: after,
                        before_unix_nanos: before,
                        speaker_id: None,
                    };
                    let q = message.clone();
                    let embedding = tokio::task::spawn_blocking(move || clip.embed_text(&q))
                        .await
                        .map_err(|e| internal(anyhow::anyhow!("clip text task join: {e}")))?
                        .map_err(internal)?;
                    let tuning = Tuning {
                        ef_search: st.cfg.hnsw_ef_search,
                        statement_timeout_ms: st.cfg.query_timeout_ms,
                    };
                    let mut s = retrieve::nearest_objects(
                        &st.pool, &embedding, top_k, &tuning, &filters, true,
                    )
                    .await
                    .map_err(internal)?;
                    s.retain(|x| x.distance <= st.cfg.object_distance_threshold);
                    let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
                    for src in &mut s {
                        src.time_label =
                            crate::humanize::humanize_time(src.start_unix_nanos, now, tz);
                    }
                    precomputed_answer = Some(
                        st.llm
                            .answer_objects(&message, &s)
                            .await
                            .map_err(internal)?,
                    );
                    sources = s;
                }
            }
        }
        AgentKind::People => {
            // Person (face) attribution — answered synchronously (precomputed), with its own
            // person-label enrichment (not the speaker enrichment below).
            let device_id = qf.device_id.or_else(|| df.device_id.clone());
            let after = qf.after_unix_nanos.or(df.after_unix_nanos);
            let before = qf.before_unix_nanos.or(df.before_unix_nanos);
            let limit = req
                .top_k
                .or(agent.default_top_k)
                .unwrap_or(st.cfg.person_top_k_default)
                .clamp(1, 200);
            // Same routing as the single-shot path (explicit name → mentioned name → "who was I
            // with" co-occurrence → roster of everyone seen). The roster is what answers
            // "who have you seen so far" without a configured owner.
            let mut s = match resolve_people_sources(
                &st,
                &message,
                qf.person_id.clone(),
                qf.person_name.clone(),
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
                    precomputed_answer = Some(PEOPLE_NO_OWNER.to_string());
                    Vec::new()
                }
            };
            let pids: Vec<String> = s.iter().filter_map(|x| x.speaker_id.clone()).collect();
            names = crate::persons::name_map(&st.pool, &pids)
                .await
                .map_err(internal)?;
            if precomputed_answer.is_none() {
                enrich_persons_for_display(
                    &mut s,
                    &names,
                    Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX),
                    tz,
                );
                precomputed_answer = Some(
                    st.llm
                        .answer_people(&message, &s, &names)
                        .await
                        .map_err(internal)?,
                );
            }
            sources = s;
        }
        AgentKind::Plates => {
            // License-plate attribution — answered synchronously (precomputed), with its own
            // plate-label enrichment (not the speaker enrichment below). Mirrors the People arm but
            // without an owner anchor: no plate filter / no plate token in the message -> no
            // sightings -> the LLM declines.
            let device_id = qf.device_id.or_else(|| df.device_id.clone());
            let after = qf.after_unix_nanos.or(df.after_unix_nanos);
            let before = qf.before_unix_nanos.or(df.before_unix_nanos);
            let limit = req
                .top_k
                .or(agent.default_top_k)
                .unwrap_or(st.cfg.plate_top_k_default)
                .clamp(1, 200);
            let explicit = resolve_plate_filter(&st.pool, qf.plate_id.clone(), qf.plate_text.clone())
                .await
                .map_err(internal)?;
            let mut s = match explicit {
                Some(ids) => retrieve::list_by_plate(
                    &st.pool,
                    &ids,
                    device_id.as_deref(),
                    after,
                    before,
                    limit,
                )
                .await
                .map_err(internal)?,
                None => {
                    let mentioned = crate::plates::resolve_plates_in_text(&st.pool, &message)
                        .await
                        .map_err(internal)?;
                    if mentioned.is_empty() {
                        Vec::new()
                    } else {
                        let ids: Vec<String> = mentioned.iter().map(|u| u.to_string()).collect();
                        retrieve::list_by_plate(
                            &st.pool,
                            &ids,
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
            let plate_ids: Vec<String> = s.iter().filter_map(|x| x.speaker_id.clone()).collect();
            names = crate::plates::label_map(&st.pool, &plate_ids)
                .await
                .map_err(internal)?;
            enrich_plates_for_display(
                &mut s,
                &names,
                Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX),
                tz,
            );
            precomputed_answer = Some(
                st.llm
                    .answer_plates(&message, &s, &names)
                    .await
                    .map_err(internal)?,
            );
            sources = s;
        }
    }
    } // end else (no camera clarification)

    // Attach human-readable speaker names + relative time once, before the sources are sent
    // to the LLM prompt, streamed as the `sources` SSE event, and persisted to jsonb — so all
    // three render the identical natural-language phrasing (no UUIDs / nanoseconds). Objects, People,
    // and Plates set their own display fields above (object label / person label / plate label), so
    // they skip this.
    if !matches!(
        agent.kind,
        AgentKind::Objects | AgentKind::People | AgentKind::Plates
    ) {
        retrieve::enrich_for_display(
            &mut sources,
            &names,
            Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX),
            tz,
        );
    }

    // Everything below runs as the SSE body. Pre-stage errors above already returned a
    // clean HTTP status; in-stream failures surface as `error` events.
    let pool = st.pool.clone();
    let llm = st.llm.clone();
    let system_prompt = agent.system_prompt; // &'static str
    let agent_id_stream = agent_id.clone();
    let is_reflection = agent.kind == AgentKind::Reflection;
    // Reflection model override (cfg wins over a compiled-in agent default).
    let reflection_model = st
        .cfg
        .reflection_llm_model
        .clone()
        .or_else(|| agent.model.map(|m| m.to_string()));

    let stream = async_stream::stream! {
        yield Ok::<Event, Infallible>(sse_event(
            "session",
            &json!({ "session_id": session_id, "agent_id": agent_id_stream }),
        ));
        yield Ok(sse_event(
            "sources",
            &serde_json::to_value(&sources).unwrap_or_else(|_| json!([])),
        ));

        // Reflection with no resolvable target: stream the setup hint, persist, done.
        if let Some(answer) = precomputed_answer {
            yield Ok(sse_event("token", &json!({ "delta": answer })));
            match insert_message(&pool, session_id, "assistant", &answer, Some(&sources), &agent_id_stream).await {
                Ok(message_id) => yield Ok(sse_event("done", &json!({ "message_id": message_id }))),
                Err(e) => {
                    tracing::error!(error = format!("{e:#}"), "failed to persist rag chat answer (precomputed)");
                    yield Ok(sse_event("error", &json!({ "message": "failed to save the answer" })));
                }
            }
        } else {
            // Both branches return different concrete stream types; box to one type.
            let stream_res = if is_reflection {
                llm.reflect_stream(
                    &message,
                    reflection_digest.as_deref().unwrap_or_default(),
                    &sources,
                    &names,
                    system_prompt,
                    history,
                    reflection_model.as_deref(),
                )
                .await
                .map(StreamExt::boxed)
            } else {
                llm.chat_stream(&message, &sources, &names, system_prompt, history)
                    .await
                    .map(StreamExt::boxed)
            };

            match stream_res {
                Ok(mut token_stream) => {
                    let mut answer = String::new();
                    let mut errored = false;
                    while let Some(item) = token_stream.next().await {
                        match item {
                            Ok(delta) => {
                                answer.push_str(&delta);
                                yield Ok(sse_event("token", &json!({ "delta": delta })));
                            }
                            Err(e) => {
                                errored = true;
                                // Log the chain server-side; don't stream sqlx/path/URL internals to the client.
                                tracing::error!(error = format!("{e:#}"), "rag chat token stream failed");
                                yield Ok(sse_event("error", &json!({ "message": "the assistant hit an internal error" })));
                                break;
                            }
                        }
                    }
                    if !errored {
                        match insert_message(
                            &pool,
                            session_id,
                            "assistant",
                            &answer,
                            Some(&sources),
                            &agent_id_stream,
                        )
                        .await
                        {
                            Ok(message_id) => {
                                yield Ok(sse_event("done", &json!({ "message_id": message_id })));
                            }
                            Err(e) => {
                                tracing::error!(error = format!("{e:#}"), "failed to persist rag chat answer");
                                yield Ok(sse_event(
                                    "error",
                                    &json!({ "message": "failed to save the answer" }),
                                ));
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = format!("{e:#}"), "rag chat stream setup failed");
                    yield Ok(sse_event("error", &json!({ "message": "the assistant hit an internal error" })));
                }
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn sse_event(name: &str, data: &serde_json::Value) -> Event {
    Event::default().event(name).data(data.to_string())
}

// ---- read endpoints (agent picker + conversation restore) ----------------------------

#[derive(Debug, Serialize)]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// `GET /v1/rag/agents` — the selectable agents for the UI picker.
pub async fn list_agents(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<AgentInfo>>, (StatusCode, String)> {
    check_auth(&headers, &st)?;
    let agents = crate::agents::list()
        .iter()
        .map(|a| AgentInfo {
            id: a.id.to_string(),
            name: a.name.to_string(),
            description: a.description.to_string(),
        })
        .collect();
    Ok(Json(agents))
}

#[derive(Debug, Serialize)]
pub struct SessionInfo {
    pub session_id: Uuid,
    pub agent_id: String,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `GET /v1/rag/chat/sessions` — recent conversations, newest first.
pub async fn list_sessions(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<SessionInfo>>, (StatusCode, String)> {
    check_auth(&headers, &st)?;
    let rows = sqlx::query(
        "SELECT session_id, agent_id, title, created_at, updated_at \
         FROM chat_sessions ORDER BY updated_at DESC LIMIT 100",
    )
    .fetch_all(&st.pool)
    .await
    .map_err(|e| internal(e.into()))?;
    let sessions = rows
        .into_iter()
        .map(|r| SessionInfo {
            session_id: r.get("session_id"),
            agent_id: r.get("agent_id"),
            title: r.get("title"),
            created_at: r.get("created_at"),
            updated_at: r.get("updated_at"),
        })
        .collect();
    Ok(Json(sessions))
}

#[derive(Debug, Serialize)]
pub struct MessageInfo {
    pub message_id: Uuid,
    pub role: String,
    pub content: String,
    pub sources: Vec<Source>,
    pub created_at: DateTime<Utc>,
}

/// `GET /v1/rag/chat/sessions/{id}/messages` — full transcript incl. persisted citations,
/// so a reopened conversation re-renders its deep-links.
pub async fn list_messages(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<Uuid>,
) -> Result<Json<Vec<MessageInfo>>, (StatusCode, String)> {
    check_auth(&headers, &st)?;
    let rows = sqlx::query(
        "SELECT message_id, role, content, sources, created_at \
         FROM chat_messages WHERE session_id = $1 ORDER BY seq ASC",
    )
    .bind(session_id)
    .fetch_all(&st.pool)
    .await
    .map_err(|e| internal(e.into()))?;
    let messages = rows
        .into_iter()
        .map(|r| {
            let sources = r
                .try_get::<Option<sqlx::types::Json<Vec<Source>>>, _>("sources")
                .ok()
                .flatten()
                .map(|j| j.0)
                .unwrap_or_default();
            MessageInfo {
                message_id: r.get("message_id"),
                role: r.get("role"),
                content: r.get("content"),
                sources,
                created_at: r.get("created_at"),
            }
        })
        .collect();
    Ok(Json(messages))
}

// ---- session store --------------------------------------------------------------------

/// Create a new conversation bound to `agent_id`; title is the first message truncated.
pub async fn create_session(
    pool: &PgPool,
    agent_id: &str,
    first_message: &str,
) -> anyhow::Result<Uuid> {
    let session_id = Uuid::now_v7();
    let title: String = first_message.chars().take(80).collect();
    sqlx::query("INSERT INTO chat_sessions (session_id, agent_id, title) VALUES ($1, $2, $3)")
        .bind(session_id)
        .bind(agent_id)
        .bind(title)
        .execute(pool)
        .await?;
    Ok(session_id)
}

/// The immutable agent binding for a session, or `None` if the session doesn't exist.
pub async fn session_agent(pool: &PgPool, session_id: Uuid) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT agent_id FROM chat_sessions WHERE session_id = $1")
        .bind(session_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("agent_id")))
}

/// The trailing `max_messages` turns (role, content) in chronological order.
pub async fn load_history(
    pool: &PgPool,
    session_id: Uuid,
    max_messages: i64,
) -> anyhow::Result<Vec<(String, String)>> {
    let rows = sqlx::query(
        "SELECT role, content FROM chat_messages \
         WHERE session_id = $1 ORDER BY seq DESC LIMIT $2",
    )
    .bind(session_id)
    .bind(max_messages.max(0))
    .fetch_all(pool)
    .await?;
    let mut history: Vec<(String, String)> = rows
        .into_iter()
        .map(|r| (r.get::<String, _>("role"), r.get::<String, _>("content")))
        .collect();
    history.reverse(); // DESC fetch -> chronological
    Ok(history)
}

/// Append a turn. `seq` is allocated as `MAX(seq)+1` within the same transaction as the
/// insert; the `UNIQUE(session_id, seq)` constraint makes a concurrent racer fail loudly
/// rather than silently reorder the transcript. `sources` is persisted (jsonb) for
/// assistant turns so deep-links survive a reload; `None` for user turns.
pub async fn insert_message(
    pool: &PgPool,
    session_id: Uuid,
    role: &str,
    content: &str,
    sources: Option<&[Source]>,
    agent_id: &str,
) -> anyhow::Result<Uuid> {
    let src_json: Option<sqlx::types::Json<serde_json::Value>> = match sources {
        Some(s) => Some(sqlx::types::Json(serde_json::to_value(s)?)),
        None => None,
    };
    let message_id = Uuid::now_v7();
    let mut tx = pool.begin().await?;
    let seq: i32 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(seq) + 1, 0) FROM chat_messages WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO chat_messages (message_id, session_id, seq, role, content, sources, agent_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(message_id)
    .bind(session_id)
    .bind(seq)
    .bind(role)
    .bind(content)
    .bind(src_json)
    .bind(agent_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE chat_sessions SET updated_at = now() WHERE session_id = $1")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(message_id)
}
