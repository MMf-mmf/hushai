//! The Gotham agent loop: a `rig` runtime (ToolSet + `multi_turn` + `PromptHook`) with a hand-rolled
//! `react` fallback behind the same tool registry (Gotham.md §2.1/§2.4). The driver [`drive`] runs
//! the whole plan→act→observe loop, streaming UI events over a channel and returning the final
//! answer + tool trace. `run_chat` (mod.rs) owns the wall-clock watchdog + whole-turn fallback.
//!
//! Runtime selection: honor `GOTHAM_RUNTIME`; a startup probe ([`probe_tools`]) downgrades `rig`→
//! `react` when the model's Ollama template rejects tools (HTTP 400 "does not support tools").

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use rig::agent::{MultiTurnStreamItem, PromptHook, ToolCallHookAction};
use rig::client::CompletionClient;
use rig::completion::Prompt;
use rig::providers::ollama;
use rig::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingPrompt};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use super::tools::{self, CallerCtx, ToolKind, TurnSinks};
use crate::config::GothamConfig;
use crate::state::AppState;

/// UI events the driver streams up to `run_chat` (which maps them to SSE).
#[derive(Debug, Clone)]
pub enum DriveMsg {
    Phase(&'static str),
    ToolCall { seq: u32, tool: String, label: String, args_summary: String },
    ToolResult { seq: u32, tool: String, ok: bool, summary: String, sources_added: usize, elapsed_ms: u64 },
    Token(String),
}

/// What the loop produced.
#[derive(Debug, Default)]
pub struct LoopResult {
    pub answer: String,
    pub trace: super::trace::TurnTrace,
    /// Set when the turn ended on `ask_user` (the answer IS the question).
    pub ask_user: bool,
    /// Set when the runtime itself errored (→ `run_chat` runs the whole-turn fallback).
    pub errored: bool,
}

/// The mutation/budget gate for the rig runtime. Cheap `Clone` (all shared state is `Arc`).
#[derive(Clone)]
struct GothamHook {
    calls: Arc<AtomicUsize>,
    max_tool_calls: usize,
    mutations_enabled: bool,
}

impl PromptHook<ollama::CompletionModel> for GothamHook {
    fn on_tool_call(
        &self,
        tool_name: &str,
        _tool_call_id: Option<String>,
        _internal_call_id: &str,
        _args: &str,
    ) -> impl std::future::Future<Output = ToolCallHookAction> + Send {
        use ToolCallHookAction as A;
        // Snapshot before the increment so the Nth call (1-based) is allowed and the (N+1)th is not.
        let prior = self.calls.fetch_add(1, Ordering::SeqCst);
        let over_budget = prior >= self.max_tool_calls;
        // A mutate tool must never execute without confirmation; Phase 1 registers none, but gate
        // defensively so a future misregistration can't slip an unconfirmed mutation through.
        let is_mutate = tools::ToolKind::from_name(tool_name)
            .map(|k| k.side_effect() == tools::SideEffect::Mutate)
            .unwrap_or(false);
        let deny_mutate = is_mutate && !self.mutations_enabled;
        async move {
            if over_budget {
                A::skip("tool budget exhausted — answer from what you already have")
            } else if deny_mutate {
                A::skip("that action needs explicit confirmation and is not available here")
            } else {
                A::cont()
            }
        }
    }
}

/// Build a fresh Ollama client at the RAG llm endpoint (self-contained, the advisor pattern — avoids
/// threading the private `Llm` client). Shared decode profile: temp + num_ctx (+ optional seed) ride
/// in `additional_params`, which rig merges into Ollama `options`.
fn ollama_client(st: &AppState) -> anyhow::Result<ollama::Client> {
    ollama::Client::builder()
        .api_key(ollama::OllamaApiKey::default())
        .base_url(&st.cfg.llm_ollama_base_url)
        .build()
        .map_err(|e| anyhow::anyhow!("building Ollama client: {e}"))
}

fn decode_params(cfg: &GothamConfig) -> Value {
    let mut params = json!({ "num_ctx": cfg.num_ctx });
    if let Some(seed) = cfg.seed {
        params["seed"] = json!(seed);
    }
    params
}

/// Run the loop. Sends `DriveMsg`s as it goes; returns the final answer + trace.
pub async fn drive(
    st: AppState,
    message: String,
    history: Vec<rig::completion::Message>,
    ctx: CallerCtx,
    sinks: Arc<Mutex<TurnSinks>>,
    graph_healthy: bool,
    tx: UnboundedSender<DriveMsg>,
) -> LoopResult {
    let cfg = st.cfg.gotham.clone();
    let effective = effective_runtime(&cfg);
    let max_tool_calls = if ctx.is_voice { cfg.voice_max_tool_calls } else { cfg.max_tool_calls };

    let _ = tx.send(DriveMsg::Phase("planning"));
    let result = if effective == "react" {
        drive_react(&st, &cfg, &message, &history, &ctx, &sinks, graph_healthy, max_tool_calls, &tx).await
    } else {
        drive_rig(&st, &cfg, &message, history, &ctx, &sinks, graph_healthy, max_tool_calls, &tx).await
    };
    match result {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, runtime = effective, "gotham runtime failed; will fall back");
            LoopResult { errored: true, ..Default::default() }
        }
    }
}

