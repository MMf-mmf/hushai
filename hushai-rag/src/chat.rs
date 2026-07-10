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
use axum::response::sse::{Event, KeepAlive};
use axum::response::{IntoResponse, Sse};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use rig::completion::Message;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::agents::AgentKind;
use crate::retrieve::{self, Filters, Source, Tuning};
use crate::routes::{
    CallerContext, PEOPLE_NO_OWNER, PeopleSources, QueryFilters, REFLECTION_NO_TARGET, check_auth,
    enrich_persons_for_display, enrich_plates_for_display, internal, is_identity_query,
    render_identity, resolve_owner_name, resolve_people_sources, resolve_plate_filter,
    resolve_speaker_filter, resolve_target_speaker,
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
    /// When true AND a speaker filter resolves, route retrieval down the exhaustive
    /// non-semantic path (everything that speaker said in scope, no vector ranking) —
    /// the same routing `/v1/rag/query` already has. The UI's "Thorough" toggle.
    #[serde(default)]
    pub exhaustive: Option<bool>,
    /// Caller's UTC offset in seconds (e.g. -14400 for EDT) for rendering "today/yesterday at
    /// h:MM PM". The browser sends its live offset so spoken times match the user's local clock;
    /// absent (e.g. non-browser callers) falls back to `ANALYSIS_TZ_OFFSET_SECS`.
    #[serde(default)]
    pub tz_offset_secs: Option<i64>,
    /// What the viewer is showing right now (sent on every turn; cheap). CONTEXT, not a filter:
    /// deliberately outside `filters` — it becomes a device/time scope only when the question is
    /// deictic ("this video/clip"), so it never disturbs the request-over-default filter merge.
    /// Absent from old clients / ignored by old servers (serde skips unknown fields).
    #[serde(default)]
    pub playback: Option<PlaybackContext>,
    /// Who is asking (CONTEXT, never a retrieval filter — same precedent as `playback`). The
    /// voice client sets `{kind:"voice", owner_verified}`; drives the spoken-style suffix, the
    /// owner prompt line, and the deterministic "what's my name" answer. See [`CallerContext`].
    #[serde(default)]
    pub caller: Option<CallerContext>,
}

/// The viewer's live playback state: which camera is on screen and where the playhead is.
#[derive(Debug, Deserialize)]
pub struct PlaybackContext {
    /// The actively-viewed camera. Fills `filters.device_id` only when the user hasn't scoped
    /// the chat explicitly (an explicit scope wins).
    #[serde(default)]
    pub device_id: Option<String>,
    /// Wall-clock playhead (unix ns). Anchors a ±`DEICTIC_CLIP_WINDOW_NANOS` window for
    /// deictic questions when no explicit time filters are set.
    #[serde(default)]
    pub playhead_unix_nanos: Option<i64>,
}

