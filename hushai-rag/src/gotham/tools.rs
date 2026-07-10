//! Gotham tool registry (Gotham.md §2.3). Each tool is a thin adapter over an EXISTING hushai-rag
//! retrieval/analytics function (or a backend `/v1/graph/*` HTTP call) — the agent adds NO new
//! perception, it orchestrates the capabilities the auto-router already exposes one-shot.
//!
//! ONE dynamic tool type. rig's `Tool` trait wants a distinct type per tool (`const NAME` +
//! associated `Args`/`Output`); a 14-tool registry would be 14 impls. Instead we implement rig's
//! object-safe `ToolDyn` ONCE for [`GothamTool`] (a `ToolKind` + captured `AppState`/ctx/sinks) and
//! register a `Vec<Box<dyn ToolDyn>>`. The same [`exec`] is called directly by the `react` runtime.
//!
//! CITATIONS: evidence-yielding tools push their `Source` rows into the shared [`TurnSinks`] (deduped
//! by segment_id, globally `[n]`-numbered); the observation returned to the model references those
//! global indices, and the runtime emits the final `sources` SSE from the same accumulator — exactly
//! the citation contract the existing chat uses.

use std::sync::{Arc, Mutex};

use chrono::Utc;
use rig::completion::ToolDefinition;
use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::retrieve::{self, Filters, Source, Tuning};
use crate::state::AppState;

/// Per-turn caller context threaded into every tool (never the raw machine values the model must
/// not see — those are humanized inside each tool).
#[derive(Debug, Clone)]
pub struct CallerCtx {
    pub tz: i64,
    pub now_ns: i64,
    pub device_id: Option<String>,
    pub is_voice: bool,
}

/// The turn's shared citation accumulator. Deduped by `segment_id`; `[n]` is the 1-based position.
#[derive(Default)]
pub struct TurnSinks {
    pub sources: Vec<Source>,
    seen: std::collections::HashMap<Uuid, usize>,
}

impl TurnSinks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add sources (deduped), returning `(global_1based_index, source)` for each in input order so
    /// the caller can render `[n]` referencing the turn-global numbering.
    pub fn add(&mut self, incoming: &[Source]) -> Vec<(usize, Source)> {
        let mut out = Vec::with_capacity(incoming.len());
        for s in incoming {
            let idx = if let Some(&i) = self.seen.get(&s.segment_id) {
                i
            } else {
                self.sources.push(s.clone());
                let i = self.sources.len();
                self.seen.insert(s.segment_id, i);
                i
            };
            out.push((idx, s.clone()));
        }
        out
    }
}

/// Read vs mutate — mutate tools are confirmation-gated and only registered in Wave 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideEffect {
    Read,
    Mutate,
}

/// Every Phase-1 tool + the graph tools + `ask_user`. Mutating tools are enumerated for Wave 3 but
/// not registered while `GOTHAM_MUTATIONS_ENABLED=false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    SearchTranscripts,
    LatestConversation,
    ListConversations,
    ConversationTranscript,
    SearchObjects,
    PeopleSightings,
    WhoWasIWith,
    CoPresence,
    PlateSightings,
    PresenceCount,
    FootageStats,
    ReflectionDigest,
    EventsFeed,
    EntityProfile,
    AskUser,
    // Graph tools (register only when /v1/graph/* probes healthy).
    GraphEntity,
    GraphConnections,
    GraphNeighborhood,
    GraphAnomalies,
    GraphBriefing,
}