/// rig runtime: register the tool set + hook, drive `multi_turn` streaming, map items to `DriveMsg`.
#[allow(clippy::too_many_arguments)]
async fn drive_rig(
    st: &AppState,
    cfg: &GothamConfig,
    message: &str,
    history: Vec<rig::completion::Message>,
    ctx: &CallerCtx,
    sinks: &Arc<Mutex<TurnSinks>>,
    graph_healthy: bool,
    max_tool_calls: usize,
    tx: &UnboundedSender<DriveMsg>,
) -> anyhow::Result<LoopResult> {
    let client = ollama_client(st)?;
    let mut preamble = super::preamble::PREAMBLE_GOTHAM.to_string();
    if ctx.is_voice {
        preamble.push_str(crate::agents::SPOKEN_STYLE_SUFFIX);
    }
    let registry = tools::build_registry(st, ctx, sinks, graph_healthy);
    let agent = client
        .agent(&cfg.llm_model)
        .preamble(&preamble)
        .temperature(cfg.temperature)
        .additional_params(decode_params(cfg))
        .default_max_turns(cfg.max_turns)
        .tools(registry)
        .build();

    let hook = GothamHook {
        calls: Arc::new(AtomicUsize::new(0)),
        max_tool_calls,
        mutations_enabled: cfg.mutations_enabled,
    };

    let mut stream = agent
        .stream_prompt(message.to_string())
        .multi_turn(cfg.max_turns)
        .with_history(history)
        .with_hook(hook)
        .await;

    let mut out = LoopResult::default();
    let mut answering = false;
    let mut seq: u32 = 0;
    // internal_call_id -> (seq, tool name, start instant) so a ToolResult can be paired to its call.
    let mut pending: std::collections::HashMap<String, (u32, String, std::time::Instant)> = std::collections::HashMap::new();

    while let Some(item) = stream.next().await {
        let item = match item {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "gotham rig stream error");
                out.errored = out.answer.trim().is_empty();
                break;
            }
        };
        match item {
            MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                tool_call,
                internal_call_id,
            }) => {
                seq += 1;
                // Hard tool-call cap: `GothamHook` already SKIPS execution once the budget is spent
                // (`prior >= max_tool_calls`), but rig still emits a stream item for every REQUESTED
                // call — so an adversarial "keep searching" prompt would stream many `tool_call`
                // frames whose tools never ran (each returns a "budget exhausted" skip result). Drop
                // those over-cap items here so the streamed + persisted trace honestly reflects only
                // the tools that actually executed (≤ max_tool_calls). Not inserting into `pending`
                // makes the matching ToolResult a no-op below.
                if seq as usize > max_tool_calls {
                    continue;
                }
                let tool = tool_call.function.name.clone();
                let label = ToolKind::from_name(&tool).map(|k| k.ui_label().to_string()).unwrap_or_else(|| "working…".to_string());
                let args_summary = summarize_args(&tool_call.function.arguments);
                let _ = tx.send(DriveMsg::ToolCall { seq, tool: tool.clone(), label, args_summary });
                pending.insert(internal_call_id, (seq, tool, std::time::Instant::now()));
            }
            MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult { tool_result, internal_call_id }) => {
                // Over-cap (hook-skipped) calls were never recorded in `pending`; drop their result.
                let Some((tseq, tool, started)) = pending.remove(&internal_call_id) else { continue };
                let text = tool_result_text(&tool_result);
                // ask_user terminates the turn: the question becomes the answer.
                if let Some(q) = text.strip_prefix(tools::ASK_USER_PREFIX) {
                    out.answer = q.to_string();
                    out.ask_user = true;
                    out.trace.record(&tool, json!({}), true, started.elapsed().as_millis() as u64, q.len(), "ask_user");
                    let _ = tx.send(DriveMsg::ToolResult { seq: tseq, tool, ok: true, summary: "asked the user".into(), sources_added: 0, elapsed_ms: started.elapsed().as_millis() as u64 });
                    break;
                }
                let ok = tool_ok(&text);
                let added = sinks.lock().unwrap().sources.len();
                let elapsed_ms = started.elapsed().as_millis() as u64;
                out.trace.record(&tool, json!({}), ok, elapsed_ms, text.len(), if ok { "ok" } else { "error" });
                let _ = tx.send(DriveMsg::ToolResult { seq: tseq, tool, ok, summary: first_line(&text), sources_added: added, elapsed_ms });
            }
            MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t)) => {
                if !answering {
                    answering = true;
                    let _ = tx.send(DriveMsg::Phase("answering"));
                }
                out.answer.push_str(&t.text);
                let _ = tx.send(DriveMsg::Token(t.text));
            }
            _ => {}
        }
    }
    if out.answer.trim().is_empty() && !out.ask_user {
        out.errored = true;
    }
    Ok(out)
}

