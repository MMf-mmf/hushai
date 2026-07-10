//! Gotham "Detective" — the agentic tool-calling chat runtime (Gotham.md Part 2, G3). Reachable
//! only by explicit `agent_id="gotham"` selection in Wave 1 (the auto-router does NOT route to it),
//! so with the agent unselected — or `GOTHAM_ENABLED=false` — existing chat is byte-identical.
//!
//! Entry: [`run_chat`] builds the same axum `Sse` stream `rag_chat` returns (a strict SUPERSET of
//! today's `session/sources/token/done` events, adding `phase`/`tool_call`/`tool_result`). It drives
//! the loop ([`runtime::drive`]) under a wall-clock watchdog, persists the turn + tool trace, and on
//! any runtime failure/empty answer runs a whole-turn FALLBACK (a grounded recordings answer) so the
//! chat surface never regresses.

pub mod confirm;
pub mod preamble;
pub mod runtime;
pub mod tools;
pub mod trace;

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::config::RagConfig;
use crate::state::AppState;
use runtime::DriveMsg;
use tools::{CallerCtx, TurnSinks};

/// Kill switch check — the chat handler calls this before dispatching to Gotham.
pub fn enabled(cfg: &RagConfig) -> bool {
    cfg.gotham.enabled
}

fn sse(name: &str, data: &Value) -> Event {
    Event::default().event(name).data(data.to_string())
}

fn now_ns() -> i64 {
    Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX)
}

/// Map a driver event to its SSE `Event`.
fn map_msg(m: DriveMsg) -> Event {
    match m {
        DriveMsg::Phase(p) => sse("phase", &json!({ "phase": p })),
        DriveMsg::ToolCall { seq, tool, label, args_summary } => {
            sse("tool_call", &json!({ "seq": seq, "tool": tool, "label": label, "args_summary": args_summary }))
        }
        DriveMsg::ToolResult { seq, tool, ok, summary, sources_added, elapsed_ms } => sse(
            "tool_result",
            &json!({ "seq": seq, "tool": tool, "ok": ok, "summary": summary, "sources_added": sources_added, "elapsed_ms": elapsed_ms }),
        ),
        DriveMsg::Token(delta) => sse("token", &json!({ "delta": delta })),
    }
}