impl ToolKind {
    pub fn name(self) -> &'static str {
        use ToolKind::*;
        match self {
            SearchTranscripts => "search_transcripts",
            LatestConversation => "latest_conversation",
            ListConversations => "list_conversations",
            ConversationTranscript => "conversation_transcript",
            SearchObjects => "search_objects",
            PeopleSightings => "people_sightings",
            WhoWasIWith => "who_was_i_with",
            CoPresence => "co_presence",
            PlateSightings => "plate_sightings",
            PresenceCount => "presence_count",
            FootageStats => "footage_stats",
            ReflectionDigest => "reflection_digest",
            EventsFeed => "events_feed",
            EntityProfile => "entity_profile",
            AskUser => "ask_user",
            GraphEntity => "graph_entity",
            GraphConnections => "graph_connections",
            GraphNeighborhood => "graph_neighborhood",
            GraphAnomalies => "graph_anomalies",
            GraphBriefing => "graph_briefing",
        }
    }

    /// Resolve a model-emitted tool name back to its kind (for labels, gating, react dispatch).
    pub fn from_name(name: &str) -> Option<ToolKind> {
        active_kinds(true).into_iter().find(|k| k.name() == name)
    }

    /// The natural-language label the UI shows while the tool runs ("consulting the people catalog…").
    pub fn ui_label(self) -> &'static str {
        use ToolKind::*;
        match self {
            SearchTranscripts => "searching what was said…",
            LatestConversation => "pulling up the latest conversation…",
            ListConversations => "listing recent conversations…",
            ConversationTranscript => "reading a conversation…",
            SearchObjects => "scanning what was seen…",
            PeopleSightings => "checking the people catalog…",
            WhoWasIWith => "finding who was around…",
            CoPresence => "checking who was seen together…",
            PlateSightings => "checking license plates…",
            PresenceCount => "counting visits…",
            FootageStats => "measuring the footage…",
            ReflectionDigest => "reviewing patterns…",
            EventsFeed => "reviewing flagged activity…",
            EntityProfile => "looking up a profile…",
            AskUser => "asking a clarifying question…",
            GraphEntity => "looking up the relationship graph…",
            GraphConnections => "tracing a connection…",
            GraphNeighborhood => "mapping the neighborhood…",
            GraphAnomalies => "reviewing anomalies…",
            GraphBriefing => "reading the daily briefing…",
        }
    }

    pub fn side_effect(self) -> SideEffect {
        SideEffect::Read // Phase 1 is entirely read-only; mutate tools land in Wave 3.
    }

    pub fn is_graph(self) -> bool {
        use ToolKind::*;
        matches!(
            self,
            GraphEntity | GraphConnections | GraphNeighborhood | GraphAnomalies | GraphBriefing
        )
    }

    /// The provider-facing schema (JSON Schema for the args object).
    pub fn definition(self) -> ToolDefinition {
        use ToolKind::*;
        let window = json!({"type": "string", "description": "time window: today|yesterday|last_7d|last_30d (optional)"});
        let (description, properties, required): (&str, Value, Vec<&str>) = match self {
            SearchTranscripts => (
                "Search recorded conversation transcripts for what was SAID about something.",
                json!({"query": {"type": "string"}, "window": window, "device_id": {"type": "string"}, "speaker_name": {"type": "string"}, "top_k": {"type": "integer"}}),
                vec!["query"],
            ),
            LatestConversation => ("Summarize the most recent recorded conversation.", json!({}), vec![]),
            ListConversations => (
                "List recent recorded conversations (time + participants).",
                json!({"window": window, "participant_name": {"type": "string"}}),
                vec![],
            ),
            ConversationTranscript => (
                "Read the full transcript of one conversation by its id.",
                json!({"conversation_id": {"type": "string"}}),
                vec!["conversation_id"],
            ),
            SearchObjects => (
                "Find when an OBJECT was seen on camera (e.g. 'a red car').",
                json!({"description": {"type": "string"}, "window": window, "device_id": {"type": "string"}, "top_k": {"type": "integer"}}),
                vec!["description"],
            ),
            PeopleSightings => (
                "When a person was seen on camera; omit person_name for the recent roster.",
                json!({"person_name": {"type": "string"}, "window": window, "device_id": {"type": "string"}}),
                vec![],
            ),
            WhoWasIWith => ("Who was seen around the owner in a window.", json!({"window": window}), vec![]),
            CoPresence => (
                "Whether/when two named people were seen together.",
                json!({"person_a": {"type": "string"}, "person_b": {"type": "string"}, "window": window}),
                vec!["person_a", "person_b"],
            ),
            PlateSightings => (
                "When a license plate was seen (by plate text or label).",
                json!({"plate_text": {"type": "string"}, "label": {"type": "string"}, "window": window}),
                vec![],
            ),
            PresenceCount => (
                "How many distinct visits a subject made in a window.",
                json!({"subject_type": {"type": "string", "enum": ["person", "plate", "object"]}, "name_or_text": {"type": "string"}, "window": window}),
                vec!["subject_type", "name_or_text"],
            ),
            FootageStats => (
                "How much video/audio footage exists in a window.",
                json!({"window": window, "lane": {"type": "string", "enum": ["video", "audio"]}}),
                vec![],
            ),
            ReflectionDigest => (
                "A deterministic analytics digest of the owner's recent conversational/mood/social patterns.",
                json!({"window_days": {"type": "integer"}}),
                vec![],
            ),
            EventsFeed => (
                "The timeline of notable flagged events (optionally alerts only / one lane).",
                json!({"window": window, "lane": {"type": "string", "enum": ["person", "plate", "speaker", "object"]}, "alerts_only": {"type": "boolean"}}),
                vec![],
            ),
            EntityProfile => (
                "The accumulated profile of a named person (first/last seen, visit count, notes).",
                json!({"name": {"type": "string"}}),
                vec!["name"],
            ),
            AskUser => (
                "Ask the user ONE clarifying question when the request is ambiguous; ends the turn.",
                json!({"question": {"type": "string"}}),
                vec!["question"],
            ),
            GraphEntity => ("Look up ONE named entity (a person, a license plate, or a place) in the relationship graph and get EVERYTHING it is connected to — the people, vehicles, companions and places linked to it. Use this to answer 'who/what is X connected to / associated with', e.g. which person a plate belongs to.", json!({"entity_name": {"type": "string", "description": "the ONE person/plate/place to look up, e.g. 'Alice' or 'EMD774'"}}), vec!["entity_name"]),
            GraphConnections => ("Find the path between TWO SPECIFIC already-named entities (e.g. is Alice connected to Bob). Only use this when you have BOTH concrete names; to find what a SINGLE entity is connected to, use graph_entity instead.", json!({"a": {"type": "string", "description": "first named entity"}, "b": {"type": "string", "description": "second named entity"}}), vec!["a", "b"]),
            GraphNeighborhood => ("Everything within one or two hops of ONE named entity in the relationship graph (a wider view than graph_entity).", json!({"entity": {"type": "string"}, "depth": {"type": "integer"}}), vec!["entity"]),
            GraphAnomalies => ("Recent pattern anomalies the system flagged.", json!({"window": window}), vec![]),
            GraphBriefing => ("The daily briefing for a date (YYYY-MM-DD).", json!({"date": {"type": "string"}}), vec![]),
        };
        ToolDefinition {
            name: self.name().to_string(),
            description: description.to_string(),
            parameters: json!({"type": "object", "properties": properties, "required": required}),
        }
    }
}

/// The read-only Phase-1 tool set (§2.3 registry sizing: tools 1–14 + `ask_user`).
pub fn phase1_read_tools() -> Vec<ToolKind> {
    use ToolKind::*;
    vec![
        SearchTranscripts,
        LatestConversation,
        ListConversations,
        ConversationTranscript,
        SearchObjects,
        PeopleSightings,
        WhoWasIWith,
        CoPresence,
        PlateSightings,
        PresenceCount,
        FootageStats,
        ReflectionDigest,
        EventsFeed,
        EntityProfile,
        AskUser,
    ]
}

/// The graph tools, registered only when `/v1/graph/*` probes healthy.
pub fn graph_tools() -> Vec<ToolKind> {
    use ToolKind::*;
    vec![GraphEntity, GraphConnections, GraphNeighborhood, GraphAnomalies, GraphBriefing]
}

/// Assemble the active tool set for this turn's caller. `graph_healthy` comes from the startup/turn
/// probe; mutating tools are never included in Phase 1.
pub fn active_kinds(graph_healthy: bool) -> Vec<ToolKind> {
    let mut ks = phase1_read_tools();
    if graph_healthy {
        ks.extend(graph_tools());
    }
    ks
}

/// One dynamically-described tool bound to the turn's state + sinks. Implements rig's `ToolDyn`.
pub struct GothamTool {
    pub kind: ToolKind,
    pub st: AppState,
    pub ctx: CallerCtx,
    pub sinks: Arc<Mutex<TurnSinks>>,
}

impl ToolDyn for GothamTool {
    fn name(&self) -> String {
        self.kind.name().to_string()
    }

    fn definition<'a>(&'a self, _prompt: String) -> WasmBoxedFuture<'a, ToolDefinition> {
        let def = self.kind.definition();
        Box::pin(async move { def })
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let v: Value = serde_json::from_str(&args).unwrap_or(Value::Object(Default::default()));
            exec(self.kind, &self.st, &v, &self.ctx, &self.sinks)
                .await
                .map_err(|e| ToolError::ToolCallError(format!("{e:#}").into()))
        })
    }
}

/// Build the boxed tool set for the rig runtime.
pub fn build_registry(
    st: &AppState,
    ctx: &CallerCtx,
    sinks: &Arc<Mutex<TurnSinks>>,
    graph_healthy: bool,
) -> Vec<Box<dyn ToolDyn>> {
    active_kinds(graph_healthy)
        .into_iter()
        .map(|kind| {
            Box::new(GothamTool {
                kind,
                st: st.clone(),
                ctx: ctx.clone(),
                sinks: sinks.clone(),
            }) as Box<dyn ToolDyn>
        })
        .collect()
}

