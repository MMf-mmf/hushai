//! Live Gotham "Detective" agentic client for the `agent` modality (G3 / Phase F).
//!
//! Structurally the SUPERSET of [`crate::query_rag`]: we POST each turn to the SAME
//! `POST /v1/rag/chat` endpoint, but with `agent_id="gotham"` so the Detective plan→act→observe
//! loop runs. The stream is a strict superset of the ordinary chat stream — the same
//! `session`/`sources`/`token`/`done`/`error` events PLUS `phase`/`tool_call`/`tool_result` (and a
//! Wave-3 `confirm`) — so on top of the accumulated answer + citations + `routed_agent_id` we also
//! collect the ordered tool trace the loop emitted.
//!
//! A down service / transport error is INFRASTRUCTURE, so the caller marks the case INCONCLUSIVE
//! (exit 2) — never a false regression. A binary with the Detective KILL-SWITCHED off (or an old
//! binary) streams NO tool frames and routes to a concrete grounded capability instead of "gotham";
//! that is not an error here — [`crate::score::score_agent`] degrades every tool metric to Info when
//! the turn did not route to `gotham`.

use crate::ctx::Ctx;
use crate::fixtures::AgentQ;
use crate::query_rag::RagSource;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::{Value, json};

/// Wall-clock cap on one turn's stream. Generous: the Detective's own watchdog is
/// `GOTHAM_WALL_CLOCK_SECS=120`, after which it FALLS BACK and still completes the stream, so this
/// only fires if the whole HTTP transport wedges. Matches the advisor client's ceiling.
const ASK_TIMEOUT_SECS: u64 = 300;

/// One tool the loop invoked, reconstructed by pairing the `tool_call` and `tool_result` SSE events
/// on their shared 1-based `seq`. `ok`/`summary`/`sources_added`/`elapsed_ms` fill in when the
/// matching `tool_result` arrives (a `tool_call` with no result — e.g. a turn aborted mid-tool —
/// keeps the `ok:true` placeholder but is still counted).
#[derive(Debug, Clone, Default)]
pub struct ToolStep {
    pub seq: i64,
    pub tool: String,
    pub label: String,
    pub args_summary: String,
    /// From `tool_result.ok`. NOTE the runtime marks a rig tool error via the result text, so this
    /// mirrors the SSE field verbatim — the scorer asserts on tool PRESENCE/COUNT, not `ok`, so a
    /// runtime `ok` quirk can't silently flip a verdict.
    pub ok: bool,
    pub summary: String,
    pub sources_added: i64,
    pub elapsed_ms: i64,
    /// True once the matching `tool_result` was seen (a bare `tool_call` stays false).
    pub resolved: bool,
}

/// One Detective turn's full result: the accumulated answer, its citations, the concrete routing,
/// the ordered tool trace, and the observed phases.
#[derive(Debug, Clone, Default)]
pub struct AgentResult {
    pub ask: String,
    pub session_id: String,
    /// The `agent_id` the request/session bound to (here normally "gotham").
    pub agent_id: String,
    /// The capability the turn ACTUALLY ran as. `Some("gotham")` ⇒ the Detective loop ran and the
    /// tool trace is authoritative; anything else (or `None`) ⇒ kill-switched/old/degraded binary,
    /// so the scorer treats tool metrics as Info.
    pub routed_agent_id: Option<String>,
    pub answer: String,
    pub sources: Vec<RagSource>,
    /// The ordered tool trace (by `seq`). Empty when the loop used no tools (or gotham is off).
    pub tools: Vec<ToolStep>,
    /// The `phase` events seen, in order (e.g. ["planning","answering"]).
    pub phases: Vec<String>,
    /// A `confirm` SSE event was streamed (Wave-3 two-phase mutation; dormant in Phase 1).
    pub saw_confirm: bool,
    /// Saw an `error` SSE event (assistant hit an internal error mid-stream / persist failure).
    pub errored: bool,
}