/// react runtime: a strict single-JSON-object loop over the SAME tool registry, model-agnostic.
#[allow(clippy::too_many_arguments)]
async fn drive_react(
    st: &AppState,
    cfg: &GothamConfig,
    message: &str,
    history: &[rig::completion::Message],
    ctx: &CallerCtx,
    sinks: &Arc<Mutex<TurnSinks>>,
    graph_healthy: bool,
    max_tool_calls: usize,
    tx: &UnboundedSender<DriveMsg>,
) -> anyhow::Result<LoopResult> {
    let client = ollama_client(st)?;
    let kinds = tools::active_kinds(graph_healthy);
    let tool_lines = kinds
        .iter()
        .map(|k| format!("- {}: {}", k.name(), k.definition().description))
        .collect::<Vec<_>>()
        .join("\n");
    let mut preamble = super::preamble::react_protocol(super::preamble::PREAMBLE_GOTHAM, &tool_lines);
    if ctx.is_voice {
        preamble.push_str(crate::agents::SPOKEN_STYLE_SUFFIX);
    }
    let agent = client
        .agent(&cfg.llm_model)
        .preamble(&preamble)
        .temperature(cfg.temperature)
        .additional_params(decode_params(cfg))
        .build();

    // A compact rendering of the recent history for coreference (the react loop is single-shot per
    // turn, so we fold history into the prompt rather than threading rig chat history).
    let history_block = render_history(history);
    let mut observations = String::new();
    let mut out = LoopResult::default();

    for _turn in 0..cfg.max_turns {
        let prompt = format!(
            "{history_block}User: {message}\n\n{}\nRespond with one JSON object.",
            if observations.is_empty() { String::new() } else { format!("Evidence gathered so far:\n{observations}\n") }
        );
        let raw = agent.prompt(prompt).await.map_err(|e| anyhow::anyhow!("react completion: {e}"))?;
        let step = parse_react(&raw);
        match step {
            ReactStep::Final(text) => {
                let _ = tx.send(DriveMsg::Phase("answering"));
                out.answer = text.clone();
                let _ = tx.send(DriveMsg::Token(text));
                return Ok(out);
            }
            ReactStep::Action { tool, args } => {
                let Some(kind) = ToolKind::from_name(&tool) else {
                    observations.push_str(&format!("(unknown tool {tool:?} — ignored)\n"));
                    continue;
                };
                if out.trace.tool_calls() >= max_tool_calls {
                    observations.push_str("(tool budget exhausted — answer now)\n");
                    continue;
                }
                let seq = out.trace.tool_calls() as u32 + 1;
                let _ = tx.send(DriveMsg::ToolCall { seq, tool: tool.clone(), label: kind.ui_label().to_string(), args_summary: summarize_args(&args) });
                let started = std::time::Instant::now();
                let before = sinks.lock().unwrap().sources.len();
                let obs = match tools::exec(kind, st, &args, ctx, sinks).await {
                    Ok(o) => o,
                    Err(e) => format!("(tool error: {e})"),
                };
                let elapsed_ms = started.elapsed().as_millis() as u64;
                if let Some(q) = obs.strip_prefix(tools::ASK_USER_PREFIX) {
                    out.answer = q.to_string();
                    out.ask_user = true;
                    out.trace.record(&tool, args, true, elapsed_ms, q.len(), "ask_user");
                    let _ = tx.send(DriveMsg::ToolResult { seq, tool, ok: true, summary: "asked the user".into(), sources_added: 0, elapsed_ms });
                    return Ok(out);
                }
                let ok = tool_ok(&obs);
                let added = sinks.lock().unwrap().sources.len().saturating_sub(before);
                out.trace.record(&tool, args, ok, elapsed_ms, obs.len(), if ok { "ok" } else { "error" });
                let _ = tx.send(DriveMsg::ToolResult { seq, tool: tool.clone(), ok, summary: first_line(&obs), sources_added: added, elapsed_ms });
                observations.push_str(&format!("[{tool}] {obs}\n"));
            }
            ReactStep::Unparseable => {
                // The forgiving fallback: any non-empty raw text becomes the answer.
                let t = raw.trim();
                if !t.is_empty() {
                    let _ = tx.send(DriveMsg::Phase("answering"));
                    out.answer = t.to_string();
                    let _ = tx.send(DriveMsg::Token(t.to_string()));
                }
                return Ok(out);
            }
        }
    }
    // Hit the turn cap without a final: ask the model once more for a direct answer.
    if out.answer.trim().is_empty() {
        let prompt = format!("{history_block}User: {message}\n\nEvidence:\n{observations}\nAnswer the user directly now, citing [n].");
        if let Ok(a) = agent.prompt(prompt).await {
            let _ = tx.send(DriveMsg::Phase("answering"));
            out.answer = a.clone();
            let _ = tx.send(DriveMsg::Token(a));
        }
    }
    if out.answer.trim().is_empty() {
        out.errored = true;
    }
    Ok(out)
}