// ------------------------------------------------------------------------------------------------
// exec — the single dispatch, called by GothamTool::call (rig) AND the react runtime directly.
// ------------------------------------------------------------------------------------------------

/// Sentinel prefix the runtime recognizes for `ask_user` (the question becomes the turn's answer).
pub const ASK_USER_PREFIX: &str = "__ASK_USER__:";

/// Run one tool. Pushes any evidence into `sinks`; returns the model-facing observation string.
pub async fn exec(
    kind: ToolKind,
    st: &AppState,
    args: &Value,
    ctx: &CallerCtx,
    sinks: &Arc<Mutex<TurnSinks>>,
) -> anyhow::Result<String> {
    use ToolKind::*;
    match kind {
        AskUser => {
            let q = arg_str(args, "question").unwrap_or_else(|| "Could you clarify?".to_string());
            Ok(format!("{ASK_USER_PREFIX}{q}"))
        }
        SearchTranscripts => tool_search_transcripts(st, args, ctx, sinks).await,
        LatestConversation => tool_latest_conversation(st, args, ctx, sinks).await,
        ListConversations => tool_list_conversations(st, args, ctx).await,
        ConversationTranscript => tool_conversation_transcript(st, args, ctx, sinks).await,
        SearchObjects => tool_search_objects(st, args, ctx, sinks).await,
        PeopleSightings => tool_people_sightings(st, args, ctx, sinks).await,
        WhoWasIWith => tool_who_was_i_with(st, args, ctx, sinks).await,
        CoPresence => tool_co_presence(st, args, ctx, sinks).await,
        PlateSightings => tool_plate_sightings(st, args, ctx, sinks).await,
        PresenceCount => tool_presence_count(st, args, ctx).await,
        FootageStats => tool_footage_stats(st, args, ctx).await,
        ReflectionDigest => tool_reflection_digest(st, args, ctx).await,
        EventsFeed => tool_events_feed(st, args, ctx, sinks).await,
        EntityProfile => tool_entity_profile(st, args, ctx).await,
        GraphEntity | GraphConnections | GraphNeighborhood | GraphAnomalies | GraphBriefing => {
            tool_graph(kind, st, args, ctx).await
        }
    }
}

// ---- arg + window helpers -----------------------------------------------------------------------

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Resolve a `{"window": "..."}` token (or absent) to `(after, before)` nanos, reusing the same
/// free-text parser the chat handler uses — so the agent's windows match everything else.
fn resolve_window(args: &Value, ctx: &CallerCtx) -> (Option<i64>, Option<i64>) {
    let Some(tok) = arg_str(args, "window") else { return (None, None) };
    let phrase = match tok.as_str() {
        "last_7d" => "last 7 days".to_string(),
        "last_30d" => "last 30 days".to_string(),
        other => other.to_string(),
    };
    match crate::timeparse::window_in_query(&phrase, ctx.now_ns, ctx.tz) {
        Some((a, b)) => (Some(a), Some(b)),
        None => (None, None),
    }
}

/// Device scope: an explicit `device_id` arg wins, else the caller's (voice) device scope.
fn device_scope(args: &Value, ctx: &CallerCtx) -> Option<String> {
    arg_str(args, "device_id").or_else(|| ctx.device_id.clone())
}

/// Render a `[n] (who, time) text` observation from turn-global indexed sources.
fn render_sources(indexed: &[(usize, Source)]) -> String {
    if indexed.is_empty() {
        return "No matching evidence found.".to_string();
    }
    let mut out = String::new();
    for (i, s) in indexed {
        let who = s.speaker_name.clone().unwrap_or_else(|| "someone we haven't identified yet".to_string());
        let txt = s.text.trim();
        let vis = s.visual_context.as_deref().filter(|v| !v.trim().is_empty()).map(|v| format!(" — {}", v.trim())).unwrap_or_default();
        if s.time_label.is_empty() {
            out.push_str(&format!("[{i}] ({who}) {txt}{vis}\n"));
        } else {
            out.push_str(&format!("[{i}] ({who}, {}) {txt}{vis}\n", s.time_label));
        }
    }
    out
}

fn now_ns() -> i64 {
    Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX)
}

fn tuning(st: &AppState) -> Tuning {
    Tuning {
        ef_search: st.cfg.hnsw_ef_search,
        statement_timeout_ms: st.cfg.query_timeout_ms,
    }
}

// ---- individual tools ---------------------------------------------------------------------------

async fn tool_search_transcripts(
    st: &AppState,
    args: &Value,
    ctx: &CallerCtx,
    sinks: &Arc<Mutex<TurnSinks>>,
) -> anyhow::Result<String> {
    let query = arg_str(args, "query").ok_or_else(|| anyhow::anyhow!("missing 'query'"))?;
    let (after, before) = resolve_window(args, ctx);
    let top_k = arg_i64(args, "top_k").unwrap_or(st.cfg.top_k_default).clamp(1, 20);
    let speaker_id = match arg_str(args, "speaker_name") {
        Some(name) => {
            let ids = crate::speakers::resolve_name(&st.pool, &name).await?;
            if ids.is_empty() { None } else { Some(ids.iter().map(|u| u.to_string()).collect()) }
        }
        None => None,
    };
    let filters = Filters {
        device_id: device_scope(args, ctx),
        after_unix_nanos: after,
        before_unix_nanos: before,
        speaker_id,
    };
    let embedding = st.embedder.embed_one(&query).await?;
    let mut s = retrieve::nearest(&st.pool, &embedding, top_k, &tuning(st), &filters).await?;
    s.retain(|x| x.distance <= st.cfg.distance_threshold);
    let ids: Vec<String> = s.iter().filter_map(|x| x.speaker_id.clone()).collect();
    let names = crate::speakers::name_map(&st.pool, &ids).await?;
    retrieve::enrich_for_display(&mut s, &names, ctx.now_ns, ctx.tz);
    if st.cfg.context_vision_enrich_enabled {
        let _ = crate::context::enrich_sources_with_vision(&st.pool, &mut s, 3).await;
    }
    let indexed = sinks.lock().unwrap().add(&s);
    Ok(render_sources(&indexed))
}

async fn tool_latest_conversation(
    st: &AppState,
    _args: &Value,
    ctx: &CallerCtx,
    sinks: &Arc<Mutex<TurnSinks>>,
) -> anyhow::Result<String> {
    let gap_nanos = st.cfg.conversation_gap_secs.max(1) * 1_000_000_000;
    let mut s = retrieve::latest_conversation(
        &st.pool,
        ctx.device_id.as_deref(),
        None,
        None,
        gap_nanos,
        st.cfg.recency_scan_limit,
        st.cfg.recency_max_sentences,
        st.cfg.recency_max_chars,
    )
    .await?;
    let ids: Vec<String> = s.iter().filter_map(|x| x.speaker_id.clone()).collect();
    let names = crate::speakers::name_map(&st.pool, &ids).await?;
    retrieve::enrich_for_display(&mut s, &names, ctx.now_ns, ctx.tz);
    let indexed = sinks.lock().unwrap().add(&s);
    Ok(render_sources(&indexed))
}