/// Run one Gotham turn and return the SSE stream. Auth + session/history/tz/caller resolution have
/// already happened in `rag_chat`; this owns everything from the `session` event onward.
#[allow(clippy::too_many_arguments)]
pub async fn run_chat(
    st: AppState,
    session_id: Uuid,
    request_agent_id: String,
    message: String,
    history: Vec<rig::completion::Message>,
    tz: i64,
    is_voice: bool,
    _owner_verified: bool,
    device_id: Option<String>,
) -> Response {
    let ctx = CallerCtx { tz, now_ns: now_ns(), device_id, is_voice };
    let wall = if is_voice {
        st.cfg.gotham.voice_wall_clock_secs
    } else {
        st.cfg.gotham.wall_clock_secs
    };

    let stream = async_stream::stream! {
        // Pin the stream's error type (only `Ok`s are ever yielded — SSE is Infallible).
        yield Ok::<Event, Infallible>(sse("session", &json!({
            "session_id": session_id,
            "agent_id": request_agent_id,
            "routed_agent_id": crate::agents::GOTHAM_AGENT_ID,
        })));

        // NOTE: the user turn is already persisted by `rag_chat` (before this dispatch), so we do
        // NOT re-insert it here — that would double-count seq.

        let sinks = Arc::new(Mutex::new(TurnSinks::new()));
        let graph_healthy = runtime::probe_graph(&st.cfg.gotham).await;

        // Spawn the loop; forward its events under the wall-clock watchdog.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DriveMsg>();
        let driver = tokio::spawn(runtime::drive(
            st.clone(),
            message.clone(),
            history,
            ctx.clone(),
            sinks.clone(),
            graph_healthy,
            tx,
        ));

        let deadline = tokio::time::sleep(Duration::from_secs(wall.max(1)));
        tokio::pin!(deadline);
        let mut timed_out = false;
        let mut streamed_token = false;
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(m) => {
                        if matches!(m, DriveMsg::Token(_)) { streamed_token = true; }
                        yield Ok(map_msg(m));
                    }
                    None => break, // driver finished and dropped tx
                },
                _ = &mut deadline => { timed_out = true; break; }
            }
        }

        let mut result = if timed_out {
            driver.abort();
            tracing::warn!(secs = wall, "gotham: wall-clock watchdog fired");
            runtime::LoopResult { errored: true, ..Default::default() }
        } else {
            driver.await.unwrap_or_else(|e| {
                tracing::warn!(error = %e, "gotham: driver task join failed");
                runtime::LoopResult { errored: true, ..Default::default() }
            })
        };

        // Whole-turn FALLBACK: any runtime failure / empty answer degrades to a grounded recordings
        // answer over the same message, so the chat surface never regresses (§2.4).
        let mut answer = std::mem::take(&mut result.answer);
        let mut trace = std::mem::take(&mut result.trace);
        if result.errored || (answer.trim().is_empty() && !result.ask_user) {
            trace.fell_back = true;
            match fallback_answer(&st, &message, &ctx).await {
                Ok((a, mut fb_sources)) => {
                    answer = a;
                    let mut s = sinks.lock().unwrap();
                    // Fallback sources become the turn's evidence (the tool loop produced none).
                    let taken = std::mem::take(&mut fb_sources);
                    s.add(&taken);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "gotham: fallback also failed");
                    if answer.trim().is_empty() {
                        answer = "I couldn't complete that investigation right now.".to_string();
                    }
                }
            }
        }

        // Final sources (deduped, globally numbered by the tools / fallback).
        let sources = { sinks.lock().unwrap().sources.clone() };
        yield Ok(sse("sources", &serde_json::to_value(&sources).unwrap_or_else(|_| json!([]))));

        // If nothing was streamed as tokens (fallback path, or ask_user), emit the answer now.
        if !streamed_token && !answer.is_empty() {
            yield Ok(sse("token", &json!({ "delta": answer })));
        }

        // Persist the assistant turn + tool trace, then close.
        match crate::chat::insert_message(&st.pool, session_id, "assistant", &answer, Some(&sources), crate::agents::GOTHAM_AGENT_ID).await {
            Ok(message_id) => {
                trace::persist(&st.pool, message_id, &trace).await;
                yield Ok(sse("done", &json!({ "message_id": message_id })));
            }
            Err(e) => {
                tracing::warn!(error = %e, "gotham: persisting assistant turn failed");
                yield Ok(sse("error", &json!({ "message": "failed to persist the answer" })));
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

/// The whole-turn fallback: a single grounded recordings answer over the message (the auto pipeline's
/// core, minus routing/condensation — enough to never leave the user empty-handed). Returns the
/// answer + its citation sources.
async fn fallback_answer(
    st: &AppState,
    message: &str,
    ctx: &CallerCtx,
) -> anyhow::Result<(String, Vec<crate::retrieve::Source>)> {
    use crate::retrieve::{self, Filters, Tuning};
    let embedding = st.embedder.embed_one(message).await?;
    let tuning = Tuning {
        ef_search: st.cfg.hnsw_ef_search,
        statement_timeout_ms: st.cfg.query_timeout_ms,
    };
    let filters = Filters {
        device_id: ctx.device_id.clone(),
        after_unix_nanos: None,
        before_unix_nanos: None,
        speaker_id: None,
    };
    let mut sources = retrieve::nearest(&st.pool, &embedding, st.cfg.top_k_default, &tuning, &filters).await?;
    sources.retain(|x| x.distance <= st.cfg.distance_threshold);
    let ids: Vec<String> = sources.iter().filter_map(|x| x.speaker_id.clone()).collect();
    let names = crate::speakers::name_map(&st.pool, &ids).await?;
    retrieve::enrich_for_display(&mut sources, &names, ctx.now_ns, ctx.tz);
    let answer = st.llm.answer(message, &sources, &names).await?;
    Ok((answer, sources))
}