/// `POST /v1/rag/chat` — stream a grounded, multi-turn answer as Server-Sent Events.
pub async fn rag_chat(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<axum::response::Response, (StatusCode, String)> {
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
    // isn't double-counted as history). Trailing window bounds the LLM context; the age
    // cutoff keeps a stale session's turns out of the LLM-visible window (and out of the
    // condenser, which would otherwise rewrite the new question around hours-old context).
    let history_rows = load_history(
        &st.pool,
        session_id,
        st.cfg.chat_history_turns * 2,
        st.cfg.chat_history_max_age_secs,
    )
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

    // Persist the user turn immediately (the ORIGINAL text, so the transcript shows what was typed):
    // a crash mid-answer still records it, and seq stays gap-free.
    insert_message(&st.pool, session_id, "user", &message, None, &agent_id)
        .await
        .map_err(internal)?;

    // F4 query condensation: rewrite a follow-up into a standalone query (carry subject + resolve
    // relative time) BEFORE routing + retrieval, so "…and the week before?" doesn't re-embed bare and
    // mis-route. No-op on the first turn / when disabled / when already standalone. From here on
    // `message` is the effective (possibly rewritten) query used for routing, retrieval, and the
    // answer prompt.
    // After condensing, the query is STANDALONE, so the router must classify it ALONE — feeding it the
    // prior turns as well drags it back toward the previous capability (observed: a condensed
    // "How many times did I see a chair?" routed to `recordings` with context but `objects` without).
    //
    // NEVER condense a deterministic-intent question (identity / recency / speaker-roster): those are
    // matched by EXACT phrase on the message text below, and the condenser resolves pronouns
    // ("what's MY name" → "what is the OWNER'S name?"), which would silently defeat the detector on a
    // follow-up turn. They're already standalone, so skipping condensation costs nothing.
    let deterministic_intent = crate::routes::is_identity_query(&message)
        || crate::routes::is_recency_query(&message)
        || crate::routes::is_speaker_roster_query(&message)
        || crate::routes::is_footage_stats_query(&message)
        || crate::routes::is_window_summary_query(&message)
        // Participants questions carry the names the detector needs verbatim; the
        // condenser would rewrite them and defeat the resolver.
        || crate::routes::is_participants_conversation_query(&message);
    let (message, router_context) = if st.cfg.query_condense
        && !history.is_empty()
        && !deterministic_intent
    {
        let condensed = st
            .llm
            .condense(&message, &recent_context)
            .await
            .map_err(internal)?;
        if condensed != message {
            tracing::info!(original = %message, condensed = %condensed, "condensed follow-up query");
        }
        (condensed, String::new())
    } else {
        (message, recent_context)
    };

    // Unified assistant: classify each message and dispatch to the right capability. A session
    // bound to a concrete agent keeps that agent (manual override / older sessions).
    let agent = if agent.id == crate::agents::AUTO_AGENT_ID {
        // Deterministic pre-route: "who was speaking/talking" is a VOICE question for the
        // recordings agent — the LLM router's "who ..." pattern drifts toward `people` (faces).
        // Same intent-detector idiom as `is_count_intent`; also keeps the eval's
        // `expect_routed_agent` assertion stable.
        let routed = if crate::routes::is_speaker_roster_query(&message)
            || crate::routes::is_recency_query(&message)
            || crate::routes::is_identity_query(&message)
            || crate::routes::is_footage_stats_query(&message)
            || crate::routes::is_window_summary_query(&message)
        {
            // Recordings-agent questions the LLM router mis-routes: a "who" roster drifts to faces,
            // "what did we last discuss" drifts on keywords, and "what's my name / who am I" drifts
            // to reflection. Footage totals ("how many minutes of video") and window summaries
            // ("what have we spoken about today") are likewise deterministic recordings-arm
            // answers. Pin them to recordings so the deterministic answers below fire.
            crate::agents::DEFAULT_AGENT_ID
        } else if crate::routes::is_profile_query(&message)
            && !crate::persons::resolve_names_in_text(&st.pool, &message)
                .await
                .map_err(internal)?
                .is_empty()
        {
            // "Tell me about <named face>" → the People arm surfaces the accumulated
            // running-memory profile; pinning keeps `expect_routed_agent` stable.
            "people"
        } else if crate::routes::is_profile_query(&message)
            && !crate::speakers::resolve_names_in_text(&st.pool, &message)
                .await
                .map_err(internal)?
                .is_empty()
        {
            // "Tell me about <named voice>" → the Grounded arm surfaces the voice profile.
            crate::agents::DEFAULT_AGENT_ID
        } else if crate::routes::is_participants_conversation_query(&message)
            && !crate::speakers::resolve_names_in_text(&st.pool, &message)
                .await
                .map_err(internal)?
                .is_empty()
        {
            // "What did X and Y talk about" → the Grounded arm's conversation-catalog
            // branch (0025); keeps `expect_routed_agent` stable.
            crate::agents::DEFAULT_AGENT_ID
        } else {
            st.llm
                .classify_agent(&message, &router_context)
                .await
                .map_err(internal)?
        };
        tracing::info!(routed_to = %routed, "auto-router selected capability");
        crate::agents::get(routed).unwrap_or_else(crate::agents::default)
    } else {
        agent
    };

    // Merge agent default scope under per-request filters (request wins per field), then
    // build this turn's context: a grounded retrieval OR (reflection) an analytics digest.
    let mut qf = req.filters.unwrap_or_default();
    // Deictic clip anchor: "this video/clip" + the viewer's live playback context resolves to
    // the on-screen camera and a ±2 min window around the playhead — for every camera-scoped
    // capability, before `clarify_camera` (a playing clip answers "which camera?" by itself).
    // An explicit chat scope / explicit time filters always win; non-deictic questions ignore
    // playback entirely (zero behavior change). Reflection is excluded (it's not camera-scoped
    // and resolves its own window).
    if agent.kind != AgentKind::Reflection && crate::routes::is_deictic_video_query(&message) {
        if let Some(pb) = &req.playback {
            if qf.device_id.is_none() {
                qf.device_id = pb.device_id.clone();
            }
            if qf.after_unix_nanos.is_none() && qf.before_unix_nanos.is_none() {
                if let Some(ph) = pb.playhead_unix_nanos {
                    let w = crate::routes::DEICTIC_CLIP_WINDOW_NANOS;
                    qf.after_unix_nanos = Some(ph.saturating_sub(w));
                    qf.before_unix_nanos = Some(ph.saturating_add(w));
                }
            }
        }
    }
    let df = &agent.default_filters;
    // Render times in the caller's local civil time (browser offset), falling back to the env default.
    let tz = req.tz_offset_secs.unwrap_or(st.cfg.analysis_tz_offset_secs);

    // Caller context: a spoken client gets the spoken-style suffix; a voice-verified owner
    // unlocks the identity answer + the owner prompt line. Never touches retrieval filters.
    let is_voice = req.caller.as_ref().is_some_and(CallerContext::is_voice);
    let owner_verified = req.caller.as_ref().is_some_and(|c| c.owner_verified);

    // Gotham "Detective": an explicit-selection-only agentic tool-calling runtime (Gotham.md Part 2).
    // The auto-router never lands here (it only returns concrete non-gotham ids), so this fires ONLY
    // when the session/request agent is `gotham` AND the kill switch is on — then Gotham owns the
    // whole SSE stream. When `GOTHAM_ENABLED=false`, we fall through and the `gotham` registry entry's
    // `AgentKind::Grounded` degrades it to an ordinary recordings answer (existing behavior).
    if agent.id == crate::agents::GOTHAM_AGENT_ID && crate::gotham::enabled(&st.cfg) {
        let device_id = req
            .caller
            .as_ref()
            .filter(|c| c.is_voice())
            .and_then(|c| c.device_id.clone());
        return Ok(crate::gotham::run_chat(
            st.clone(),
            session_id,
            agent_id.clone(),
            message.clone(),
            history.clone(),
            tz,
            is_voice,
            owner_verified,
            device_id,
        )
        .await);
        // run_chat already returns an axum Response.
    }

    // For reflection-with-no-target we skip the LLM entirely and stream a setup hint.
    let mut precomputed_answer: Option<String> = None;
    let mut reflection_digest: Option<String> = None;
    // Conversation-group lengths from the 0025 expansion (Grounded semantic path only):
    // `sources` is the FLATTENED render order; the stream site re-groups by these lengths
    // AFTER enrichment so global unnamed-speaker ordinals stay collision-free.
    let mut convo_group_lens: Option<Vec<usize>> = None;
    let mut sources: Vec<Source>;
    let names;

    // Deterministic caller-identity answer ("what's my name"), before any retrieval/LLM — same
    // idiom as the roster/clarify precomputes. Only on the grounded default (the auto-router
    // lands identity questions here); an explicit specialized agent keeps its own behaviour.
    let identity = agent.kind == AgentKind::Grounded && is_identity_query(&message);

    // "This video" with no camera scoped + more than one camera -> ask which one instead of
    // silently answering across everything. Reflection isn't camera-scoped, so it never triggers.
    let scope_is_all = qf.device_id.is_none() && df.device_id.is_none();
    let clarify_camera = !identity
        && scope_is_all
        && matches!(
            agent.kind,
            AgentKind::People | AgentKind::Objects | AgentKind::Plates | AgentKind::Grounded
        )
        && crate::routes::is_deictic_video_query(&message)
        && crate::routes::camera_count(&st.pool).await.map_err(internal)? > 1;

    if identity {
        let name = resolve_owner_name(&st).await.map_err(internal)?;
        precomputed_answer = Some(render_identity(name.as_deref(), owner_verified));
        sources = vec![];
        names = std::collections::HashMap::new();
    } else if clarify_camera {
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
            // Voice deictic anchor (0025): "what were we just talking about", asked BY VOICE
            // with no explicit device filter, means the conversation happening AT THAT
            // PHONE — not whichever camera recorded most recently. Applied only to the
            // deterministic conversation branches below (recency / window summary /
            // participants); the semantic path stays unfiltered so topical questions still
            // search every camera. This is the group-A-not-group-B guarantee for voice.
            let convo_device_id = device_id.clone().or_else(|| {
                if is_voice {
                    req.caller.as_ref().and_then(|c| c.device_id.clone())
                } else {
                    None
                }
            });
            #[allow(unused_assignments)]
            let mut participants_ids: Vec<Uuid> = Vec::new();
            // Recency ("what did we last discuss"): summarize the most recent gap-grouped
            // conversation instead of semantic top-k (which returns a lone keyword-similar 2s
            // snippet — the exact "cites one tiny segment" failure). Window: explicit filters win,
            // else a natural-language phrase ("yesterday"), else unbounded (the newest activity).
            // Enriched in-branch since the summary prompt reads speaker names + humanized times.
            // Footage totals ("how many minutes of video do we have today?") — pure segment
            // arithmetic, precomputed with no LLM. Before this branch the question fell to
            // semantic retrieval, matched nothing, and declined.
            if crate::routes::is_footage_stats_query(&message) {
                let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
                let parsed = crate::timeparse::window_in_query(&message, now, tz);
                let s_after = after.or(parsed.map(|(a, _)| a));
                let s_before = before.or(parsed.map(|(_, b)| b));
                let rows = crate::stats::footage_stats(&st.pool, device_id.as_deref(), s_after, s_before)
                    .await
                    .map_err(internal)?;
                precomputed_answer = Some(crate::stats::render_footage_stats(
                    &rows,
                    crate::stats::wants_audio_lane(&message),
                    now,
                    tz,
                ));
                sources = vec![];
                names = std::collections::HashMap::new();
            } else
            // Window summary ("what have we spoken about today?"): summarize ALL of the
            // window's gap-grouped conversations — never semantic top-k over ~2s fragments
            // (the observed disconnected-snippet failure). A bare ask defaults to today.
            if crate::routes::is_window_summary_query(&message) {
                let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
                let parsed = crate::timeparse::window_in_query(&message, now, tz);
                let mut w_after = after.or(parsed.map(|(a, _)| a));
                let mut w_before = before.or(parsed.map(|(_, b)| b));
                if w_after.is_none() && w_before.is_none() {
                    if let Some((a, b)) = crate::timeparse::window_in_query("today", now, tz) {
                        w_after = Some(a);
                        w_before = Some(b);
                    }
                }
                let gap_nanos = st.cfg.conversation_gap_secs.max(1) * 1_000_000_000;
                let mut convos = retrieve::conversations_in_window(
                    &st.pool,
                    convo_device_id.as_deref(),
                    w_after,
                    w_before,
                    gap_nanos,
                    st.cfg.recency_scan_limit,
                    st.cfg.summary_max_convos,
                    st.cfg.recency_max_sentences,
                    st.cfg.summary_max_total_chars,
                )
                .await
                .map_err(internal)?;
                let ids: Vec<String> = convos
                    .iter()
                    .flatten()
                    .filter_map(|x| x.speaker_id.clone())
                    .collect();
                names = crate::speakers::name_map(&st.pool, &ids)
                    .await
                    .map_err(internal)?;
                for c in &mut convos {
                    retrieve::enrich_for_display(c, &names, now, tz);
                }
                precomputed_answer = Some(
                    st.llm
                        .answer_window_summary(&message, &convos, &names)
                        .await
                        .map_err(internal)?,
                );
                sources = convos.into_iter().flatten().collect();
            } else
            if crate::routes::is_recency_query(&message) {
                let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
                let parsed = crate::timeparse::window_in_query(&message, now, tz);
                let r_after = after.or(parsed.map(|(a, _)| a));
                let r_before = before.or(parsed.map(|(_, b)| b));
                let gap_nanos = st.cfg.conversation_gap_secs.max(1) * 1_000_000_000;
                let mut s = retrieve::latest_conversation(
                    &st.pool,
                    convo_device_id.as_deref(),
                    r_after,
                    r_before,
                    gap_nanos,
                    st.cfg.recency_scan_limit,
                    st.cfg.recency_max_sentences,
                    st.cfg.recency_max_chars,
                )
                .await
                .map_err(internal)?;
                let ids: Vec<String> = s.iter().filter_map(|x| x.speaker_id.clone()).collect();
                names = crate::speakers::name_map(&st.pool, &ids)
                    .await
                    .map_err(internal)?;
                retrieve::enrich_for_display(&mut s, &names, now, tz);
                precomputed_answer = Some(
                    st.llm
                        .answer_recency(&message, &s, &names)
                        .await
                        .map_err(internal)?,
                );
                sources = s;
            } else
            // "What did X and Y talk about" → the persisted conversation catalog (0025):
            // conversations whose participant set contains every resolved name, each
            // rendered as its own section (never mixed). Falls through when no catalog
            // name resolves (the phrase alone must not hijack "what did they talk about").
            if crate::routes::is_participants_conversation_query(&message)
                && !{
                    let pids = crate::speakers::resolve_names_in_text(&st.pool, &message)
                        .await
                        .map_err(internal)?;
                    participants_ids = pids.clone();
                    pids.is_empty()
                }
            {
                let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
                let parsed = crate::timeparse::window_in_query(&message, now, tz);
                let p_after = after.or(parsed.map(|(a, _)| a));
                let p_before = before.or(parsed.map(|(_, b)| b));
                let mut groups = crate::routes::participants_conversation_groups(
                    &st,
                    &participants_ids,
                    convo_device_id.as_deref(),
                    p_after,
                    p_before,
                )
                .await
                .map_err(internal)?;
                let lens: Vec<usize> = groups.iter().map(|g| g.len()).collect();
                let mut flat: Vec<Source> = groups.drain(..).flatten().collect();
                let ids: Vec<String> = flat.iter().filter_map(|x| x.speaker_id.clone()).collect();
                names = crate::speakers::name_map(&st.pool, &ids)
                    .await
                    .map_err(internal)?;
                retrieve::enrich_for_display(&mut flat, &names, now, tz);
                let groups = retrieve::regroup_sources(&flat, &lens);
                precomputed_answer = Some(
                    st.llm
                        .answer_grouped(&message, &groups, &names)
                        .await
                        .map_err(internal)?,
                );
                sources = flat;
            } else
            // Clip-scoped "who was speaking": a roster question over a bounded window is a SET
            // question — answer it deterministically (distinct speakers heard in the window),
            // not by semantic NN (embedding "who was speaking" retrieves nothing useful; that's
            // exactly the observed "I don't have information" failure). Gated on a bounded
            // window (the deictic anchor above, or explicit after+before filters), so an
            // un-anchored "who was speaking" keeps today's semantic path — zero baseline drift.
            if let (true, Some(after_ns), Some(before_ns)) = (
                crate::routes::is_speaker_roster_query(&message),
                after,
                before,
            ) {
                let s = retrieve::list_speakers_in_window(
                    &st.pool,
                    device_id.as_deref(),
                    after_ns,
                    before_ns,
                    50,
                )
                .await
                .map_err(internal)?;
                let ids: Vec<String> = s.iter().filter_map(|x| x.speaker_id.clone()).collect();
                names = crate::speakers::name_map(&st.pool, &ids)
                    .await
                    .map_err(internal)?;
                let in_clip = crate::routes::is_deictic_video_query(&message);
                let answer = if s.is_empty() {
                    // Distinguish "footage exists but nobody spoke" from "nothing captured /
                    // not yet transcribed for the moment you're watching".
                    let covered = retrieve::window_has_footage(
                        &st.pool,
                        device_id.as_deref(),
                        after_ns,
                        before_ns,
                    )
                    .await
                    .map_err(internal)?;
                    render_empty_roster(covered, in_clip)
                } else {
                    render_speaker_roster(&s, &names, in_clip)
                };
                precomputed_answer = Some(answer);
                sources = s;
            } else {
            // "Tell me about <named voice>": narrate the accumulated running-memory profile
            // (chat-time freshen keeps it exactly as current as the events table), with the
            // voice's recent utterances as citations. Falls through to the semantic path when
            // no single named voice resolves or no profile has accumulated yet.
            let mut speaker_profile: Option<(String, crate::llm::ProfileContext, Uuid)> = None;
            if crate::routes::is_profile_query(&message) {
                let sids = crate::speakers::resolve_names_in_text(&st.pool, &message)
                    .await
                    .map_err(internal)?;
                if sids.len() == 1 {
                    let sid = sids[0];
                    if st.cfg.profile_chat_refresh {
                        let popts = hushai_backend::profiles::ProfileOpts {
                            visit_gap_secs: st.cfg.presence_visit_gap_secs,
                            convo_gap_secs: st.cfg.conversation_gap_secs,
                            grace_secs: st.cfg.profile_grace_secs,
                            ..Default::default()
                        };
                        if let Err(e) = hushai_backend::profiles::refresh_subject(
                            &st.pool,
                            "speaker",
                            sid,
                            &popts,
                        )
                        .await
                        {
                            tracing::warn!(error = %e, "speaker profile refresh failed");
                        }
                    }
                    if let Some(p) = hushai_backend::profiles::get_profile(&st.pool, "speaker", sid)
                        .await
                        .map_err(internal)?
                    {
                        let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
                        let nm = crate::speakers::name_map(&st.pool, &[sid.to_string()])
                            .await
                            .map_err(internal)?;
                        let label = nm
                            .get(&sid.to_string())
                            .cloned()
                            .unwrap_or_else(|| "that voice".to_string());
                        speaker_profile = Some((
                            label,
                            crate::llm::ProfileContext {
                                text: p.profile_text,
                                visit_count: p.visit_count,
                                first_seen_label: p
                                    .first_seen_unix_nanos
                                    .map(|t| crate::humanize::humanize_time(t, now, tz)),
                                last_seen_label: p
                                    .last_seen_unix_nanos
                                    .map(|t| crate::humanize::humanize_time(t, now, tz)),
                            },
                            sid,
                        ));
                    }
                }
            }
            if let Some((label, pc, sid)) = speaker_profile {
                let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
                let mut s = retrieve::list_by_speaker(
                    &st.pool,
                    std::slice::from_ref(&sid.to_string()),
                    device_id.as_deref(),
                    after,
                    before,
                    8,
                )
                .await
                .map_err(internal)?;
                let ids: Vec<String> = s.iter().filter_map(|x| x.speaker_id.clone()).collect();
                names = crate::speakers::name_map(&st.pool, &ids)
                    .await
                    .map_err(internal)?;
                retrieve::enrich_for_display(&mut s, &names, now, tz);
                precomputed_answer = Some(
                    st.llm
                        .answer_profile(&message, &label, &pc, &s, &names)
                        .await
                        .map_err(internal)?,
                );
                sources = s;
            } else {
            let speaker_name = qf.speaker_name.or_else(|| df.speaker_name.clone());
            let speaker_id = resolve_speaker_filter(&st.pool, qf.speaker_id, speaker_name)
                .await
                .map_err(internal)?;
            let top_k = req
                .top_k
                .or(agent.default_top_k)
                .unwrap_or(st.cfg.top_k_default)
                .clamp(1, 50);
            // Route: exhaustive attribution ("everything Bob said") vs topical semantic
            // search — mirrors /v1/rag/query. Exhaustive only when explicitly requested
            // AND a speaker filter resolved; it skips the query embedding entirely.
            let want_exhaustive = req.exhaustive.unwrap_or(false)
                && speaker_id.as_ref().is_some_and(|v| !v.is_empty());
            let mut s = if want_exhaustive {
                retrieve::list_by_speaker(
                    &st.pool,
                    speaker_id.as_deref().unwrap_or_default(),
                    device_id.as_deref(),
                    after,
                    before,
                    top_k.max(50),
                )
                .await
                .map_err(internal)?
            } else {
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
                retrieve::nearest(&st.pool, &embedding, top_k, &tuning, &filters)
                    .await
                    .map_err(internal)?
            };
            // Exhaustive rows carry distance 0.0, so they survive this unchanged.
            s.retain(|x| x.distance <= st.cfg.distance_threshold);
            // Relative-margin prune (see routes.rs): one marginal cross-conversation hit
            // must not drag a whole unrelated conversation into the grounded prompt.
            retrieve::prune_rel_margin(&mut s, st.cfg.prune_rel_margin);
            // Conversation-neighborhood expansion (0025): pruned hits widen into their
            // persisted conversation's surrounding sentences; the stream prompt then
            // renders per-conversation sections so two concurrent conversations can never
            // blend into one answer. Group lengths ride to the stream site — enrichment
            // happens on the FLAT list below, then the groups are rebuilt by length.
            if st.cfg.expand_enabled
                && !want_exhaustive
                && s.iter().any(|x| x.conversation_id.is_some())
            {
                let groups = retrieve::expand_to_conversations(
                    &st.pool,
                    &s,
                    st.cfg.expand_window_secs.saturating_mul(1_000_000_000),
                    st.cfg.expand_max_sentences_per_convo,
                    st.cfg.expand_max_total_chars,
                )
                .await
                .map_err(internal)?;
                convo_group_lens = Some(groups.iter().map(|g| g.len()).collect());
                s = groups.into_iter().flatten().collect();
            }
            let ids: Vec<String> = s.iter().filter_map(|x| x.speaker_id.clone()).collect();
            names = crate::speakers::name_map(&st.pool, &ids)
                .await
                .map_err(internal)?;
            sources = s;
            }
            }
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
                    // Same natural-language window precedence as the People arm.
                    let now_ns = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
                    let parsed = crate::timeparse::window_in_query(&message, now_ns, tz);
                    let after = qf.after_unix_nanos.or(df.after_unix_nanos).or(parsed.map(|(a, _)| a));
                    let before = qf.before_unix_nanos.or(df.before_unix_nanos).or(parsed.map(|(_, b)| b));
                    let top_k = req
                        .top_k
                        .or(agent.default_top_k)
                        .unwrap_or(st.cfg.object_top_k_default)
                        .clamp(1, 50);
                    // F1 (presence aggregation): "how many times / how often did I see a <COCO class>"
                    // is answered from the deterministic exact-class rollup (uncapped count + rhythm),
                    // not by asking the LLM to count a CLIP-retrieved list. Only fires when the query
                    // names a concrete COCO class; open-vocab "when did I see a red mug" still takes the
                    // semantic CLIP path below.
                    let count_class = if crate::presence::is_count_intent(&message) {
                        crate::routes::find_object_class_in_query(&message)
                    } else {
                        None
                    };
                    if let Some(label) = count_class {
                        let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
                        let summary = crate::presence::object_presence(
                            &st.pool,
                            &label,
                            device_id.as_deref(),
                            after,
                            before,
                            tz,
                            st.cfg.presence_visit_gap_secs.max(0) * 1_000_000_000,
                        )
                        .await
                        .map_err(internal)?;
                        let mut s = retrieve::list_by_object_class(
                            &st.pool,
                            std::slice::from_ref(&label),
                            device_id.as_deref(),
                            after,
                            before,
                            top_k,
                        )
                        .await
                        .map_err(internal)?;
                        for src in &mut s {
                            src.time_label =
                                crate::humanize::humanize_time(src.start_unix_nanos, now, tz);
                        }
                        precomputed_answer = Some(crate::presence::render_presence(
                            &summary,
                            &format!("A {label}"),
                            now,
                            tz,
                        ));
                        sources = s;
                    } else {
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
        }
        AgentKind::People => {
            // Person (face) attribution — answered synchronously (precomputed), with its own
            // person-label enrichment (not the speaker enrichment below).
            let device_id = qf.device_id.or_else(|| df.device_id.clone());
            // Natural-language windows ("in the last 10 minutes", "today") count here too —
            // same precedence as the recency branch: explicit filters win, then the parsed
            // phrase, else unbounded. Without this, windowed counts scanned ALL time.
            let now_ns = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
            let parsed = crate::timeparse::window_in_query(&message, now_ns, tz);
            let after = qf.after_unix_nanos.or(df.after_unix_nanos).or(parsed.map(|(a, _)| a));
            let before = qf.before_unix_nanos.or(df.before_unix_nanos).or(parsed.map(|(_, b)| b));
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
                // F1 (presence aggregation): a "how many times / how often / when first-last / what
                // times" question about ONE specific person is answered from a DETERMINISTIC rollup
                // (uncapped COUNT(DISTINCT segment) + first/last + rhythm), not by asking the small
                // LLM to count a top-k-capped sighting list (which undercounts past the cap and
                // miscounts even within it). The sighting list still rides along as citations.
                let distinct = distinct_ids(&pids);
                // "Tell me about <named face>": narrate the accumulated running-memory profile
                // (chat-time freshen keeps it current), with the sightings as citations. Only
                // when exactly one person resolved AND a profile has accumulated; otherwise the
                // normal answer paths below run unchanged.
                let mut person_profile: Option<(String, crate::llm::ProfileContext)> = None;
                if crate::routes::is_profile_query(&message) && distinct.len() == 1 {
                    if let Ok(pid) = Uuid::parse_str(&distinct[0]) {
                        if st.cfg.profile_chat_refresh {
                            let popts = hushai_backend::profiles::ProfileOpts {
                                visit_gap_secs: st.cfg.presence_visit_gap_secs,
                                convo_gap_secs: st.cfg.conversation_gap_secs,
                                grace_secs: st.cfg.profile_grace_secs,
                                ..Default::default()
                            };
                            if let Err(e) = hushai_backend::profiles::refresh_subject(
                                &st.pool,
                                "person",
                                pid,
                                &popts,
                            )
                            .await
                            {
                                tracing::warn!(error = %e, "person profile refresh failed");
                            }
                        }
                        if let Some(p) =
                            hushai_backend::profiles::get_profile(&st.pool, "person", pid)
                                .await
                                .map_err(internal)?
                        {
                            let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
                            let label = names
                                .get(&distinct[0])
                                .cloned()
                                .unwrap_or_else(|| "that person".to_string());
                            person_profile = Some((
                                label,
                                crate::llm::ProfileContext {
                                    text: p.profile_text,
                                    visit_count: p.visit_count,
                                    first_seen_label: p
                                        .first_seen_unix_nanos
                                        .map(|t| crate::humanize::humanize_time(t, now, tz)),
                                    last_seen_label: p
                                        .last_seen_unix_nanos
                                        .map(|t| crate::humanize::humanize_time(t, now, tz)),
                                },
                            ));
                        }
                    }
                }
                if let Some((label, pc)) = person_profile {
                    precomputed_answer = Some(
                        st.llm
                            .answer_profile(&message, &label, &pc, &s, &names)
                            .await
                            .map_err(internal)?,
                    );
                } else
                // "How many PEOPLE did you see" is a DISTINCT-people question over the roster —
                // it must never fall into the single-person frequency rollup below (the observed
                // "Mendel was seen 62 times" answer to "how many people in the last 10 min").
                // The roster sources are one row per distinct person; enumerate them verbatim.
                if crate::routes::is_people_count_query(&message) {
                    let mut seen = std::collections::BTreeSet::new();
                    let mut labels: Vec<String> = Vec::new();
                    for src in &s {
                        let key = src
                            .speaker_id
                            .clone()
                            .unwrap_or_else(|| src.segment_id.to_string());
                        if seen.insert(key) {
                            labels.push(src.speaker_name.clone().unwrap_or_else(|| {
                                "someone we haven't identified yet".to_string()
                            }));
                        }
                    }
                    precomputed_answer = Some(crate::presence::render_people_count(&labels));
                } else if crate::presence::is_count_intent(&message) && distinct.len() == 1 {
                    let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
                    let summary = crate::presence::person_presence(
                        &st.pool,
                        &distinct,
                        device_id.as_deref(),
                        after,
                        before,
                        tz,
                        st.cfg.presence_visit_gap_secs.max(0) * 1_000_000_000,
                    )
                    .await
                    .map_err(internal)?;
                    let label = names
                        .get(&distinct[0])
                        .cloned()
                        .unwrap_or_else(|| "That person".to_string());
                    precomputed_answer =
                        Some(crate::presence::render_presence(&summary, &label, now, tz));
                } else if crate::routes::is_co_occurrence_query(&message) {
                    // "Who/was I with": each source is a co-present person (empty = nobody). The small
                    // LLM sometimes drops one when listing several, so enumerate deterministically.
                    let mut seen = std::collections::BTreeSet::new();
                    let mut who: Vec<String> = Vec::new();
                    for src in &s {
                        if let Some(n) = &src.speaker_name {
                            if seen.insert(n.clone()) {
                                who.push(n.clone());
                            }
                        }
                    }
                    precomputed_answer = Some(crate::presence::render_people_list(&who));
                } else {
                    precomputed_answer = Some(
                        st.llm
                            .answer_people(&message, &s, &names)
                            .await
                            .map_err(internal)?,
                    );
                }
            }
            sources = s;
        }
        AgentKind::Plates => {
            // License-plate attribution — answered synchronously (precomputed), with its own
            // plate-label enrichment (not the speaker enrichment below). Mirrors the People arm but
            // without an owner anchor: no plate filter / no plate token in the message -> no
            // sightings -> the LLM declines.
            let device_id = qf.device_id.or_else(|| df.device_id.clone());
            // Same natural-language window precedence as the People arm.
            let now_ns = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
            let parsed = crate::timeparse::window_in_query(&message, now_ns, tz);
            let after = qf.after_unix_nanos.or(df.after_unix_nanos).or(parsed.map(|(a, _)| a));
            let before = qf.before_unix_nanos.or(df.before_unix_nanos).or(parsed.map(|(_, b)| b));
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
            // F1 (presence aggregation): "how many times / how often did I see plate X" about ONE
            // plate is answered from the deterministic rollup, not the LLM counting a capped list.
            let distinct = distinct_ids(&plate_ids);
            if crate::presence::is_count_intent(&message) && distinct.len() == 1 {
                let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
                let summary = crate::presence::plate_presence(
                    &st.pool,
                    &distinct,
                    device_id.as_deref(),
                    after,
                    before,
                    tz,
                    st.cfg.presence_visit_gap_secs.max(0) * 1_000_000_000,
                )
                .await
                .map_err(internal)?;
                let label = names
                    .get(&distinct[0])
                    .cloned()
                    .unwrap_or_else(|| "That plate".to_string());
                precomputed_answer =
                    Some(crate::presence::render_presence(&summary, &label, now, tz));
            } else {
                precomputed_answer = Some(
                    st.llm
                        .answer_plates(&message, &s, &names)
                        .await
                        .map_err(internal)?,
                );
            }
            sources = s;
        }
        AgentKind::Events => {
            // Timeline of flagged events ("what happened / any alerts"). Optionally narrowed to a lane
            // (person/object/plate/speech) or to alerts, parsed from the message. Count-intent → a
            // DETERMINISTIC count; otherwise the LLM narrates the pre-fetched list (grounded).
            let device_id = qf.device_id.or_else(|| df.device_id.clone());
            // Same natural-language window precedence as the People arm.
            let now_ns = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
            let parsed = crate::timeparse::window_in_query(&message, now_ns, tz);
            let after = qf.after_unix_nanos.or(df.after_unix_nanos).or(parsed.map(|(a, _)| a));
            let before = qf.before_unix_nanos.or(df.before_unix_nanos).or(parsed.map(|(_, b)| b));
            let limit = req.top_k.or(agent.default_top_k).unwrap_or(50).clamp(1, 200);
            let ml = message.to_lowercase();
            let alerts_only = ["alert", "alarm", "unusual", "suspicious"].iter().any(|k| ml.contains(k));
            let subject_type = if alerts_only {
                None
            } else if ["person", "people", "face", "who "].iter().any(|k| ml.contains(k)) {
                Some("person")
            } else if ["plate", "vehicle", "car "].iter().any(|k| ml.contains(k)) {
                Some("plate")
            } else if ["said", "heard", "speech", "talk", "conversation"].iter().any(|k| ml.contains(k)) {
                Some("speaker")
            } else if ["object", "thing"].iter().any(|k| ml.contains(k)) {
                Some("object")
            } else {
                None
            };
            let devices: Vec<String> = device_id.into_iter().collect();
            let mut s = retrieve::list_events(&st.pool, &devices, after, before, subject_type, alerts_only, limit)
                .await
                .map_err(internal)?;
            let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
            for src in &mut s {
                src.time_label = crate::humanize::humanize_time(src.start_unix_nanos, now, tz);
            }
            names = std::collections::HashMap::new();
            if crate::presence::is_count_intent(&message) {
                let n = s.len();
                precomputed_answer = Some(if n == 0 {
                    "Nothing notable was recorded for that period.".to_string()
                } else {
                    format!(
                        "There {} {} notable event{} in the recordings for that period.",
                        if n == 1 { "was" } else { "were" },
                        n,
                        if n == 1 { "" } else { "s" }
                    )
                });
            } else {
                precomputed_answer =
                    Some(st.llm.answer_events(&message, &s).await.map_err(internal)?);
            }
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
        AgentKind::Objects | AgentKind::People | AgentKind::Plates | AgentKind::Events
    ) {
        retrieve::enrich_for_display(
            &mut sources,
            &names,
            Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX),
            tz,
        );
        // Cross-modal: annotate these transcript passages with same-segment vision (who was on
        // camera, objects, plates) so the model can fuse "what was said" with "what was seen".
        // Best-effort + env-gated; only the transcript agents (Grounded/Reflection) reach here.
        if st.cfg.context_vision_enrich_enabled {
            if let Err(e) = crate::context::enrich_sources_with_vision(&st.pool, &mut sources, 3).await {
                tracing::warn!(error = format!("{e:#}"), "vision enrichment skipped");
            }
        }
    }

    // Everything below runs as the SSE body. Pre-stage errors above already returned a
    // clean HTTP status; in-stream failures surface as `error` events.
    let pool = st.pool.clone();
    let llm = st.llm.clone();
    // Build the effective system prompt: persona + (voice) spoken-style suffix + (verified owner)
    // one identity line so the model resolves first-person references without breaking the
    // no-outside-knowledge rule. Only the streaming path uses it, so the owner-name lookup is
    // gated on there being no precomputed answer (a precomputed answer skips generation).
    let mut system_prompt = agent.system_prompt.to_string();
    if precomputed_answer.is_none() {
        if is_voice {
            system_prompt.push_str(crate::agents::SPOKEN_STYLE_SUFFIX);
        }
        if owner_verified {
            if let Some(name) = resolve_owner_name(&st).await.map_err(internal)? {
                system_prompt.push_str(&format!(
                    " You are speaking with {name}, the verified owner of these recordings; \
                     first-person words in their questions (\"I\", \"me\", \"my\") refer to {name}."
                ));
            }
        }
        // Append the system briefing (date, known voices/people, cameras, last activity). The
        // permission clause is co-located so it overrides the persona's strict "only the passages"
        // rule for exactly these world facts — without weakening grounding of what was said/seen.
        // Env-gated; an empty briefing appends nothing (byte-identical to the pre-feature prompt).
        if st.cfg.context_briefing_enabled {
            let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
            let briefing = crate::context::assemble_briefing(&st, tz, now).await;
            if !briefing.trim().is_empty() {
                system_prompt.push_str(&format!(
                    "\n\nThe following facts are reliable, provided by the system. You may use them \
                     for the current date and time, who you are speaking with, and the known people, \
                     voices, and cameras — but keep grounding everything about what was said or seen \
                     in the passages you are given.\nFacts:\n{briefing}"
                ));
            }
        }
    }
    let agent_id_stream = agent_id.clone();
    // The concrete capability the auto-router landed on (or the session's pinned agent). The
    // request `agent_id` may be the synthetic "auto"; this is where the answer actually came from.
    // Surfaced in the `session` event + persisted so both the UI and the eval harness can see it.
    let routed_agent_id = agent.id.to_string();
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
            &json!({ "session_id": session_id, "agent_id": agent_id_stream, "routed_agent_id": routed_agent_id }),
        ));
        yield Ok(sse_event(
            "sources",
            &serde_json::to_value(&sources).unwrap_or_else(|_| json!([])),
        ));

        // Reflection with no resolvable target: stream the setup hint, persist, done.
        if let Some(answer) = precomputed_answer {
            yield Ok(sse_event("token", &json!({ "delta": answer })));
            match insert_message(&pool, session_id, "assistant", &answer, Some(&sources), &routed_agent_id).await {
                Ok(message_id) => yield Ok(sse_event("done", &json!({ "message_id": message_id }))),
                Err(e) => {
                    tracing::error!(error = format!("{e:#}"), "failed to persist rag chat answer (precomputed)");
                    yield Ok(sse_event("error", &json!({ "message": "failed to save the answer" })));
                }
            }
        } else {
            // Rebuild the 0025 conversation groups from the ENRICHED flat sources (hoisted:
            // chat_stream's opaque return type captures its argument lifetimes).
            let convo_groups: Option<Vec<Vec<Source>>> = convo_group_lens
                .as_ref()
                .filter(|lens| lens.len() > 1)
                .map(|lens| crate::retrieve::regroup_sources(&sources, lens));
            // Both branches return different concrete stream types; box to one type.
            let stream_res = if is_reflection {
                llm.reflect_stream(
                    &message,
                    reflection_digest.as_deref().unwrap_or_default(),
                    &sources,
                    &names,
                    &system_prompt,
                    history,
                    reflection_model.as_deref(),
                )
                .await
                .map(StreamExt::boxed)
            } else {
                llm.chat_stream(&message, &sources, &names, &system_prompt, history, convo_groups.as_deref())
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
                            &routed_agent_id,
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

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

fn sse_event(name: &str, data: &serde_json::Value) -> Event {
    Event::default().event(name).data(data.to_string())
}

/// Distinct, sorted ids — used to decide whether a count question is about ONE subject (route to
/// the deterministic presence rollup) vs many (fall back to the LLM/list answer).
fn distinct_ids(ids: &[String]) -> Vec<String> {
    let mut d = ids.to_vec();
    d.sort();
    d.dedup();
    d
}

/// Deterministic answer for a windowed "who was speaking" (one source per distinct speaker,
/// chronological — see `retrieve::list_speakers_in_window`). Labels come from the SAME
/// `assign_unnamed_ordinals` + `display_label` pair that `enrich_for_display` applies to the
/// citations, so the spoken answer and the citation chips always agree ("unidentified
/// speaker 1" in both). A NULL-speaker row (speech with no voiceprint) is reported as
/// unattributed speech, never as a person.
fn render_speaker_roster(
    sources: &[Source],
    names: &std::collections::HashMap<String, String>,
    in_clip: bool,
) -> String {
    let scope = if in_clip { "in this clip" } else { "during that period" };
    let ordinals = crate::speakers::assign_unnamed_ordinals(
        sources.iter().map(|s| s.speaker_id.as_deref()),
        names,
    );
    let mut labels: Vec<String> = Vec::new();
    let mut unattributed = false;
    for s in sources {
        match s.speaker_id.as_deref() {
            None => unattributed = true,
            Some(id) => {
                let label =
                    crate::speakers::display_label(Some(id), names, ordinals.get(id).copied());
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
        }
    }
    if labels.is_empty() {
        return if unattributed {
            format!(
                "Someone was speaking {scope}, but the voice isn't attributed to anyone yet. \
                 You can name it under Voices."
            )
        } else {
            // Callers answer the empty case via `render_empty_roster`; defensive fallback.
            format!("I didn't hear any speech {scope}.")
        };
    }
    let list = match labels.len() {
        1 => labels[0].clone(),
        2 => format!("{} and {}", labels[0], labels[1]),
        n => format!("{}, and {}", labels[..n - 1].join(", "), labels[n - 1]),
    };
    let verb = if labels.len() == 1 { "was" } else { "were" };
    let mut out = format!("{list} {verb} speaking {scope}.");
    if unattributed {
        out.push_str(" There's also some speech that isn't attributed to a known voice yet.");
    }
    out
}

/// The windowed "who was speaking" answer when NO transcript rows exist in the window:
/// footage-with-silence and not-yet-processed read very differently to the user.
fn render_empty_roster(window_has_footage: bool, in_clip: bool) -> String {
    let scope = if in_clip { "in this clip" } else { "during that period" };
    if window_has_footage {
        format!("I didn't hear any speech {scope}.")
    } else {
        format!(
            "I don't have processed audio for {} yet — it may still be uploading or transcribing.",
            if in_clip { "this clip" } else { "that period" }
        )
    }
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

/// The trailing `max_messages` turns (role, content) in chronological order. Turns older
/// than `max_age_secs` are excluded (`0` = no age cutoff): they still exist in the stored
/// transcript, but a resumed stale session must not feed hours-old context to the LLM.
pub async fn load_history(
    pool: &PgPool,
    session_id: Uuid,
    max_messages: i64,
    max_age_secs: i64,
) -> anyhow::Result<Vec<(String, String)>> {
    let rows = sqlx::query(
        "SELECT role, content FROM chat_messages \
         WHERE session_id = $1 \
           AND ($3 <= 0 OR created_at > now() - make_interval(secs => $3::double precision)) \
         ORDER BY seq DESC LIMIT $2",
    )
    .bind(session_id)
    .bind(max_messages.max(0))
    .bind(max_age_secs)
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

#[cfg(test)]
mod tests {
    use super::{render_empty_roster, render_speaker_roster};
    use crate::retrieve::Source;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn src(speaker_id: Option<&str>, t: i64) -> Source {
        Source {
            segment_id: Uuid::nil(),
            device_id: "cam".into(),
            text: "hello".into(),
            start_unix_nanos: t,
            distance: 0.0,
            speaker_id: speaker_id.map(str::to_string),
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
            conversation_id: None,
        }
    }

    fn names(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn roster_names_single_known_speaker() {
        let s = [src(Some("id-mendel"), 1)];
        let n = names(&[("id-mendel", "Mendel")]);
        assert_eq!(
            render_speaker_roster(&s, &n, true),
            "Mendel was speaking in this clip."
        );
    }

    #[test]
    fn roster_mixes_named_and_unnamed_with_matching_ordinals() {
        // The unnamed voice must render as "unidentified speaker 1" — the same label
        // enrich_for_display puts on its citation chip (same ordinal inputs, same order).
        let s = [src(Some("id-mendel"), 1), src(Some("id-stranger"), 2)];
        let n = names(&[("id-mendel", "Mendel")]);
        assert_eq!(
            render_speaker_roster(&s, &n, true),
            "Mendel and unidentified speaker 1 were speaking in this clip."
        );
    }

    #[test]
    fn roster_reports_unattributed_speech_as_speech_not_a_person() {
        let s = [src(None, 1)];
        let out = render_speaker_roster(&s, &HashMap::new(), true);
        assert!(out.contains("isn't attributed"), "got: {out}");
        assert!(!out.contains("unidentified speaker"), "got: {out}");
    }

    #[test]
    fn roster_appends_unattributed_note_alongside_names() {
        let s = [src(Some("id-mendel"), 1), src(None, 2)];
        let n = names(&[("id-mendel", "Mendel")]);
        let out = render_speaker_roster(&s, &n, true);
        assert!(out.starts_with("Mendel was speaking in this clip."), "got: {out}");
        assert!(out.contains("isn't attributed to a known voice"), "got: {out}");
    }

    #[test]
    fn roster_windowed_phrasing_without_deictic_clip() {
        let s = [src(Some("id-mendel"), 1)];
        let n = names(&[("id-mendel", "Mendel")]);
        assert_eq!(
            render_speaker_roster(&s, &n, false),
            "Mendel was speaking during that period."
        );
    }

    #[test]
    fn empty_roster_distinguishes_silence_from_missing_footage() {
        assert_eq!(
            render_empty_roster(true, true),
            "I didn't hear any speech in this clip."
        );
        let lagging = render_empty_roster(false, true);
        assert!(lagging.contains("don't have processed audio"), "got: {lagging}");
    }
}