async fn tool_list_conversations(st: &AppState, args: &Value, ctx: &CallerCtx) -> anyhow::Result<String> {
    let (after, before) = resolve_window(args, ctx);
    let participant_ids: Option<Vec<Uuid>> = match arg_str(args, "participant_name") {
        Some(name) => {
            let ids = crate::speakers::resolve_name(&st.pool, &name).await?;
            if ids.is_empty() { None } else { Some(ids) }
        }
        None => None,
    };
    let metas = retrieve::list_conversations(
        &st.pool,
        ctx.device_id.as_deref(),
        after,
        before,
        participant_ids.as_deref(),
        st.cfg.summary_max_convos.max(1) as i64,
    )
    .await?;
    if metas.is_empty() {
        return Ok("No conversations found in that window.".to_string());
    }
    let mut out = String::new();
    for m in &metas {
        let when = crate::humanize::humanize_time(m.started_at_unix_nanos, ctx.now_ns, ctx.tz);
        out.push_str(&format!(
            "conversation {} — {} — {} participant(s), {} sentence(s), {}\n",
            m.conversation_id, when, m.speaker_ids.len(), m.sentence_count, m.status
        ));
    }
    Ok(out)
}

async fn tool_conversation_transcript(
    st: &AppState,
    args: &Value,
    ctx: &CallerCtx,
    sinks: &Arc<Mutex<TurnSinks>>,
) -> anyhow::Result<String> {
    let id_str = arg_str(args, "conversation_id").ok_or_else(|| anyhow::anyhow!("missing 'conversation_id'"))?;
    let id = Uuid::parse_str(&id_str).map_err(|_| anyhow::anyhow!("invalid conversation_id"))?;
    let mut s = retrieve::conversation_transcript(&st.pool, id, st.cfg.recency_max_sentences).await?;
    let ids: Vec<String> = s.iter().filter_map(|x| x.speaker_id.clone()).collect();
    let names = crate::speakers::name_map(&st.pool, &ids).await?;
    retrieve::enrich_for_display(&mut s, &names, ctx.now_ns, ctx.tz);
    let indexed = sinks.lock().unwrap().add(&s);
    Ok(render_sources(&indexed))
}

async fn tool_search_objects(
    st: &AppState,
    args: &Value,
    ctx: &CallerCtx,
    sinks: &Arc<Mutex<TurnSinks>>,
) -> anyhow::Result<String> {
    let Some(clip) = st.clip.clone() else {
        return Ok("Object search is unavailable (the visual model isn't loaded).".to_string());
    };
    let description = arg_str(args, "description").ok_or_else(|| anyhow::anyhow!("missing 'description'"))?;
    let (after, before) = resolve_window(args, ctx);
    let top_k = arg_i64(args, "top_k").unwrap_or(st.cfg.object_top_k_default).clamp(1, 20);
    let q = description.clone();
    let embedding = tokio::task::spawn_blocking(move || clip.embed_text(&q))
        .await
        .map_err(|e| anyhow::anyhow!("clip text task join: {e}"))??;
    let filters = Filters {
        device_id: device_scope(args, ctx),
        after_unix_nanos: after,
        before_unix_nanos: before,
        speaker_id: None,
    };
    let mut s = retrieve::nearest_objects(&st.pool, &embedding, top_k, &tuning(st), &filters, true).await?;
    s.retain(|x| x.distance <= st.cfg.object_distance_threshold);
    for src in &mut s {
        src.time_label = crate::humanize::humanize_time(src.start_unix_nanos, ctx.now_ns, ctx.tz);
        if src.text.trim().is_empty() || src.text.trim() == "__frame__" {
            src.text = "something in view".to_string();
        }
    }
    let indexed = sinks.lock().unwrap().add(&s);
    Ok(render_sources(&indexed))
}

async fn tool_people_sightings(
    st: &AppState,
    args: &Value,
    ctx: &CallerCtx,
    sinks: &Arc<Mutex<TurnSinks>>,
) -> anyhow::Result<String> {
    let (after, before) = resolve_window(args, ctx);
    let device = device_scope(args, ctx);
    let limit = st.cfg.person_top_k_default.clamp(1, 200);
    let mut s = match arg_str(args, "person_name") {
        Some(name) => {
            let ids = crate::persons::resolve_name(&st.pool, &name).await?;
            if ids.is_empty() {
                return Ok(format!("No one named {name} is in the catalog."));
            }
            let ids: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
            retrieve::list_by_person(&st.pool, &ids, device.as_deref(), after, before, limit).await?
        }
        None => retrieve::list_recent_persons(&st.pool, device.as_deref(), after, before, limit).await?,
    };
    let pids: Vec<String> = s.iter().filter_map(|x| x.speaker_id.clone()).collect();
    let names = crate::persons::name_map(&st.pool, &pids).await?;
    for src in &mut s {
        src.speaker_name = Some(crate::persons::display_label(src.speaker_id.as_deref(), &names, None));
        src.time_label = crate::humanize::humanize_time(src.start_unix_nanos, ctx.now_ns, ctx.tz);
    }
    let indexed = sinks.lock().unwrap().add(&s);
    Ok(render_sources(&indexed))
}

async fn tool_who_was_i_with(
    st: &AppState,
    args: &Value,
    ctx: &CallerCtx,
    sinks: &Arc<Mutex<TurnSinks>>,
) -> anyhow::Result<String> {
    let (after, before) = resolve_window(args, ctx);
    let owner = resolve_owner_person(st).await?;
    if owner.is_empty() {
        return Ok("No owner identity is set, so I can't tell who you were with.".to_string());
    }
    let mut s = retrieve::list_co_occurring_persons(
        &st.pool,
        &owner,
        device_scope(args, ctx).as_deref(),
        after,
        before,
        st.cfg.person_top_k_default.clamp(1, 200),
    )
    .await?;
    let pids: Vec<String> = s.iter().filter_map(|x| x.speaker_id.clone()).collect();
    let names = crate::persons::name_map(&st.pool, &pids).await?;
    for src in &mut s {
        src.speaker_name = Some(crate::persons::display_label(src.speaker_id.as_deref(), &names, None));
        src.time_label = crate::humanize::humanize_time(src.start_unix_nanos, ctx.now_ns, ctx.tz);
    }
    let indexed = sinks.lock().unwrap().add(&s);
    Ok(render_sources(&indexed))
}

