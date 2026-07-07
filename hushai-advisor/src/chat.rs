//! `POST /v1/advisor/chat` (SSE) plus the session/transcript read endpoints.
//!
//! SSE event order (extends the hushai-rag chat protocol):
//!   `session`   {session_id, phase}          — first, so the client learns an auto-created id
//!   `phase`     {phase}                      — progress heartbeats (gathering/refining/recalling/
//!                                              routing/drafting/reviewing/polishing/memorizing)
//!   `questions` {round, questions[]}         — a Yenta round; terminal for a gathering turn
//!   `chapters`  {iteration, chapters:[{no,title}]} — citation chips render before tokens
//!   `token`     {delta} …                    — the streamed final answer
//!   `done`      {message_id} | `error` {message}
//!
//! Session semantics: a session is a consultation window. `phase` 'gathering' keeps
//! feeding the sufficiency gate; a turn arriving on a 'done' (or crashed-mid-'answering')
//! session starts a FRESH gathering cycle in the same session — history is kept, the
//! round counter and refined question reset.

use std::convert::Infallible;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Sse;
use axum::response::sse::{Event, KeepAlive};
use chrono::{DateTime, Utc};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::books::ChapterRef;
use crate::pipeline::{self, AdvisorEvent, TurnCtx};
use crate::routes::{check_auth, internal};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// Continue this consultation. When absent, a new session is created and its id is
    /// returned in the first SSE `session` event.
    #[serde(default)]
    pub session_id: Option<Uuid>,
    pub message: String,
}

