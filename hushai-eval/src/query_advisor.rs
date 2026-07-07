//! Live advisor client for the `advisor` modality.
//!
//! Mirrors `query_rag`: we POST each scripted turn to `POST /v1/advisor/chat` on the live
//! `hushai-advisor` service (`:8095`), consume the SSE stream, and return what the scorer needs —
//! whether a `questions` follow-up round fired (and the questions themselves), the LAST `chapters`
//! event's numbers (the final grounding; earlier refine iterations are superseded), and the
//! accumulated `token` text (the final answer). The caller threads the first turn's `session_id`
//! (learned from the always-first `session` event) into every later turn, so one fixture is one
//! real multi-turn advisor session — gate rounds and memory included.
//!
//! A down service / transport error is INFRASTRUCTURE, so the caller marks the case INCONCLUSIVE
//! (exit 2) — never a FALSE regression.

use crate::ctx::Ctx;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::{Value, json};

/// Wall-clock cap on one turn's stream so a stuck LLM becomes a scored failure, not a hang. Wider
/// than the RAG cap: the advisor chains several LLM stages per turn (gathering → refining →
/// recalling → routing → drafting → reviewing → polishing → memorizing).
const ASK_TIMEOUT_SECS: u64 = 300;

/// One turn's full result: which terminal shape the turn took + the evidence the scorer asserts on.
#[derive(Debug, Clone, Default)]
pub struct AdvisorTurnResult {
    pub message: String,
    /// From the (always-first) `session` event; the caller threads it into later turns.
    pub session_id: String,
    /// A `questions` follow-up round fired — the turn ended asking, not answering.
    pub saw_questions: bool,
    /// The follow-up questions themselves (empty unless `saw_questions`).
    pub questions: Vec<String>,
    /// Chapter numbers (`no`) of the LAST `chapters` event — the final grounding. Refine
    /// iterations may emit several; only the last one grounds the answer, so we overwrite.
    pub chapters: Vec<i64>,
    /// Accumulated `token` deltas — the final answer text (empty on a questions turn).
    pub answer: String,
    /// Saw an `error` SSE event (the advisor hit an internal error mid-stream).
    pub errored: bool,
}

/// Preflight: is the advisor service reachable? `/healthz` first (the liveness convention shared
/// with the RAG service); failing that, ANY HTTP response from the sessions endpoint (even a
/// 401/404) still means "up" — only a connection-level failure is "down". This separates "service
/// absent" (→ INCONCLUSIVE here) from "wrong token / wrong answer" (a 401 on the chat POST
/// surfaces as a transport error in `ask`, also INCONCLUSIVE).
pub async fn advisor_up(ctx: &Ctx) -> bool {
    let base = ctx.advisor_url.trim_end_matches('/');
    if matches!(ctx.http.get(format!("{base}/healthz")).send().await, Ok(r) if r.status().is_success()) {
        return true;
    }
    ctx.http.get(format!("{base}/v1/advisor/sessions")).send().await.is_ok()
}

/// Precondition probe: the advisor grounds every answer in the ingested book corpus, so an empty
/// `book_chunks` can only produce garbage. A query error here means the advisor migrations aren't
/// applied. Both are INFRA — the caller maps 0 / Err to INCONCLUSIVE ("run ingest-book"), never a
/// scored FAIL. Uses the same DB-direct access `observe`/`poll` use (`ctx.pool`).
pub async fn book_chunk_count(ctx: &Ctx) -> Result<i64> {
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM book_chunks")
        .fetch_one(&ctx.pool)
        .await
        .context("SELECT count(*) FROM book_chunks")?;
    Ok(n)
}

/// Fire one scripted turn at `POST /v1/advisor/chat` and collect the streamed result. `session_id`
/// continues an existing advisor session (the caller threads it from the first turn's result);
/// `None` lets the service auto-create one (its id arrives on the `session` event).
pub async fn ask(ctx: &Ctx, message: &str, session_id: Option<&str>) -> Result<AdvisorTurnResult> {
    let url = format!("{}/v1/advisor/chat", ctx.advisor_url.trim_end_matches('/'));
    let mut body = json!({ "message": message });
    if let Some(sid) = session_id {
        body["session_id"] = json!(sid);
    }

    let mut req = ctx
        .http
        .post(&url)
        .header("accept", "text/event-stream")
        .json(&body);
    if let Some(tok) = &ctx.advisor_token {
        req = req.bearer_auth(tok);
    }

    let fut = async {
        let resp = req.send().await.context("POST /v1/advisor/chat")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("advisor chat returned {status}: {}", body.chars().take(300).collect::<String>());
        }

        let mut out = AdvisorTurnResult { message: message.to_string(), ..Default::default() };
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        'outer: while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading SSE chunk")?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            // Process complete events (separated by a blank line). Keep the trailing partial.
            while let Some(idx) = buf.find("\n\n") {
                let block = buf[..idx].to_string();
                buf.drain(..idx + 2);
                if handle_block(&block, &mut out) {
                    break 'outer; // done / error terminates the stream
                }
            }
        }
        // Flush any final event not terminated by a blank line.
        if !buf.trim().is_empty() {
            handle_block(&buf, &mut out);
        }
        Ok::<AdvisorTurnResult, anyhow::Error>(out)
    };

    match tokio::time::timeout(std::time::Duration::from_secs(ASK_TIMEOUT_SECS), fut).await {
        Ok(res) => res,
        Err(_) => anyhow::bail!("advisor chat timed out after {ASK_TIMEOUT_SECS}s for turn {message:?}"),
    }
}

/// Parse one SSE event block into `out`. Returns true when the stream is terminal (`done`/`error`).
fn handle_block(block: &str, out: &mut AdvisorTurnResult) -> bool {
    let mut event = String::new();
    let mut data_lines: Vec<&str> = Vec::new();
    for raw in block.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue; // blank or keep-alive comment
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    let data = data_lines.join("\n");
    match event.as_str() {
        "session" => {
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                out.session_id = v.get("session_id").and_then(|x| x.as_str()).unwrap_or_default().to_string();
            }
            false
        }
        // Progress heartbeat (gathering/refining/…): nothing scoreable.
        "phase" => false,
        "questions" => {
            out.saw_questions = true;
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                if let Some(qs) = v.get("questions").and_then(|x| x.as_array()) {
                    out.questions = qs.iter().filter_map(|q| q.as_str().map(str::to_string)).collect();
                }
            }
            false
        }
        "chapters" => {
            // Each refine iteration re-emits the grounding; the LAST one wins, so overwrite.
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                if let Some(ch) = v.get("chapters").and_then(|x| x.as_array()) {
                    out.chapters = ch.iter().filter_map(|c| c.get("no").and_then(|n| n.as_i64())).collect();
                }
            }
            false
        }
        "token" => {
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                if let Some(delta) = v.get("delta").and_then(|x| x.as_str()) {
                    out.answer.push_str(delta);
                }
            }
            false
        }
        "error" => {
            out.errored = true;
            true
        }
        "done" => true,
        _ => false,
    }
}
