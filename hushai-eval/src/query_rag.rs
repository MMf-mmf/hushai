//! Live RAG-chat client for the `chat`/`rag` modality.
//!
//! Unlike `query::observe` (DB-direct, always available), scoring a RAG ANSWER requires the live
//! `hushai-rag` service (`:8090`). We POST each scenario question to `POST /v1/rag/chat`, consume
//! the SSE stream, and return the accumulated answer + the `sources` + the concrete `routed_agent_id`
//! the auto-router landed on (surfaced by the Stage-0a change to `hushai-rag/src/chat.rs`; `None`
//! against an older RAG binary → the scorer degrades routing to an Info metric rather than failing).
//!
//! A down service / transport error is INFRASTRUCTURE, so the caller marks the case INCONCLUSIVE
//! (exit 2) — never a FALSE regression.

use crate::ctx::Ctx;
use crate::fixtures::ChatQ;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

/// Wall-clock cap on one question's stream so a stuck LLM becomes a scored failure, not a hang.
const ASK_TIMEOUT_SECS: u64 = 90;

/// The subset of `hushai_rag::retrieve::Source` the scorer needs. Optional fields carry serde
/// defaults so a schema drift on the RAG side doesn't break parsing.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct RagSource {
    #[serde(default)]
    pub segment_id: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub start_unix_nanos: i64,
    #[serde(default)]
    pub distance: f64,
    #[serde(default)]
    pub speaker_id: Option<String>,
    #[serde(default)]
    pub speaker_name: Option<String>,
    #[serde(default)]
    pub time_label: String,
}

/// One question's full result: the accumulated answer, its citations, and where it was routed.
#[derive(Debug, Clone, Default)]
pub struct RagAnswer {
    pub ask: String,
    pub session_id: String,
    /// The `agent_id` the request/session was bound to (may be the synthetic "auto").
    pub agent_id: String,
    /// The concrete capability the router resolved to. `None` if the RAG binary predates the
    /// Stage-0a `routed_agent_id` field.
    pub routed_agent_id: Option<String>,
    pub answer: String,
    pub sources: Vec<RagSource>,
    /// Saw an `error` SSE event (the assistant hit an internal error mid-stream).
    pub errored: bool,
}

/// Preflight: is the RAG service reachable? Probe `/healthz` — it's UNauthenticated (liveness),
/// unlike `/v1/rag/*`, which `run_stack.sh` always token-gates (it mints an ephemeral `RAG_TOKEN`).
/// This cleanly separates "service down" (→ INCONCLUSIVE here) from "wrong token / wrong answer"
/// (a 401 on the chat POST surfaces as a transport error in `ask`, also INCONCLUSIVE).
pub async fn rag_up(ctx: &Ctx) -> bool {
    let url = format!("{}/healthz", ctx.rag_url.trim_end_matches('/'));
    matches!(ctx.http.get(&url).send().await, Ok(r) if r.status().is_success())
}

/// Fire one question at `POST /v1/rag/chat` and collect the streamed answer. `base_ns` is the
/// fixture base for turning offset-based filters into absolute nanos. `session_id` continues an
/// existing chat session (the caller threads it from a prior answer's `session_id` when the
/// fixture labels questions with `ChatQ.session`); `None` opens a fresh session.
pub async fn ask(ctx: &Ctx, q: &ChatQ, base_ns: i64, session_id: Option<&str>) -> Result<RagAnswer> {
    let url = format!("{}/v1/rag/chat", ctx.rag_url.trim_end_matches('/'));
    let body = build_body(q, base_ns, session_id);

    let mut req = ctx
        .http
        .post(&url)
        .header("accept", "text/event-stream")
        .json(&body);
    if let Some(tok) = &ctx.rag_token {
        req = req.bearer_auth(tok);
    }

    let fut = async {
        let resp = req.send().await.context("POST /v1/rag/chat")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("rag chat returned {status}: {}", body.chars().take(300).collect::<String>());
        }

        let mut out = RagAnswer { ask: q.ask.clone(), ..Default::default() };
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
        Ok::<RagAnswer, anyhow::Error>(out)
    };

    match tokio::time::timeout(std::time::Duration::from_secs(ASK_TIMEOUT_SECS), fut).await {
        Ok(res) => res,
        Err(_) => anyhow::bail!("rag chat timed out after {ASK_TIMEOUT_SECS}s for question {:?}", q.ask),
    }
}

/// Parse one SSE event block into `out`. Returns true when the stream is terminal (`done`/`error`).
fn handle_block(block: &str, out: &mut RagAnswer) -> bool {
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
                out.agent_id = v.get("agent_id").and_then(|x| x.as_str()).unwrap_or_default().to_string();
                out.routed_agent_id = v
                    .get("routed_agent_id")
                    .and_then(|x| x.as_str())
                    .map(str::to_string);
            }
            false
        }
        "sources" => {
            if let Ok(srcs) = serde_json::from_str::<Vec<RagSource>>(&data) {
                out.sources = srcs;
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

/// Build the `/v1/rag/chat` request body from a question spec. Filter time-bounds are offsets from
/// the fixture base; `tz_offset_secs: 0` pins relative-time phrasing so `time_label`s are stable.
fn build_body(q: &ChatQ, base_ns: i64, session_id: Option<&str>) -> Value {
    let mut body = json!({
        "agent_id": q.agent_id,
        "message": q.ask,
        "tz_offset_secs": 0,
    });
    if let Some(sid) = session_id {
        body["session_id"] = json!(sid);
    }
    if let Some(k) = q.top_k {
        body["top_k"] = json!(k);
    }
    if let Some(f) = &q.filters {
        let mut filters = serde_json::Map::new();
        if let Some(d) = &f.device_id {
            filters.insert("device_id".into(), json!(d));
        }
        if let Some(off) = f.after_offset_ns {
            filters.insert("after_unix_nanos".into(), json!(base_ns + off));
        }
        if let Some(off) = f.before_offset_ns {
            filters.insert("before_unix_nanos".into(), json!(base_ns + off));
        }
        if let Some(s) = &f.speaker_name {
            filters.insert("speaker_name".into(), json!(s));
        }
        if let Some(p) = &f.person_name {
            filters.insert("person_name".into(), json!(p));
        }
        if let Some(p) = &f.plate_text {
            filters.insert("plate_text".into(), json!(p));
        }
        if !filters.is_empty() {
            body["filters"] = Value::Object(filters);
        }
    }
    if let Some(pb) = &q.playback {
        let mut playback = serde_json::Map::new();
        if let Some(d) = &pb.device_id {
            playback.insert("device_id".into(), json!(d));
        }
        if let Some(off) = pb.playhead_offset_ns {
            playback.insert("playhead_unix_nanos".into(), json!(base_ns + off));
        }
        if !playback.is_empty() {
            body["playback"] = Value::Object(playback);
        }
    }
    if let Some(c) = &q.caller {
        let mut caller = serde_json::Map::new();
        if let Some(k) = &c.kind {
            caller.insert("kind".into(), json!(k));
        }
        caller.insert("owner_verified".into(), json!(c.owner_verified));
        body["caller"] = Value::Object(caller);
    }
    body
}