// ---- react JSON protocol ------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum ReactStep {
    Action { tool: String, args: Value },
    Final(String),
    Unparseable,
}

/// Forgiving parser: extract the first balanced `{...}` object, accept either `{"action":{...}}` or
/// `{"final":"..."}`. Anything else → Unparseable (the caller then treats raw text as the answer).
fn parse_react(raw: &str) -> ReactStep {
    let Some(obj) = first_json_object(raw) else { return ReactStep::Unparseable };
    if let Some(f) = obj.get("final").and_then(Value::as_str) {
        return ReactStep::Final(f.to_string());
    }
    if let Some(action) = obj.get("action").and_then(Value::as_object)
        && let Some(tool) = action.get("tool").and_then(Value::as_str)
    {
        let args = action.get("args").cloned().unwrap_or_else(|| json!({}));
        return ReactStep::Action { tool: tool.to_string(), args };
    }
    ReactStep::Unparseable
}

/// Extract + parse the first balanced top-level JSON object substring (tolerates prose/fences around
/// it). Brace-counting with string/escape awareness.
fn first_json_object(s: &str) -> Option<Value> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&s[start..=i]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

// ---- misc helpers -------------------------------------------------------------------------------

/// Effective runtime, honoring `GOTHAM_RUNTIME` with a fallback to `rig` for any unknown value.
pub fn effective_runtime(cfg: &GothamConfig) -> &'static str {
    match cfg.runtime.trim().to_lowercase().as_str() {
        "react" => "react",
        _ => "rig",
    }
}