async fn tool_co_presence(
    st: &AppState,
    args: &Value,
    ctx: &CallerCtx,
    sinks: &Arc<Mutex<TurnSinks>>,
) -> anyhow::Result<String> {
    let a = arg_str(args, "person_a").ok_or_else(|| anyhow::anyhow!("missing 'person_a'"))?;
    let b = arg_str(args, "person_b").ok_or_else(|| anyhow::anyhow!("missing 'person_b'"))?;
    let (after, before) = resolve_window(args, ctx);
    let a_ids: Vec<String> = crate::persons::resolve_name(&st.pool, &a).await?.iter().map(|u| u.to_string()).collect();
    let b_ids: Vec<String> = crate::persons::resolve_name(&st.pool, &b).await?.iter().map(|u| u.to_string()).collect();
    if a_ids.is_empty() || b_ids.is_empty() {
        return Ok("One of those people isn't in the catalog.".to_string());
    }
    let mut s = retrieve::list_co_presence_pair(
        &st.pool,
        &a_ids,
        &b_ids,
        device_scope(args, ctx).as_deref(),
        after,
        before,
        st.cfg.person_top_k_default.clamp(1, 200),
    )
    .await?;
    let pids: Vec<String> = s.iter().filter_map(|x| x.speaker_id.clone()).collect();
    let names = crate::persons::name_map(&st.pool, &pids).await?;
    for src in &mut s {
        src.speaker_name = Some(crate::persons::display_label(src.speaker_id.as_deref(), &names, None));
        src.time_label = crate::humanize::humanize_time(src.start_unix_nanos, ctx.now_ns, ctx.tz);
    }
    let indexed = sinks.lock().unwrap().add(&s);
    if indexed.is_empty() {
        return Ok(format!("No record of {a} and {b} being seen together in that window."));
    }
    Ok(render_sources(&indexed))
}

async fn tool_plate_sightings(
    st: &AppState,
    args: &Value,
    ctx: &CallerCtx,
    sinks: &Arc<Mutex<TurnSinks>>,
) -> anyhow::Result<String> {
    let raw = arg_str(args, "plate_text").or_else(|| arg_str(args, "label"))
        .ok_or_else(|| anyhow::anyhow!("missing 'plate_text' or 'label'"))?;
    let (after, before) = resolve_window(args, ctx);
    let ids = crate::plates::resolve_plate_text(&st.pool, &raw).await?;
    if ids.is_empty() {
        return Ok(format!("No plate matching {raw} is in the catalog."));
    }
    let ids: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
    let mut s = retrieve::list_by_plate(&st.pool, &ids, device_scope(args, ctx).as_deref(), after, before, st.cfg.plate_top_k_default.clamp(1, 200)).await?;
    let labels = crate::plates::label_map(&st.pool, &ids).await?;
    for src in &mut s {
        src.speaker_name = src.speaker_id.as_deref().and_then(|id| labels.get(id).cloned());
        src.time_label = crate::humanize::humanize_time(src.start_unix_nanos, ctx.now_ns, ctx.tz);
    }
    let indexed = sinks.lock().unwrap().add(&s);
    Ok(render_sources(&indexed))
}

async fn tool_presence_count(st: &AppState, args: &Value, ctx: &CallerCtx) -> anyhow::Result<String> {
    let subject_type = arg_str(args, "subject_type").unwrap_or_default();
    let name = arg_str(args, "name_or_text").ok_or_else(|| anyhow::anyhow!("missing 'name_or_text'"))?;
    let (after, before) = resolve_window(args, ctx);
    let device = device_scope(args, ctx);
    let gap = st.cfg.presence_visit_gap_secs.max(0) * 1_000_000_000;
    let (summary, label) = match subject_type.as_str() {
        "person" => {
            let ids: Vec<String> = crate::persons::resolve_name(&st.pool, &name).await?.iter().map(|u| u.to_string()).collect();
            if ids.is_empty() { return Ok(format!("No one named {name} is in the catalog.")); }
            (crate::presence::person_presence(&st.pool, &ids, device.as_deref(), after, before, ctx.tz, gap).await?, name.clone())
        }
        "plate" => {
            let ids: Vec<String> = crate::plates::resolve_plate_text(&st.pool, &name).await?.iter().map(|u| u.to_string()).collect();
            if ids.is_empty() { return Ok(format!("No plate matching {name} is in the catalog.")); }
            (crate::presence::plate_presence(&st.pool, &ids, device.as_deref(), after, before, ctx.tz, gap).await?, format!("plate {name}"))
        }
        "object" => (crate::presence::object_presence(&st.pool, &name, device.as_deref(), after, before, ctx.tz, gap).await?, format!("a {name}")),
        other => return Ok(format!("Unknown subject_type {other:?} (use person|plate|object).")),
    };
    Ok(crate::presence::render_presence(&summary, &label, ctx.now_ns, ctx.tz))
}

async fn tool_footage_stats(st: &AppState, args: &Value, ctx: &CallerCtx) -> anyhow::Result<String> {
    let (after, before) = resolve_window(args, ctx);
    let rows = crate::stats::footage_stats(&st.pool, device_scope(args, ctx).as_deref(), after, before).await?;
    let wants_audio = arg_str(args, "lane").as_deref() == Some("audio");
    Ok(crate::stats::render_footage_stats(&rows, wants_audio, ctx.now_ns, ctx.tz))
}

async fn tool_reflection_digest(st: &AppState, args: &Value, _ctx: &CallerCtx) -> anyhow::Result<String> {
    let owner = resolve_owner_speaker(st).await?;
    if owner.is_empty() {
        return Ok("No owner identity is set, so I can't build a reflection digest.".to_string());
    }
    let days = arg_i64(args, "window_days").unwrap_or(st.cfg.analysis_window_days_default).clamp(1, 365);
    let window = crate::analytics::AnalysisWindow::last_days(days);
    let dcfg = crate::analytics::DigestConfig::from_rag(&st.cfg);
    let digest = crate::analytics::compute_digest(&st.pool, &owner, window, &dcfg, None).await?;
    Ok(crate::analytics::render_digest(&digest))
}

async fn tool_events_feed(
    st: &AppState,
    args: &Value,
    ctx: &CallerCtx,
    sinks: &Arc<Mutex<TurnSinks>>,
) -> anyhow::Result<String> {
    let (after, before) = resolve_window(args, ctx);
    let devices: Vec<String> = device_scope(args, ctx).into_iter().collect();
    let subject_type = arg_str(args, "lane");
    let alerts_only = arg_bool(args, "alerts_only");
    let mut s = retrieve::list_events(&st.pool, &devices, after, before, subject_type.as_deref(), alerts_only, 50).await?;
    for src in &mut s {
        src.time_label = crate::humanize::humanize_time(src.start_unix_nanos, ctx.now_ns, ctx.tz);
    }
    let indexed = sinks.lock().unwrap().add(&s);
    Ok(render_sources(&indexed))
}