/// `POST /v1/advisor/chat` — run one consultation turn, streamed as Server-Sent Events.
pub async fn advisor_chat(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, String)> {
    check_auth(&headers, &st)?;
    hushai_backend::observe::counter("hushai_advisor_requests_total", &[("endpoint", "chat")]);

    let message = req.message.trim().to_string();
    if message.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "message must not be empty".into()));
    }
    if message.chars().count() > st.cfg.max_message_chars {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("message too long (max {} chars)", st.cfg.max_message_chars),
        ));
    }

    // Resolve the session (existing) or create one (new).
    let (session_id, phase, followup_rounds) = match req.session_id {
        Some(sid) => {
            let (phase, rounds) = session_state(&st.pool, sid)
                .await
                .map_err(internal)?
                .ok_or((StatusCode::NOT_FOUND, "advisor session not found".to_string()))?;
            (sid, phase, rounds)
        }
        None => {
            let sid = create_session(&st.pool, &message).await.map_err(internal)?;
            (sid, "gathering".to_string(), 0)
        }
    };

    // One turn per session at a time: a live turn holds 30–90s of phase-machine state;
    // a concurrent second turn would reset it out from under the running pipeline
    // (see state.rs::inflight). The guard drops with the SSE stream, so a client
    // disconnect releases it too.
    let guard = InflightGuard::acquire(&st.inflight, session_id).ok_or((
        StatusCode::CONFLICT,
        "a turn is already in progress for this session — wait for it to finish".to_string(),
    ))?;

    // A 'done' session (or one that crashed mid-'answering') starts a fresh gathering
    // cycle on the new message: history stays, the cycle state resets.
    let followup_rounds = if phase != "gathering" {
        sqlx::query(
            "UPDATE advisor_sessions \
             SET phase = 'gathering', followup_rounds = 0, refined_question = NULL, \
                 updated_at = now() \
             WHERE session_id = $1",
        )
        .bind(session_id)
        .execute(&st.pool)
        .await
        .map_err(|e| internal(e.into()))?;
        0
    } else {
        followup_rounds
    };

    // Load prior turns BEFORE persisting the new user message (so it isn't
    // double-counted as history), then persist the user turn immediately — a crash
    // mid-answer still records it, and seq stays gap-free.
    let history_text = load_history_text(&st.pool, session_id, st.cfg.history_turns * 2)
        .await
        .map_err(internal)?;
    insert_message(&st.pool, session_id, "user", "message", &message, None)
        .await
        .map_err(internal)?;

    let ctx = TurnCtx {
        session_id,
        message,
        history_text,
        followup_rounds,
    };
    let pipeline_events = pipeline::run_turn(st, ctx);

    let stream = async_stream::stream! {
        // Owns the in-flight slot for the whole stream (released on drop, including
        // client disconnect mid-answer).
        let _guard = guard;
        yield Ok::<Event, Infallible>(sse_event(
            "session",
            &json!({ "session_id": session_id, "phase": "gathering" }),
        ));
        let mut inner = std::pin::pin!(pipeline_events);
        while let Some(ev) = inner.next().await {
            let out = match ev {
                AdvisorEvent::Phase { phase } => sse_event("phase", &json!({ "phase": phase })),
                AdvisorEvent::Questions { round, questions } => {
                    sse_event("questions", &json!({ "round": round, "questions": questions }))
                }
                AdvisorEvent::Chapters { iteration, chapters } => sse_event(
                    "chapters",
                    &json!({
                        "iteration": iteration,
                        "chapters": serde_json::to_value(&chapters).unwrap_or_else(|_| json!([])),
                    }),
                ),
                AdvisorEvent::Token { delta } => sse_event("token", &json!({ "delta": delta })),
                AdvisorEvent::Done { message_id } => {
                    sse_event("done", &json!({ "message_id": message_id }))
                }
                AdvisorEvent::Error { message } => {
                    sse_event("error", &json!({ "message": message }))
                }
            };
            yield Ok(out);
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn sse_event(name: &str, data: &serde_json::Value) -> Event {
    Event::default().event(name).data(data.to_string())
}

/// RAII slot in [`AppState::inflight`]: acquired before the turn starts, released when
/// the SSE stream is dropped. `None` when the session already has a live turn.
struct InflightGuard {
    set: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<Uuid>>>,
    session_id: Uuid,
}

impl InflightGuard {
    fn acquire(
        set: &std::sync::Arc<std::sync::Mutex<std::collections::HashSet<Uuid>>>,
        session_id: Uuid,
    ) -> Option<Self> {
        let inserted = set
            .lock()
            .expect("inflight lock poisoned")
            .insert(session_id);
        inserted.then(|| Self { set: set.clone(), session_id })
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Ok(mut s) = self.set.lock() {
            s.remove(&self.session_id);
        }
    }
}

// ---- read endpoints ---------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SessionInfo {
    pub session_id: Uuid,
    pub title: Option<String>,
    pub phase: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `GET /v1/advisor/sessions` — recent consultations, newest first.
pub async fn list_sessions(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<SessionInfo>>, (StatusCode, String)> {
    check_auth(&headers, &st)?;
    let rows = sqlx::query(
        "SELECT session_id, title, phase, created_at, updated_at \
         FROM advisor_sessions ORDER BY updated_at DESC LIMIT 100",
    )
    .fetch_all(&st.pool)
    .await
    .map_err(|e| internal(e.into()))?;
    let sessions = rows
        .into_iter()
        .map(|r| SessionInfo {
            session_id: r.get("session_id"),
            title: r.get("title"),
            phase: r.get("phase"),
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
    /// 'message' | 'followup_questions' | 'final_answer' — lets a reloaded session
    /// re-render Yenta rounds vs answers faithfully.
    pub kind: String,
    pub content: String,
    pub chapters: Vec<ChapterRef>,
    pub created_at: DateTime<Utc>,
}

/// `GET /v1/advisor/sessions/{id}/messages` — full transcript incl. persisted chapter
/// citations, so a reopened consultation re-renders its flow.
pub async fn list_messages(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<Uuid>,
) -> Result<Json<Vec<MessageInfo>>, (StatusCode, String)> {
    check_auth(&headers, &st)?;
    let rows = sqlx::query(
        "SELECT message_id, role, kind, content, chapters, created_at \
         FROM advisor_messages WHERE session_id = $1 ORDER BY seq ASC",
    )
    .bind(session_id)
    .fetch_all(&st.pool)
    .await
    .map_err(|e| internal(e.into()))?;
    let messages = rows
        .into_iter()
        .map(|r| {
            let chapters = r
                .try_get::<Option<sqlx::types::Json<Vec<ChapterRef>>>, _>("chapters")
                .ok()
                .flatten()
                .map(|j| j.0)
                .unwrap_or_default();
            MessageInfo {
                message_id: r.get("message_id"),
                role: r.get("role"),
                kind: r.get("kind"),
                content: r.get("content"),
                chapters,
                created_at: r.get("created_at"),
            }
        })
        .collect();
    Ok(Json(messages))
}

// ---- session store ------------------------------------------------------------------

/// Create a new consultation; title is the first message truncated.
pub async fn create_session(pool: &PgPool, first_message: &str) -> anyhow::Result<Uuid> {
    let session_id = Uuid::now_v7();
    let title: String = first_message.chars().take(80).collect();
    sqlx::query("INSERT INTO advisor_sessions (session_id, title) VALUES ($1, $2)")
        .bind(session_id)
        .bind(title)
        .execute(pool)
        .await?;
    Ok(session_id)
}

/// A session's (phase, followup_rounds), or `None` if it doesn't exist.
pub async fn session_state(
    pool: &PgPool,
    session_id: Uuid,
) -> anyhow::Result<Option<(String, i64)>> {
    let row = sqlx::query(
        "SELECT phase, followup_rounds FROM advisor_sessions WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| {
        (
            r.get::<String, _>("phase"),
            r.get::<i32, _>("followup_rounds") as i64,
        )
    }))
}

/// The trailing `max_messages` turns rendered as "user:/advisor:" lines (chronological),
/// the text block the sufficiency gate and the refiner read.
pub async fn load_history_text(
    pool: &PgPool,
    session_id: Uuid,
    max_messages: i64,
) -> anyhow::Result<String> {
    let rows = sqlx::query(
        "SELECT role, content FROM advisor_messages \
         WHERE session_id = $1 ORDER BY seq DESC LIMIT $2",
    )
    .bind(session_id)
    .bind(max_messages.max(0))
    .fetch_all(pool)
    .await?;
    let mut lines: Vec<String> = rows
        .into_iter()
        .map(|r| {
            let role: String = r.get("role");
            let content: String = r.get("content");
            let label = if role == "assistant" { "advisor" } else { "user" };
            format!("{label}: {content}")
        })
        .collect();
    lines.reverse(); // DESC fetch -> chronological
    Ok(lines.join("\n"))
}

/// Append a turn. `seq` is allocated as `MAX(seq)+1` within the same transaction as the
/// insert; the `UNIQUE(session_id, seq)` constraint makes a concurrent racer fail loudly
/// rather than silently reorder the transcript (the 0008 contract). `chapters` is
/// persisted (jsonb) for final answers; `None` otherwise.
pub async fn insert_message(
    pool: &PgPool,
    session_id: Uuid,
    role: &str,
    kind: &str,
    content: &str,
    chapters: Option<&[ChapterRef]>,
) -> anyhow::Result<Uuid> {
    let mut tx = pool.begin().await?;
    let message_id = insert_message_tx(&mut tx, session_id, role, kind, content, chapters).await?;
    sqlx::query("UPDATE advisor_sessions SET updated_at = now() WHERE session_id = $1")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(message_id)
}

/// A Yenta round, atomically: the followup_questions turn AND the session's round
/// counter/phase commit together — a failure can't leave a dangling question block whose
/// round was never counted (or vice versa).
pub async fn insert_followup_round(
    pool: &PgPool,
    session_id: Uuid,
    content: &str,
    round: i64,
) -> anyhow::Result<Uuid> {
    let mut tx = pool.begin().await?;
    let message_id =
        insert_message_tx(&mut tx, session_id, "assistant", "followup_questions", content, None)
            .await?;
    sqlx::query(
        "UPDATE advisor_sessions \
         SET followup_rounds = $2, phase = 'gathering', updated_at = now() \
         WHERE session_id = $1",
    )
    .bind(session_id)
    .bind(round as i32)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(message_id)
}

/// The final answer, atomically: the final_answer turn AND the phase='done' transition
/// commit together — a failure can't leave a delivered answer on a session stuck in
/// 'answering'.
pub async fn insert_final_answer(
    pool: &PgPool,
    session_id: Uuid,
    content: &str,
    chapters: &[ChapterRef],
) -> anyhow::Result<Uuid> {
    let mut tx = pool.begin().await?;
    let message_id =
        insert_message_tx(&mut tx, session_id, "assistant", "final_answer", content, Some(chapters))
            .await?;
    sqlx::query(
        "UPDATE advisor_sessions SET phase = 'done', updated_at = now() WHERE session_id = $1",
    )
    .bind(session_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(message_id)
}

/// The shared insert body (seq alloc + row) inside the caller's transaction.
async fn insert_message_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: Uuid,
    role: &str,
    kind: &str,
    content: &str,
    chapters: Option<&[ChapterRef]>,
) -> anyhow::Result<Uuid> {
    let chapters_json: Option<sqlx::types::Json<serde_json::Value>> = match chapters {
        Some(c) => Some(sqlx::types::Json(serde_json::to_value(c)?)),
        None => None,
    };
    let message_id = Uuid::now_v7();
    let seq: i32 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(seq) + 1, 0) FROM advisor_messages WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO advisor_messages (message_id, session_id, seq, role, kind, content, chapters) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(message_id)
    .bind(session_id)
    .bind(seq)
    .bind(role)
    .bind(kind)
    .bind(content)
    .bind(chapters_json)
    .execute(&mut **tx)
    .await?;
    Ok(message_id)
}