/// Fire one Detective turn at `POST /v1/rag/chat` and collect the streamed answer + tool trace.
/// `base_ns` turns offset-based filters into absolute nanos; `session_id` continues a labeled
/// multi-turn session (threaded by the caller). Preflight liveness reuses [`crate::query_rag::rag_up`].
pub async fn ask(ctx: &Ctx, q: &AgentQ, base_ns: i64, session_id: Option<&str>) -> Result<AgentResult> {
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
        let resp = req.send().await.context("POST /v1/rag/chat (agent)")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("gotham chat returned {status}: {}", body.chars().take(300).collect::<String>());
        }

        let mut out = AgentResult { ask: q.ask.clone(), ..Default::default() };
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        'outer: while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading SSE chunk")?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(idx) = buf.find("\n\n") {
                let block = buf[..idx].to_string();
                buf.drain(..idx + 2);
                if handle_block(&block, &mut out) {
                    break 'outer; // done / error terminates the stream
                }
            }
        }
        if !buf.trim().is_empty() {
            handle_block(&buf, &mut out);
        }
        Ok::<AgentResult, anyhow::Error>(out)
    };

    match tokio::time::timeout(std::time::Duration::from_secs(ASK_TIMEOUT_SECS), fut).await {
        Ok(res) => res,
        Err(_) => anyhow::bail!("gotham chat timed out after {ASK_TIMEOUT_SECS}s for turn {:?}", q.ask),
    }
}

/// Parse one SSE event block into `out`. Returns true when the stream is terminal (`done`/`error`).
/// Unknown events are ignored (forward-compat). Only `done`/`error` terminate — every other event
/// (incl. the new `phase`/`tool_call`/`tool_result`/`confirm`) is non-terminal so answer + trace
/// accumulation keeps running to the end.
fn handle_block(block: &str, out: &mut AgentResult) -> bool {
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
                out.routed_agent_id = v.get("routed_agent_id").and_then(|x| x.as_str()).map(str::to_string);
            }
            false
        }
        "phase" => {
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                if let Some(p) = v.get("phase").and_then(|x| x.as_str()) {
                    out.phases.push(p.to_string());
                }
            }
            false
        }
        "tool_call" => {
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                out.tools.push(ToolStep {
                    seq: v.get("seq").and_then(|x| x.as_i64()).unwrap_or(out.tools.len() as i64 + 1),
                    tool: v.get("tool").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                    label: v.get("label").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                    args_summary: v.get("args_summary").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                    ok: true,
                    resolved: false,
                    ..Default::default()
                });
            }
            false
        }
        "tool_result" => {
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                let seq = v.get("seq").and_then(|x| x.as_i64());
                // Pair to the matching call by seq; fall back to the last unresolved call. Resolve
                // the INDEX with immutable scans first (two `iter_mut` closures would doubly-borrow),
                // then take one mutable ref.
                let idx = seq
                    .and_then(|s| out.tools.iter().position(|t| t.seq == s && !t.resolved))
                    .or_else(|| out.tools.iter().rposition(|t| !t.resolved));
                if let Some(step) = idx.map(|i| &mut out.tools[i]) {
                    step.ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(step.ok);
                    step.summary = v.get("summary").and_then(|x| x.as_str()).unwrap_or_default().to_string();
                    step.sources_added = v.get("sources_added").and_then(|x| x.as_i64()).unwrap_or(0);
                    step.elapsed_ms = v.get("elapsed_ms").and_then(|x| x.as_i64()).unwrap_or(0);
                    step.resolved = true;
                }
            }
            false
        }
        "confirm" => {
            out.saw_confirm = true;
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

/// Build the `/v1/rag/chat` request body for a Detective turn. Mirrors `query_rag::build_body` but
/// defaults `agent_id` to the Detective persona; `tz_offset_secs: 0` pins relative-time phrasing.
fn build_body(q: &AgentQ, base_ns: i64, session_id: Option<&str>) -> Value {
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
    body
}