async fn tool_entity_profile(st: &AppState, args: &Value, ctx: &CallerCtx) -> anyhow::Result<String> {
    let name = arg_str(args, "name").ok_or_else(|| anyhow::anyhow!("missing 'name'"))?;
    // Resolve against people first, then speakers (a named face or a named voice).
    let (subject_type, sid) = {
        let pids = crate::persons::resolve_name(&st.pool, &name).await?;
        if let Some(id) = pids.first() {
            ("person", *id)
        } else {
            let sids = crate::speakers::resolve_name(&st.pool, &name).await?;
            match sids.first() {
                Some(id) => ("speaker", *id),
                None => return Ok(format!("No one named {name} is in the catalog.")),
            }
        }
    };
    if st.cfg.profile_chat_refresh {
        let popts = hushai_backend::profiles::ProfileOpts {
            visit_gap_secs: st.cfg.presence_visit_gap_secs,
            convo_gap_secs: st.cfg.conversation_gap_secs,
            grace_secs: st.cfg.profile_grace_secs,
            ..Default::default()
        };
        if let Err(e) = hushai_backend::profiles::refresh_subject(&st.pool, subject_type, sid, &popts).await {
            tracing::warn!(error = %e, "gotham entity_profile: refresh failed");
        }
    }
    match hushai_backend::profiles::get_profile(&st.pool, subject_type, sid).await? {
        Some(p) => {
            let mut out = format!("Profile of {name}:\n{}", p.profile_text.trim());
            let mut facts = Vec::new();
            if let Some(t) = p.first_seen_unix_nanos {
                facts.push(format!("first seen {}", crate::humanize::humanize_time(t, ctx.now_ns, ctx.tz)));
            }
            if let Some(t) = p.last_seen_unix_nanos {
                facts.push(format!("most recently {}", crate::humanize::humanize_time(t, ctx.now_ns, ctx.tz)));
            }
            if p.visit_count > 0 {
                facts.push(format!("{} visit(s)", p.visit_count));
            }
            if !facts.is_empty() {
                out.push_str(&format!("\n({}.)", facts.join("; ")));
            }
            Ok(out)
        }
        None => Ok(format!("No profile has been built for {name} yet.")),
    }
}

/// Graph tools: GET the backend `/v1/graph/*` read API (loopback + bearer). The three relationship
/// tools (`graph_entity` / `graph_neighborhood` / `graph_connections`) resolve the user-facing NAME
/// to the real `{type}/{id}` graph node, hit the correct backend route
/// (`/v1/graph/neighbors/{type}/{id}?hops=`, `/v1/graph/path?from=&to=`), and render a NAME-based
/// human-readable summary — the model never sees a raw UUID. `graph_anomalies` / `graph_briefing`
/// pass through the raw (truncated) JSON body (they hit `/v1/events` + `/v1/graph/digests`, which are
/// already fine). Errors surface as a plain observation, never a hard failure (the agent can retry).
async fn tool_graph(kind: ToolKind, st: &AppState, args: &Value, ctx: &CallerCtx) -> anyhow::Result<String> {
    let base = st.cfg.gotham.backend_base_url.trim_end_matches('/');
    let max = st.cfg.gotham.tool_result_max_chars;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(st.cfg.gotham.tool_timeout_ms.max(1)))
        .build()?;
    let token = st.cfg.gotham.backend_token.as_deref();
    match kind {
        // Both resolve a NAME → node, then read the origin's neighborhood; `graph_entity` is
        // `graph_neighborhood` pinned to 1 hop.
        ToolKind::GraphEntity | ToolKind::GraphNeighborhood => {
            let (arg_key, hops) = if kind == ToolKind::GraphEntity {
                ("entity_name", 1)
            } else {
                ("entity", arg_i64(args, "depth").unwrap_or(1).clamp(1, 3))
            };
            let name = arg_str(args, arg_key).unwrap_or_default();
            let Some((ntype, id)) = resolve_node(st, &name).await else {
                return Ok(format!("I don't have anyone or anything called \"{name}\" on record."));
            };
            let path = format!("/v1/graph/neighbors/{ntype}/{}?hops={hops}", urlenc(&id));
            let (status, body) = graph_fetch(&client, base, token, &path).await?;
            if !status.is_success() {
                return Ok(format!("The graph service returned no usable result (status {}).", status.as_u16()));
            }
            Ok(truncate(&render_neighbors(st, ntype, &id, &body).await, max))
        }
        ToolKind::GraphConnections => {
            let a = arg_str(args, "a").unwrap_or_default();
            let b = arg_str(args, "b").unwrap_or_default();
            let ra = resolve_node(st, &a).await;
            let rb = resolve_node(st, &b).await;
            let (ta, ia, tb, ib) = match (ra, rb) {
                (Some((ta, ia)), Some((tb, ib))) => (ta, ia, tb, ib),
                (None, _) => return Ok(format!("I don't have anyone or anything called \"{a}\" on record.")),
                (_, None) => return Ok(format!("I don't have anyone or anything called \"{b}\" on record.")),
            };
            let path = format!(
                "/v1/graph/path?from={}&to={}",
                urlenc(&format!("{ta}:{ia}")),
                urlenc(&format!("{tb}:{ib}"))
            );
            let (status, body) = graph_fetch(&client, base, token, &path).await?;
            if !status.is_success() {
                return Ok(format!("No connection found between {a} and {b}."));
            }
            Ok(truncate(&render_path(st, &a, &b, &body).await, max))
        }
        ToolKind::GraphAnomalies => {
            let (after, before) = resolve_window(args, ctx);
            let mut path = "/v1/events?type=pattern_anomaly".to_string();
            if let Some(a) = after { path.push_str(&format!("&after={a}")); }
            if let Some(b) = before { path.push_str(&format!("&before={b}")); }
            let (status, body) = graph_fetch(&client, base, token, &path).await?;
            if !status.is_success() {
                return Ok(format!("The graph service returned no usable result (status {}).", status.as_u16()));
            }
            Ok(truncate(&body, max))
        }
        ToolKind::GraphBriefing => {
            let date = arg_str(args, "date").unwrap_or_else(|| civil_date(ctx.now_ns, ctx.tz));
            let path = format!("/v1/graph/digests/{date}");
            let (status, body) = graph_fetch(&client, base, token, &path).await?;
            if !status.is_success() {
                return Ok(format!("The graph service returned no usable result (status {}).", status.as_u16()));
            }
            Ok(truncate(&body, max))
        }
        _ => unreachable!("tool_graph called with a non-graph kind"),
    }
}