/// Startup tools-probe: fire one tiny tools-enabled completion; if Ollama rejects tools for the
/// model, the caller should select `react`. Best-effort — a transport error is treated as "supports
/// tools" (don't downgrade on a transient hiccup); only a clear rejection downgrades.
pub async fn probe_tools(st: &AppState) -> bool {
    let Ok(client) = ollama_client(st) else { return true };
    // A trivial no-op tool just to force a tools-enabled request shape.
    let sinks = Arc::new(Mutex::new(TurnSinks::new()));
    let ctx = CallerCtx { tz: 0, now_ns: 0, device_id: None, is_voice: false };
    let registry = tools::build_registry(st, &ctx, &sinks, false);
    let agent = client
        .agent(&st.cfg.gotham.llm_model)
        .preamble("probe")
        .tools(registry)
        .build();
    match agent.prompt("ping").await {
        Ok(_) => true,
        Err(e) => {
            let msg = format!("{e}").to_lowercase();
            !(msg.contains("does not support tools") || msg.contains("400"))
        }
    }
}

/// Probe the backend `/v1/graph/*` read API health (any HTTP response ⇒ mounted ⇒ healthy; a
/// transport error ⇒ unhealthy, so graph tools are simply not registered this turn).
pub async fn probe_graph(cfg: &GothamConfig) -> bool {
    let base = cfg.backend_base_url.trim_end_matches('/');
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(cfg.tool_timeout_ms.clamp(1, 3000)))
        .build()
    else {
        return false;
    };
    let mut req = client.get(format!("{base}/v1/graph/entities/__probe__"));
    if let Some(tok) = cfg.backend_token.as_deref() {
        req = req.bearer_auth(tok);
    }
    req.send().await.is_ok()
}