/// GET a `/v1/graph/*` path with the shared client + optional bearer; returns `(status, body)`.
async fn graph_fetch(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    path: &str,
) -> anyhow::Result<(reqwest::StatusCode, String)> {
    let mut req = client.get(format!("{base}{path}"));
    if let Some(tok) = token {
        req = req.bearer_auth(tok);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    Ok((status, body))
}

/// Resolve a user-facing NAME to a graph node `(type, id-string)`: person (`display_name`), then plate
/// (`display_name` OR the normalized plate text — a plate's `display_name` e.g. "EMD774" often differs
/// from its OCR-folded `plate_text_norm` e.g. "EM0774"), then speaker (`display_name`). Case-insensitive
/// (ILIKE); first hit wins. Runtime sqlx (no `.sqlx` offline cache in this crate).
async fn resolve_node(st: &AppState, name: &str) -> Option<(&'static str, String)> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if let Ok(Some(id)) = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT person_id FROM persons WHERE display_name ILIKE $1 LIMIT 1",
    )
    .bind(name)
    .fetch_optional(&st.pool)
    .await
    {
        return Some(("person", id.to_string()));
    }
    let norm = crate::plates::normalize_plate(name);
    if let Ok(Some(id)) = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT plate_id FROM license_plates \
         WHERE display_name ILIKE $1 OR ($2 <> '' AND plate_text_norm = $2) LIMIT 1",
    )
    .bind(name)
    .bind(&norm)
    .fetch_optional(&st.pool)
    .await
    {
        return Some(("plate", id.to_string()));
    }
    if let Ok(Some(id)) = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT speaker_id FROM speakers WHERE display_name ILIKE $1 LIMIT 1",
    )
    .bind(name)
    .fetch_optional(&st.pool)
    .await
    {
        return Some(("speaker", id.to_string()));
    }
    None
}

/// The REVERSE of [`resolve_node`]: a graph node `(type, id)` → its human display name, so the model
/// never sees a raw UUID. `device` ids are already human-readable (returned verbatim). A missing /
/// unnamed / non-uuid catalog node falls back to a friendly phrase, never a bare id.
async fn node_label(st: &AppState, ntype: &str, id: &str) -> String {
    if ntype == "device" {
        return id.to_string();
    }
    let Ok(uid) = uuid::Uuid::parse_str(id) else {
        return unknown_node_phrase(ntype);
    };
    let looked_up: Option<String> = match ntype {
        "person" => sqlx::query_scalar::<_, Option<String>>(
            "SELECT display_name FROM persons WHERE person_id = $1",
        )
        .bind(uid)
        .fetch_optional(&st.pool)
        .await
        .ok()
        .flatten()
        .flatten(),
        "plate" => sqlx::query_scalar::<_, Option<String>>(
            "SELECT COALESCE(display_name, plate_text_norm) FROM license_plates WHERE plate_id = $1",
        )
        .bind(uid)
        .fetch_optional(&st.pool)
        .await
        .ok()
        .flatten()
        .flatten(),
        "speaker" => sqlx::query_scalar::<_, Option<String>>(
            "SELECT display_name FROM speakers WHERE speaker_id = $1",
        )
        .bind(uid)
        .fetch_optional(&st.pool)
        .await
        .ok()
        .flatten()
        .flatten(),
        _ => None,
    };
    match looked_up {
        Some(s) if !s.trim().is_empty() => s,
        _ => unknown_node_phrase(ntype),
    }
}

/// Friendly stand-in when a node has no display name (never a bare UUID).
fn unknown_node_phrase(ntype: &str) -> String {
    match ntype {
        "person" => "an unidentified person",
        "speaker" => "an unidentified voice",
        "plate" => "an unknown plate",
        "device" => "a device",
        _ => "something unrecognized",
    }
    .to_string()
}

/// Cache-through wrapper over [`node_label`] so repeated endpoints in one neighborhood are labeled once.
async fn labeled(
    st: &AppState,
    memo: &mut std::collections::HashMap<(String, String), String>,
    ntype: &str,
    id: &str,
) -> String {
    let key = (ntype.to_string(), id.to_string());
    if let Some(v) = memo.get(&key) {
        return v.clone();
    }
    let label = node_label(st, ntype, id).await;
    memo.insert(key, label.clone());
    label
}

/// Pull `(type, id)` from an `edge_json` endpoint object — `src`/`dst` are NESTED `{type,id}`.
fn edge_endpoint(e: &Value, key: &str) -> (String, String) {
    let obj = e.get(key);
    let t = obj.and_then(|o| o.get("type")).and_then(Value::as_str).unwrap_or("").to_string();
    let i = obj.and_then(|o| o.get("id")).and_then(Value::as_str).unwrap_or("").to_string();
    (t, i)
}

/// A `" (N unit(s))"` count suffix, or empty when the count is unknown/zero.
fn count_paren(n: i64, unit: &str) -> String {
    if n <= 0 {
        String::new()
    } else {
        format!(" ({n} {unit}{})", if n == 1 { "" } else { "s" })
    }
}

/// One readable sentence for a single edge, worded from the origin's perspective. Directed types
/// (`arrived_with_vehicle` person→plate, `visits_place` entity→device) keep natural src→dst order;
/// undirected types read `origin <verb> other`.
fn describe_edge(
    edge_type: &str,
    origin_label: &str,
    other_label: &str,
    src_label: &str,
    dst_label: &str,
    count: i64,
) -> String {
    match edge_type {
        "co_present" => format!(
            "{origin_label} was seen together with {other_label}{}",
            count_paren(count, "time")
        ),
        "conversed_with" => format!(
            "{origin_label} talked with {other_label}{}",
            count_paren(count, "conversation")
        ),
        "arrived_with_vehicle" => format!(
            "{src_label} arrived with the vehicle {dst_label}{}",
            count_paren(count, "time")
        ),
        "visits_place" => format!(
            "{src_label} was seen at {dst_label}{}",
            count_paren(count, "time")
        ),
        "same_identity_candidate" => format!("{origin_label} may be the same as {other_label}"),
        other => format!("{origin_label} {} {other_label}", other.replace('_', " ")),
    }
}

/// Render `/v1/graph/neighbors/{type}/{id}` JSON as a NAME-based summary: one readable sentence per
/// direct (depth-1) edge, plus any farther-out neighbors (depth ≥ 2, only present when hops > 1).
async fn render_neighbors(st: &AppState, origin_type: &str, origin_id: &str, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let origin_label = node_label(st, origin_type, origin_id).await;
    let mut memo: std::collections::HashMap<(String, String), String> = std::collections::HashMap::new();
    memo.insert((origin_type.to_string(), origin_id.to_string()), origin_label.clone());

    // Direct (depth-1) edges → one sentence each.
    let empty = Vec::new();
    let edges = v.get("edges").and_then(Value::as_array).unwrap_or(&empty);
    let mut lines: Vec<String> = Vec::new();
    for e in edges.iter().take(40) {
        let edge_type = e.get("edge_type").and_then(Value::as_str).unwrap_or("");
        // Skip identity candidates ruled out by review — they are not real relationships.
        if edge_type == "same_identity_candidate"
            && e.get("status").and_then(Value::as_str) == Some("rejected")
        {
            continue;
        }
        let (st_t, st_i) = edge_endpoint(e, "src");
        let (dt_t, dt_i) = edge_endpoint(e, "dst");
        let count = e.get("observation_count").and_then(Value::as_i64).unwrap_or(0);
        let src_label = labeled(st, &mut memo, &st_t, &st_i).await;
        let dst_label = labeled(st, &mut memo, &dt_t, &dt_i).await;
        let origin_is_src = st_t == origin_type && st_i == origin_id;
        let other_label = if origin_is_src { dst_label.clone() } else { src_label.clone() };
        lines.push(describe_edge(edge_type, &origin_label, &other_label, &src_label, &dst_label, count));
    }

    // Farther-out neighbors (depth ≥ 2) — the backend records each node once at its minimum depth,
    // so these never overlap the direct edges above.
    let mut far: Vec<String> = Vec::new();
    if let Some(ns) = v.get("neighbors").and_then(Value::as_array) {
        for n in ns.iter().take(60) {
            if n.get("depth").and_then(Value::as_i64).unwrap_or(1) < 2 {
                continue;
            }
            let t = n.get("type").and_then(Value::as_str).unwrap_or("");
            let i = n.get("id").and_then(Value::as_str).unwrap_or("");
            far.push(labeled(st, &mut memo, t, i).await);
        }
    }

    if lines.is_empty() && far.is_empty() {
        return format!("{origin_label} has no recorded relationships in the graph yet.");
    }
    let mut out = String::new();
    if lines.is_empty() {
        out.push_str(&format!("{origin_label} has no direct relationships on record.\n"));
    } else {
        out.push_str(&format!("{origin_label} — relationships:\n"));
        for l in &lines {
            out.push_str("- ");
            out.push_str(l);
            out.push('\n');
        }
    }
    if !far.is_empty() {
        far.sort();
        far.dedup();
        out.push_str(&format!("Also connected further out: {}.\n", far.join(", ")));
    }
    out
}

/// Render `/v1/graph/path` JSON as a NAME-based chain ("Alice → EMD774 → Bob").
async fn render_path(st: &AppState, a_name: &str, b_name: &str, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if !v.get("found").and_then(Value::as_bool).unwrap_or(false) {
        return format!("No connection found between {a_name} and {b_name}.");
    }
    let mut labels: Vec<String> = Vec::new();
    if let Some(path) = v.get("path").and_then(Value::as_array) {
        for node in path {
            if let Some((t, i)) = node.as_str().and_then(|s| s.split_once(':')) {
                labels.push(node_label(st, t, i).await);
            }
        }
    }
    match labels.len() {
        0 => format!("No connection found between {a_name} and {b_name}."),
        1 => format!("{a_name} and {b_name} are the same entity in the graph."),
        _ => format!("Connection: {}", labels.join(" → ")),
    }
}

// ---- small utilities ----------------------------------------------------------------------------

async fn resolve_owner_person(st: &AppState) -> anyhow::Result<Vec<String>> {
    if let Some(id) = st.cfg.owner_person_id.as_deref() {
        return Ok(vec![id.to_string()]);
    }
    if let Some(name) = st.cfg.owner_person_name.as_deref() {
        return Ok(crate::persons::resolve_name(&st.pool, name).await?.iter().map(|u| u.to_string()).collect());
    }
    if let Some((id, _)) = crate::persons::owner(&st.pool).await? {
        return Ok(vec![id.to_string()]);
    }
    Ok(vec![])
}

async fn resolve_owner_speaker(st: &AppState) -> anyhow::Result<Vec<String>> {
    if let Some(id) = st.cfg.owner_speaker_id.as_deref() {
        return Ok(vec![id.to_string()]);
    }
    if let Some(name) = st.cfg.owner_speaker_name.as_deref() {
        return Ok(crate::speakers::resolve_name(&st.pool, name).await?.iter().map(|u| u.to_string()).collect());
    }
    if let Some((id, _)) = crate::speakers::owner(&st.pool).await? {
        return Ok(vec![id.to_string()]);
    }
    Ok(vec![])
}

/// The civil date (YYYY-MM-DD) at `now_ns` shifted by the fixed `tz_offset_secs` — matches the
/// graph's fixed-offset day convention (Gotham.md gotchas: tz offset is a fixed shift, not DST).
fn civil_date(now_ns: i64, tz_offset_secs: i64) -> String {
    let secs = now_ns / 1_000_000_000 + tz_offset_secs;
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_else(Utc::now)
        .format("%Y-%m-%d")
        .to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… (truncated — narrow the request)", &s[..end])
}

/// Minimal percent-encoding for a path segment / query value (space + a few reserved chars). The
/// backend resolves names case-insensitively; we only need to survive spaces and slashes.
fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[allow(dead_code)]
fn _now_ns_unused() -> i64 {
    now_ns()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase1_registers_fifteen_read_tools() {
        assert_eq!(phase1_read_tools().len(), 15, "tools 1-14 + ask_user");
        // No mutate tool is ever in the Phase-1 set.
        assert!(phase1_read_tools().iter().all(|k| k.side_effect() == SideEffect::Read));
    }

    #[test]
    fn graph_tools_add_only_when_healthy() {
        assert_eq!(active_kinds(false).len(), 15);
        assert_eq!(active_kinds(true).len(), 20, "15 + 5 graph tools");
        assert!(active_kinds(true).iter().filter(|k| k.is_graph()).count() == 5);
    }

    #[test]
    fn every_tool_has_a_valid_object_schema() {
        for k in active_kinds(true) {
            let d = k.definition();
            assert_eq!(d.name, k.name());
            assert_eq!(d.parameters["type"], "object", "{} schema must be an object", k.name());
            assert!(d.parameters.get("properties").is_some());
            assert!(!d.description.is_empty());
        }
    }

    #[test]
    fn tool_names_are_unique() {
        let mut names: Vec<&str> = active_kinds(true).iter().map(|k| k.name()).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n, "tool names must be unique");
    }

    #[test]
    fn urlenc_escapes_spaces_and_slashes() {
        assert_eq!(urlenc("Bob Smith"), "Bob%20Smith");
        assert_eq!(urlenc("a/b"), "a%2Fb");
        assert_eq!(urlenc("pl-in.ok_~"), "pl-in.ok_~");
    }
}