/// Truncate at a UTF-8 char boundary at or below `max` (stable-Rust; `floor_char_boundary` is nightly).
fn clip_to(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn summarize_args(args: &Value) -> String {
    clip_to(&args.to_string(), 160)
}

fn first_line(s: &str) -> String {
    clip_to(s.lines().next().unwrap_or("").trim(), 200)
}

/// Whether a tool's result text represents a SUCCESSFUL call, for the SSE `tool_result.ok` flag +
/// the persisted trace `outcome`. A failed tool surfaces DIFFERENTLY per runtime, and none of the
/// forms begins with a bare `ToolCallError`, so the original `!starts_with("ToolCallError")` check
/// silently reported `ok:true` for a failed tool (e.g. a `plate_sightings` called with no plate arg
/// streamed `ok:true` with a `"Toolset error: ToolCallError: …"` summary). Detect all three markers:
///   * rig runtime — rig serializes a tool `Err(ToolError::ToolCallError(..))` as
///     `"Toolset error: ToolCallError: …"`;
///   * react runtime — a failed `tools::exec` is wrapped as `"(tool error: …)"`;
///   * defensive — a bare `"ToolCallError"` prefix (the original check).
/// The model never sees `ok` (it reasons over the tool OUTPUT text), so this only corrects
/// trace/UI fidelity — it cannot change tool selection or the answer.
fn tool_ok(text: &str) -> bool {
    let t = text.trim_start();
    !(t.starts_with("ToolCallError") || t.starts_with("Toolset error") || t.starts_with("(tool error:"))
}

fn tool_result_text(tr: &rig::completion::message::ToolResult) -> String {
    // ToolResult.content is OneOrMany<ToolResultContent>; render the text parts.
    use rig::completion::message::ToolResultContent;
    tr.content
        .iter()
        .map(|c| match c {
            ToolResultContent::Text(t) => t.text.clone(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("")
}

fn render_history(history: &[rig::completion::Message]) -> String {
    if history.is_empty() {
        return String::new();
    }
    let mut out = String::from("Recent conversation:\n");
    for m in history {
        // Best-effort flatten: Message is user/assistant with content parts.
        let (who, text) = match m {
            rig::completion::Message::User { content } => ("User", flatten_user(content)),
            rig::completion::Message::Assistant { content, .. } => ("Assistant", flatten_assistant(content)),
            _ => ("", String::new()),
        };
        if !text.trim().is_empty() {
            out.push_str(&format!("{who}: {}\n", text.trim()));
        }
    }
    out.push('\n');
    out
}

fn flatten_user(content: &rig::OneOrMany<rig::completion::message::UserContent>) -> String {
    use rig::completion::message::UserContent;
    content
        .iter()
        .filter_map(|c| match c {
            UserContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn flatten_assistant(content: &rig::OneOrMany<rig::completion::message::AssistantContent>) -> String {
    use rig::completion::message::AssistantContent;
    content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_final() {
        let s = r#"{"thought":"done","final":"You saw Bob [1]."}"#;
        assert_eq!(parse_react(s), ReactStep::Final("You saw Bob [1].".into()));
    }

    #[test]
    fn parses_action_with_prose_around() {
        let s = "Sure!\n{\"thought\":\"look\",\"action\":{\"tool\":\"people_sightings\",\"args\":{\"person_name\":\"Bob\"}}}\nok";
        match parse_react(s) {
            ReactStep::Action { tool, args } => {
                assert_eq!(tool, "people_sightings");
                assert_eq!(args["person_name"], "Bob");
            }
            other => panic!("expected action, got {other:?}"),
        }
    }

    #[test]
    fn action_without_args_defaults_empty() {
        let s = r#"{"action":{"tool":"latest_conversation"}}"#;
        match parse_react(s) {
            ReactStep::Action { tool, args } => {
                assert_eq!(tool, "latest_conversation");
                assert!(args.is_object());
            }
            other => panic!("expected action, got {other:?}"),
        }
    }

    #[test]
    fn garbage_is_unparseable() {
        assert_eq!(parse_react("I don't know how to format JSON"), ReactStep::Unparseable);
        assert_eq!(parse_react(""), ReactStep::Unparseable);
    }

    #[test]
    fn braces_inside_strings_dont_break_matching() {
        let s = r#"{"final":"use {braces} freely"}"#;
        assert_eq!(parse_react(s), ReactStep::Final("use {braces} freely".into()));
    }

    #[test]
    fn effective_runtime_defaults_rig() {
        let mk = |r: &str| {
            let mut c = dummy_cfg();
            c.runtime = r.to_string();
            c
        };
        assert_eq!(effective_runtime(&mk("react")), "react");
        assert_eq!(effective_runtime(&mk("rig")), "rig");
        assert_eq!(effective_runtime(&mk("nonsense")), "rig");
    }

    fn dummy_cfg() -> GothamConfig {
        GothamConfig {
            enabled: true,
            runtime: "rig".into(),
            llm_model: "qwen2.5:7b".into(),
            judge_model: None,
            temperature: 0.0,
            seed: None,
            num_ctx: 16384,
            max_turns: 6,
            max_tool_calls: 8,
            voice_max_tool_calls: 4,
            tool_timeout_ms: 20000,
            wall_clock_secs: 120,
            voice_wall_clock_secs: 45,
            tool_result_max_chars: 4000,
            obs_total_max_chars: 16000,
            mutations_enabled: false,
            confirm_ttl_secs: 300,
            audit_reads: true,
            backend_base_url: "http://127.0.0.1:8080".into(),
            backend_token: None,
            briefing_enabled: false,
            briefing_hour: 7,
            critique_enabled: false,
        }
    }
}
