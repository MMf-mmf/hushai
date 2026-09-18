//! Grounded answer synthesis via a Rig LLM (local Ollama chat model).
//!
//! The agent is built per request from a stored client (cheap; avoids naming Rig's
//! generic `Agent` type). The preamble (an agent persona, see `agents.rs`) forces the
//! model to answer ONLY from the retrieved context and to decline when the context lacks
//! the answer — this is what makes the negative / no-hallucination case behave.
//!
//! Two paths share `build_prompt` (the grounded context block):
//!   - `answer`: single-shot for `POST /v1/rag/query` (default persona, no history).
//!   - `chat_stream`: multi-turn, token-streaming for `POST /v1/rag/chat` (agent persona +
//!     prior turns as chat history). Retrieval is re-anchored on the latest user message;
//!     prior turns are given to the model only for coreference ("what did he say next?").

use std::collections::HashMap;

use anyhow::{Context, anyhow};
use futures_util::{Stream, StreamExt};
use rig::agent::MultiTurnStreamItem;
use rig::client::CompletionClient;
use rig::completion::{Message, Prompt};
use rig::providers::ollama;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};

use crate::retrieve::Source;

pub struct Llm {
    client: ollama::Client,
    model: String,
    /// Sampling temperature applied to every agent build (0.0 = greedy/deterministic).
    temperature: f64,
    /// Optional Ollama sampling seed, merged into `options.seed`.
    seed: Option<i64>,
}

impl Llm {
    pub fn new(
        ollama_base_url: &str,
        model: &str,
        temperature: f64,
        seed: Option<i64>,
    ) -> anyhow::Result<Self> {
        let client = ollama::Client::builder()
            .api_key(ollama::OllamaApiKey::default())
            .base_url(ollama_base_url)
            .build()
            .with_context(|| format!("building Ollama client for {ollama_base_url}"))?;
        Ok(Self {
            client,
            model: model.to_string(),
            temperature,
            seed,
        })
    }

    /// Apply the deterministic-decode knobs to a freshly-created agent builder. `.temperature()`
    /// maps to `options.temperature`; the seed (when set) rides in `additional_params` and Rig
    /// merges it into `options.seed` (only `think`/`keep_alive` are lifted to top-level, so `seed`
    /// stays a model option). Every agent-build site funnels through here so routing AND answers
    /// share one decode profile.
    fn tune(
        &self,
        b: rig::agent::AgentBuilder<ollama::CompletionModel>,
    ) -> rig::agent::AgentBuilder<ollama::CompletionModel> {
        let b = b.temperature(self.temperature);
        match self.seed {
            Some(seed) => b.additional_params(serde_json::json!({ "seed": seed })),
            None => b,
        }
    }

    /// Route a chat message to one capability for the unified "auto" assistant: returns one of
    /// the agent ids (`recordings`/`reflection`/`people`/`objects`/`plates`). A single cheap
    /// classification call to the same local model; `recent_context` (the last turn or two) lets
    /// follow-ups like "Morgan" after a clarifying question route correctly. Always returns a valid
    /// id (`recordings` on any uncertainty) — see [`crate::agents::parse_agent_label`].
    pub async fn classify_agent(
        &self,
        message: &str,
        recent_context: &str,
    ) -> anyhow::Result<&'static str> {
        let agent = self
            .tune(self.client.agent(&self.model))
            .preamble(crate::agents::ROUTER_PREAMBLE)
            .build();
        let prompt = if recent_context.trim().is_empty() {
            format!("Question: {message}\nCategory:")
        } else {
            format!("Recent conversation:\n{recent_context}\n\nQuestion: {message}\nCategory:")
        };
        let raw = agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("LLM router prompt failed: {e}"))?;
        Ok(crate::agents::parse_agent_label(&raw))
    }

    /// Condense a follow-up message into a STANDALONE query using the recent conversation (flaw F4).
    /// "…and the week before?" after "how many times did I see a chair?" → "how many times did I see
    /// a chair the week before?". Returns the message UNCHANGED when it's already self-contained (or on
    /// any doubt) — this must never distort a clear question. Runs at the shared temp (0 = deterministic).
    pub async fn condense(&self, message: &str, recent_context: &str) -> anyhow::Result<String> {
        if recent_context.trim().is_empty() {
            return Ok(message.to_string());
        }
        let agent = self.tune(self.client.agent(&self.model))
            .preamble(
                "You rewrite the user's LATEST message into a single standalone question for a \
                 personal-recordings search. Carry over any subject (a person, object, or license \
                 plate) and any time frame that the latest message refers to from the recent \
                 conversation. Resolve relative time words (e.g. 'the week before') into an explicit \
                 phrase. If the latest message is ALREADY a complete standalone question, or you are \
                 unsure, return it EXACTLY unchanged. Output ONLY the rewritten question, nothing else.",
            )
            .build();
        let prompt = format!(
            "Recent conversation:\n{recent_context}\n\nLatest message: {message}\n\nStandalone question:"
        );
        match agent.prompt(prompt).await {
            Ok(rewritten) => {
                let r = rewritten.trim().trim_matches('"').trim();
                // Guard against a degenerate/empty/oversized rewrite → fall back to the original.
                if r.is_empty() || r.len() > message.len() + 200 {
                    Ok(message.to_string())
                } else {
                    Ok(r.to_string())
                }
            }
            // Condensation is best-effort: never fail the turn over it.
            Err(_) => Ok(message.to_string()),
        }
    }

    /// Produce a grounded answer for `question` given the retrieved `sources`. `names`
    /// maps speaker-id strings to display names for per-passage attribution. Single-shot
    /// (no history) using the default recordings persona — this is the `/v1/rag/query`
    /// path and is byte-for-byte equivalent to the prior hardcoded-preamble behaviour.
    pub async fn answer(
        &self,
        question: &str,
        sources: &[Source],
        names: &HashMap<String, String>,
    ) -> anyhow::Result<String> {
        let agent = self
            .tune(self.client.agent(&self.model))
            .preamble(crate::agents::default_preamble())
            .build();
        let prompt = build_prompt(question, sources, names);
        agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("LLM prompt failed: {e}"))
    }

    /// [`Llm::answer`] over CONVERSATION-scoped groups (0025): renders per-conversation
    /// sections via [`build_grouped_prompt`] so the model never merges statements across
    /// concurrent conversations. Groups must already be enriched (flat) by the caller.
    pub async fn answer_grouped(
        &self,
        question: &str,
        groups: &[Vec<Source>],
        names: &HashMap<String, String>,
    ) -> anyhow::Result<String> {
        let agent = self
            .tune(self.client.agent(&self.model))
            .preamble(crate::agents::default_preamble())
            .build();
        let prompt = build_grouped_prompt(question, groups, names);
        agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("LLM prompt failed: {e}"))
    }

    /// Multi-turn, token-streaming grounded answer. `system_prompt` is the selected
    /// agent's persona; `history` is the prior conversation (oldest→newest) for
    /// coreference. Returns a stream of answer-text deltas (tool-call / reasoning items
    /// are filtered out — this path registers no tools). The caller accumulates the
    /// deltas for persistence and forwards them as SSE `token` events.
    pub async fn chat_stream(
        &self,
        question: &str,
        sources: &[Source],
        names: &HashMap<String, String>,
        system_prompt: &str,
        history: Vec<Message>,
        groups: Option<&[Vec<Source>]>,
    ) -> anyhow::Result<impl Stream<Item = anyhow::Result<String>> + Send> {
        let agent = self
            .tune(self.client.agent(&self.model))
            .preamble(system_prompt)
            .build();
        // Conversation-scoped grouping (0025): when the retrieval expanded into more than
        // one conversation, render per-conversation sections (never mixing groups);
        // otherwise byte-identical to the flat prompt.
        let prompt = match groups {
            Some(g) if g.iter().filter(|x| !x.is_empty()).count() > 1 => {
                build_grouped_prompt(question, g, names)
            }
            _ => build_prompt(question, sources, names),
        };
        let stream = agent.stream_prompt(prompt).with_history(history).await;
        Ok(stream.filter_map(|item| async move {
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) => {
                    Some(Ok(t.text))
                }
                // Ignore non-text items (final aggregate, reasoning, tool deltas).
                Ok(_) => None,
                Err(e) => Some(Err(anyhow!("LLM stream failed: {e}"))),
            }
        }))
    }

    /// Produce a grounded answer for an OBJECT question ("when did I see a car") from CLIP-retrieved
    /// object sightings. Single-shot, no history, objects persona. Sources carry the object label in
    /// `text` and a humanized `time_label`; there is no speaker attribution.
    pub async fn answer_objects(
        &self,
        question: &str,
        sources: &[Source],
    ) -> anyhow::Result<String> {
        let agent = self
            .tune(self.client.agent(&self.model))
            .preamble(crate::agents::objects_preamble())
            .build();
        let prompt = build_objects_prompt(question, sources);
        agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("LLM objects prompt failed: {e}"))
    }

    /// Produce a grounded answer for an EVENTS question ("what happened yesterday" / "any alerts")
    /// from the pre-fetched event timeline. Single-shot, no history, events persona. Sources carry a
    /// plain-language description in `text` and a humanized `time_label`; no speaker attribution.
    pub async fn answer_events(&self, question: &str, sources: &[Source]) -> anyhow::Result<String> {
        let agent = self
            .tune(self.client.agent(&self.model))
            .preamble(crate::agents::events_preamble())
            .build();
        let prompt = build_events_prompt(question, sources);
        agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("LLM events prompt failed: {e}"))
    }

    /// Produce a grounded answer for a PERSON question ("when did I see Bob" / "who was I with")
    /// from face sightings. Single-shot, people persona. Sources carry the resolved person name in
    /// `speaker_name` + a humanized `time_label` (set by `enrich_for_display`), so the shared
    /// `build_prompt` renders "[1] (Bob, yesterday at 5:14 PM) (seen on camera)".
    pub async fn answer_people(
        &self,
        question: &str,
        sources: &[Source],
        names: &HashMap<String, String>,
    ) -> anyhow::Result<String> {
        let agent = self
            .tune(self.client.agent(&self.model))
            .preamble(crate::agents::people_preamble())
            .build();
        let prompt = build_prompt(question, sources, names);
        agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("LLM people prompt failed: {e}"))
    }

    /// Produce a grounded answer for a license-PLATE question ("when did I see a car with plate
    /// ABC123") from plate sightings. Single-shot, plates persona. Sources carry the resolved plate
    /// label in `speaker_name` ("plate ABC123" / "Mom's car" / "an unreadable plate") + a humanized
    /// `time_label`, so the shared `build_prompt` renders "[1] (plate ABC123, yesterday at 5:14 PM)
    /// (license plate seen on camera)".
    pub async fn answer_plates(
        &self,
        question: &str,
        sources: &[Source],
        names: &HashMap<String, String>,
    ) -> anyhow::Result<String> {
        let agent = self
            .tune(self.client.agent(&self.model))
            .preamble(crate::agents::plates_preamble())
            .build();
        let prompt = build_prompt(question, sources, names);
        agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("LLM plates prompt failed: {e}"))
    }

    /// Produce a grounded SUMMARY of the most recent conversation ("what did we last discuss").
    /// `sources` are the latest conversation's sentences in chronological order (from
    /// `retrieve::latest_conversation`, already run through `enrich_for_display`). Single-shot,
    /// recordings persona — mirrors `answer_people`/`answer_events`.
    pub async fn answer_recency(
        &self,
        question: &str,
        sources: &[Source],
        names: &HashMap<String, String>,
    ) -> anyhow::Result<String> {
        let agent = self
            .tune(self.client.agent(&self.model))
            .preamble(crate::agents::default_preamble())
            .build();
        let prompt = build_conversation_prompt(question, sources, names);
        agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("LLM recency prompt failed: {e}"))
    }

    /// Narrate an entity PROFILE ("tell me about Casey") — the accumulated running-memory
    /// observation log plus recent sightings/passages as citations. Single-shot, recordings
    /// persona; the profile is deterministic context (dates reliable), never a citation.
    pub async fn answer_profile(
        &self,
        question: &str,
        label: &str,
        profile: &ProfileContext,
        sources: &[Source],
        names: &HashMap<String, String>,
    ) -> anyhow::Result<String> {
        let agent = self
            .tune(self.client.agent(&self.model))
            .preamble(crate::agents::default_preamble())
            .build();
        let prompt = build_profile_prompt(question, label, profile, sources, names);
        agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("LLM profile prompt failed: {e}"))
    }

    /// Produce a grounded SUMMARY of ALL of a window's conversations ("what have we spoken
    /// about today"). `convos` are chronological conversations of chronological, enriched
    /// sentences (from `retrieve::conversations_in_window`). Single-shot, recordings persona —
    /// mirrors [`Self::answer_recency`], but over the whole window instead of the latest convo.
    pub async fn answer_window_summary(
        &self,
        question: &str,
        convos: &[Vec<Source>],
        names: &HashMap<String, String>,
    ) -> anyhow::Result<String> {
        let agent = self
            .tune(self.client.agent(&self.model))
            .preamble(crate::agents::default_preamble())
            .build();
        let prompt = build_window_summary_prompt(question, convos, names);
        agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("LLM window-summary prompt failed: {e}"))
    }

    /// Single-shot reflection answer for `POST /v1/rag/query` (reflection persona, no
    /// history). `digest_text` is the pre-rendered analytics digest; `excerpts` are
    /// representative quotes. `model` overrides the default Ollama model (e.g. a larger
    /// model for the harder digest→coaching synthesis); `None` uses the default.
    pub async fn reflect(
        &self,
        question: &str,
        digest_text: &str,
        excerpts: &[Source],
        names: &HashMap<String, String>,
        model: Option<&str>,
    ) -> anyhow::Result<String> {
        let agent = self
            .tune(self.client.agent(model.unwrap_or(&self.model)))
            .preamble(crate::agents::reflection_preamble())
            .build();
        let prompt = build_reflection_prompt(question, digest_text, excerpts, names);
        agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("LLM reflect failed: {e}"))
    }

    /// Streaming reflection answer for `POST /v1/rag/chat`. `system_prompt` is the
    /// reflection agent's persona; `history` is prior turns for coreference. Same
    /// delta-filtering as `chat_stream`.
    pub async fn reflect_stream(
        &self,
        question: &str,
        digest_text: &str,
        excerpts: &[Source],
        names: &HashMap<String, String>,
        system_prompt: &str,
        history: Vec<Message>,
        model: Option<&str>,
    ) -> anyhow::Result<impl Stream<Item = anyhow::Result<String>> + Send> {
        let agent = self
            .tune(self.client.agent(model.unwrap_or(&self.model)))
            .preamble(system_prompt)
            .build();
        let prompt = build_reflection_prompt(question, digest_text, excerpts, names);
        let stream = agent.stream_prompt(prompt).with_history(history).await;
        Ok(stream.filter_map(|item| async move {
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) => {
                    Some(Ok(t.text))
                }
                Ok(_) => None,
                Err(e) => Some(Err(anyhow!("LLM reflect stream failed: {e}"))),
            }
        }))
    }
}

/// Assemble the numbered context block + question. Each passage is prefixed with the
/// resolved speaker name (or "unknown speaker") so the model can attribute quotes. Public
/// for prompt-assembly tests.
pub fn build_prompt(question: &str, sources: &[Source], names: &HashMap<String, String>) -> String {
    if sources.is_empty() {
        return format!(
            "Context passages: (none found)\n\nQuestion: {question}\n\n\
             There are no relevant passages, so state that you don't have information \
             about this in the recordings."
        );
    }
    let mut ctx = String::new();
    for (i, s) in sources.iter().enumerate() {
        // Prefer the enriched display label (always set by `enrich_for_display`, and the
        // place where distinct-unnamed numbering / "unattributed audio" is decided). For any
        // caller that skips enrichment, fall back to the same labeling helper so two distinct
        // unidentified speakers still render distinctly instead of collapsing to one literal.
        let who = s.speaker_name.clone().unwrap_or_else(|| {
            crate::speakers::display_label(s.speaker_id.as_deref(), names, None)
        });
        // Time and identity are pre-humanized in `retrieve::enrich_for_display`, so the
        // model only ever sees natural language here — never a raw timestamp, segment id,
        // or UUID it could echo back. (The real citation handle is the `[i]` index; the
        // segment id still rides along in the serialized `sources` for the UI deep-link.)
        // Same-segment vision context ("on camera: Bob; in view: car") rides along as an em-dash
        // suffix when `context::enrich_sources_with_vision` populated it, so the model can answer
        // "who was there when I said X" without a separate lookup. Absent → byte-identical to before.
        let vis = match &s.visual_context {
            Some(v) if !v.trim().is_empty() => format!(" — {}", v.trim()),
            _ => String::new(),
        };
        if s.time_label.is_empty() {
            ctx.push_str(&format!("[{}] ({}) {}{}\n", i + 1, who, s.text.trim(), vis));
        } else {
            ctx.push_str(&format!(
                "[{}] ({}, {}) {}{}\n",
                i + 1,
                who,
                s.time_label,
                s.text.trim(),
                vis
            ));
        }
    }
    format!("Context passages:\n{ctx}\nQuestion: {question}")
}

/// Assemble the recency prompt: the most recent conversation's sentences (chronological, already
/// enriched with speaker names + `time_label`), framed so the model SUMMARIZES rather than answers
/// a lookup. Reuses `build_prompt`'s exact `[i] (who, time) text` line format so citations render
/// identically, then swaps the trailing instruction for a spoken-summary directive.
pub fn build_conversation_prompt(
    question: &str,
    sources: &[Source],
    names: &HashMap<String, String>,
) -> String {
    if sources.is_empty() {
        return format!(
            "Recent conversation: (none found)\n\nQuestion: {question}\n\n\
             There is no recorded conversation yet, so say you don't have anything in the recordings."
        );
    }
    let mut ctx = String::new();
    for (i, s) in sources.iter().enumerate() {
        let who = s.speaker_name.clone().unwrap_or_else(|| {
            crate::speakers::display_label(s.speaker_id.as_deref(), names, None)
        });
        if s.time_label.is_empty() {
            ctx.push_str(&format!("[{}] ({}) {}\n", i + 1, who, s.text.trim()));
        } else {
            ctx.push_str(&format!(
                "[{}] ({}, {}) {}\n",
                i + 1,
                who,
                s.time_label,
                s.text.trim()
            ));
        }
    }
    format!(
        "The following is the most recent recorded conversation, in the order it was said:\n{ctx}\n\
         Question: {question}\n\n\
         Give a brief, natural spoken summary of what this conversation was about — the main topics \
         and who was involved. Use only what is above."
    )
}

/// The accumulated profile handed to [`Llm::answer_profile`] — pre-humanized (the model never
/// sees raw nanos; first/last labels come from `humanize_time`).
#[derive(Debug, Clone, Default)]
pub struct ProfileContext {
    pub text: String,
    pub visit_count: i64,
    pub first_seen_label: Option<String>,
    pub last_seen_label: Option<String>,
}

/// Assemble the profile prompt: the running-memory observation log first (context, not a
/// citation), then any recent passages in the standard `[i]` format, then a narrate-only-from-
/// the-above instruction. Unlike `build_prompt`, EMPTY sources do not force a decline — the
/// profile itself is the answer material.
pub fn build_profile_prompt(
    question: &str,
    label: &str,
    profile: &ProfileContext,
    sources: &[Source],
    names: &HashMap<String, String>,
) -> String {
    let mut out = format!(
        "Accumulated profile of {label} (built from prior recordings; dates are reliable):\n{}\n",
        profile.text.trim()
    );
    let mut facts: Vec<String> = Vec::new();
    if let Some(f) = &profile.first_seen_label {
        facts.push(format!("first seen {f}"));
    }
    if let Some(l) = &profile.last_seen_label {
        facts.push(format!("most recently {l}"));
    }
    if profile.visit_count > 0 {
        facts.push(format!(
            "{} visit{} in total",
            profile.visit_count,
            if profile.visit_count == 1 { "" } else { "s" }
        ));
    }
    if !facts.is_empty() {
        out.push_str(&format!("({}.)\n", facts.join("; ")));
    }
    if !sources.is_empty() {
        out.push_str("\nRecent passages:\n");
        for (i, s) in sources.iter().enumerate() {
            let who = s.speaker_name.clone().unwrap_or_else(|| {
                crate::speakers::display_label(s.speaker_id.as_deref(), names, None)
            });
            if s.time_label.is_empty() {
                out.push_str(&format!("[{}] ({}) {}\n", i + 1, who, s.text.trim()));
            } else {
                out.push_str(&format!("[{}] ({}, {}) {}\n", i + 1, who, s.time_label, s.text.trim()));
            }
        }
    }
    out.push_str(&format!(
        "\nQuestion: {question}\n\n\
         Give a brief, natural spoken rundown of {label} using ONLY the profile and passages \
         above — when they first and most recently appeared, how often, and anything notable. \
         Do not invent details."
    ));
    out
}

/// Assemble the window-summary prompt: every kept conversation as its own section, with GLOBAL
/// `[i]` numbering continuing across sections — the flattened conversations are exactly the
/// `sources` array the SSE stream carries, so citation indices line up. Reuses `build_prompt`'s
/// `[i] (who, time) text` line format; the section header carries the conversation's start time.
pub fn build_window_summary_prompt(
    question: &str,
    convos: &[Vec<Source>],
    names: &HashMap<String, String>,
) -> String {
    if convos.iter().all(|c| c.is_empty()) {
        return format!(
            "Recorded conversations: (none found)\n\nQuestion: {question}\n\n\
             Nothing was recorded in that period, so say you don't have anything in the recordings for it."
        );
    }
    let mut ctx = String::new();
    let mut i = 0usize;
    for (ci, convo) in convos.iter().enumerate() {
        if convo.is_empty() {
            continue;
        }
        let when = convo
            .first()
            .map(|s| s.time_label.clone())
            .filter(|t| !t.is_empty())
            .map(|t| format!(" ({t})"))
            .unwrap_or_default();
        ctx.push_str(&format!("Conversation {}{}:\n", ci + 1, when));
        for s in convo {
            i += 1;
            let who = s.speaker_name.clone().unwrap_or_else(|| {
                crate::speakers::display_label(s.speaker_id.as_deref(), names, None)
            });
            if s.time_label.is_empty() {
                ctx.push_str(&format!("[{}] ({}) {}\n", i, who, s.text.trim()));
            } else {
                ctx.push_str(&format!("[{}] ({}, {}) {}\n", i, who, s.time_label, s.text.trim()));
            }
        }
        ctx.push('\n');
    }
    format!(
        "The following are the recorded conversations from the period the question asks about, \
         oldest first:\n{ctx}\
         Question: {question}\n\n\
         Give a brief, natural spoken overview of what was discussed — the main topics of each \
         conversation and who was involved. Use only what is above; do not invent details."
    )
}

/// Assemble the grounded prompt over CONVERSATION-scoped groups (0025): each group renders
/// as its own section with a participants header, GLOBAL `[i]` numbering continues across
/// sections (the flattened groups are exactly the `sources` array the SSE carries, so
/// citation indices line up — the window-summary contract), and a trailing instruction
/// forbids combining statements across conversations. A single group degenerates to the
/// flat `build_prompt` layout plus the participants line.
///
/// CALLER CONTRACT: run `enrich_for_display` over the FLAT source list BEFORE grouping —
/// unnamed-speaker ordinals are assigned globally, so "unidentified speaker 1" in two
/// different sections is guaranteed to be the same voice (and two different voices never
/// share a label).
pub fn build_grouped_prompt(
    question: &str,
    groups: &[Vec<Source>],
    names: &HashMap<String, String>,
) -> String {
    let nonempty = groups.iter().filter(|g| !g.is_empty()).count();
    if nonempty == 0 {
        return build_prompt(question, &[], names);
    }
    if nonempty == 1 {
        let flat: Vec<Source> = groups.iter().flatten().cloned().collect();
        return build_prompt(question, &flat, names);
    }
    let mut ctx = String::new();
    let mut i = 0usize;
    let mut section = 0usize;
    for group in groups.iter().filter(|g| !g.is_empty()) {
        // Participants in first-utterance order, deduped on the enriched label.
        let mut participants: Vec<String> = Vec::new();
        for s in group {
            let who = s.speaker_name.clone().unwrap_or_else(|| {
                crate::speakers::display_label(s.speaker_id.as_deref(), names, None)
            });
            if !participants.contains(&who) {
                participants.push(who);
            }
        }
        let when = group
            .first()
            .map(|s| s.time_label.clone())
            .filter(|t| !t.is_empty())
            .map(|t| format!("{t} — "))
            .unwrap_or_default();
        section += 1;
        ctx.push_str(&format!(
            "Conversation {section} ({}{}):\n",
            when,
            participants.join(", ")
        ));
        for s in group {
            i += 1;
            let who = s.speaker_name.clone().unwrap_or_else(|| {
                crate::speakers::display_label(s.speaker_id.as_deref(), names, None)
            });
            let vis = match &s.visual_context {
                Some(v) if !v.trim().is_empty() => format!(" — {}", v.trim()),
                _ => String::new(),
            };
            if s.time_label.is_empty() {
                ctx.push_str(&format!("[{}] ({}) {}{}\n", i, who, s.text.trim(), vis));
            } else {
                ctx.push_str(&format!(
                    "[{}] ({}, {}) {}{}\n",
                    i,
                    who,
                    s.time_label,
                    s.text.trim(),
                    vis
                ));
            }
        }
        ctx.push('\n');
    }
    format!(
        "Context passages, grouped by conversation:\n{ctx}\
         Question: {question}\n\n\
         Each conversation above is a SEPARATE discussion between only the people listed \
         for it. Never combine statements from different conversations into one answer. If \
         the question is about one conversation, ignore the others and say which \
         conversation you are describing."
    )
}

/// Assemble the numbered OBJECT-sightings block + question. Each line is the seen object (the
/// whole-frame `__frame__` rows render as "something in view") and its plain-language time. No
/// speaker attribution; identity/timestamps are pre-humanized so the model never sees raw values.
pub fn build_objects_prompt(question: &str, sources: &[Source]) -> String {
    if sources.is_empty() {
        return format!(
            "Object sightings: (none found)\n\nQuestion: {question}\n\n\
             Nothing matching was seen in the recordings, so say you didn't see that."
        );
    }
    let mut ctx = String::new();
    for (i, s) in sources.iter().enumerate() {
        let what = match s.text.trim() {
            "" | "__frame__" => "something in view",
            other => other,
        };
        if s.time_label.is_empty() {
            ctx.push_str(&format!("[{}] ({})\n", i + 1, what));
        } else {
            ctx.push_str(&format!("[{}] ({}, {})\n", i + 1, what, s.time_label));
        }
    }
    format!("Object sightings:\n{ctx}\nQuestion: {question}")
}

/// Assemble the numbered EVENT-timeline block + question. Each line is a flagged event and its
/// plain-language time; identity/timestamps are pre-humanized so the model never sees raw values.
pub fn build_events_prompt(question: &str, sources: &[Source]) -> String {
    if sources.is_empty() {
        return format!(
            "Events: (none found)\n\nQuestion: {question}\n\n\
             Nothing notable was recorded for that time, so say so plainly."
        );
    }
    let mut ctx = String::new();
    for (i, s) in sources.iter().enumerate() {
        let what = s.text.trim();
        if s.time_label.is_empty() {
            ctx.push_str(&format!("[{}] ({})\n", i + 1, what));
        } else {
            ctx.push_str(&format!("[{}] ({}, {})\n", i + 1, what, s.time_label));
        }
    }
    format!("Events:\n{ctx}\nQuestion: {question}")
}

/// Assemble the reflection prompt: the pre-rendered analytics digest (from
/// `analytics::render_digest`), optional example quotes, and the question. The
/// strict-grounding contract lives in the reflection persona; this only supplies the body.
/// Unattributed excerpts render as "you" — they are the target speaker's own words.
pub fn build_reflection_prompt(
    question: &str,
    digest_text: &str,
    excerpts: &[Source],
    names: &HashMap<String, String>,
) -> String {
    let mut body = format!("Analysis of recent conversations:\n{digest_text}\n");
    if !excerpts.is_empty() {
        body.push_str("\nExample moments from the recordings:\n");
        for (i, s) in excerpts.iter().enumerate() {
            let who = s
                .speaker_id
                .as_ref()
                .and_then(|id| names.get(id))
                .map(String::as_str)
                .unwrap_or("you");
            body.push_str(&format!("[{}] ({}) {}\n", i + 1, who, s.text.trim()));
        }
    }
    format!(
        "{body}\nThe person asks: {question}\n\nRespond using only the analysis and examples above."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn src(text: &str) -> Source {
        Source {
            segment_id: Uuid::now_v7(),
            device_id: "cam-A".into(),
            text: text.into(),
            start_unix_nanos: 42,
            distance: 0.1,
            speaker_id: None,
            // Enrichment normally fills these; the prompt must render the human strings and
            // never the raw segment id / nanoseconds.
            speaker_name: None,
            time_label: "yesterday at 5:14 PM".into(),
            visual_context: None,
            conversation_id: None,
        }
    }

    /// The prompt must carry only human-readable values: no raw nanos, no segment id.
    fn assert_no_machine_values(p: &str) {
        assert!(!p.contains("t="), "leaked raw timestamp marker: {p}");
        assert!(!p.contains("ns)"), "leaked nanosecond marker: {p}");
        assert!(!p.contains("segment "), "leaked segment id: {p}");
    }

    #[test]
    fn prompt_includes_context_and_question() {
        let p = build_prompt(
            "what was said?",
            &[src("the meeting is tuesday")],
            &HashMap::new(),
        );
        assert!(p.contains("the meeting is tuesday"));
        assert!(p.contains("what was said?"));
        assert!(p.contains("[1]"));
        // The humanized time rides along inline.
        assert!(p.contains("yesterday at 5:14 PM"));
        // No speaker_id at all -> the neutral, non-person "unattributed audio" wording.
        assert!(p.contains(crate::speakers::UNATTRIBUTED_AUDIO));
        assert_no_machine_values(&p);
    }

    /// Regression for the "everyone collapses into one unidentified person" bug: two
    /// distinct unnamed speakers must render as two DISTINCT labels (so "who talks the
    /// most" can tell them apart), and a NULL-speaker passage as "unattributed audio".
    #[test]
    fn prompt_distinguishes_distinct_unnamed_speakers() {
        let mut a = src("first person speaking");
        a.speaker_id = Some(Uuid::now_v7().to_string());
        let mut b = src("second person speaking");
        b.speaker_id = Some(Uuid::now_v7().to_string());
        let c = src("nobody attributed"); // speaker_id None
        let mut sources = vec![a, b, c];
        // Enrich with an EMPTY names map (neither unnamed speaker is named).
        crate::retrieve::enrich_for_display(&mut sources, &HashMap::new(), 1_000, 0);
        let p = build_prompt("who talks the most?", &sources, &HashMap::new());
        assert!(p.contains("unidentified speaker 1"));
        assert!(p.contains("unidentified speaker 2"));
        assert!(p.contains(crate::speakers::UNATTRIBUTED_AUDIO));
        assert!(!p.contains("someone we haven't identified yet"));
        assert_no_machine_values(&p);
    }

    #[test]
    fn prompt_attributes_named_speaker() {
        let mut s = src("the meeting is tuesday");
        let id = Uuid::now_v7().to_string();
        s.speaker_id = Some(id.clone());
        let mut names = HashMap::new();
        names.insert(id, "Bob".to_string());
        let p = build_prompt("what did bob say?", &[s], &names);
        assert!(
            p.contains("Bob"),
            "named speaker should appear for attribution"
        );
        assert!(!p.contains("someone we haven't identified yet"));
        assert_no_machine_values(&p);
    }

    #[test]
    fn prompt_prefers_enriched_speaker_name() {
        let mut s = src("the meeting is tuesday");
        s.speaker_name = Some("Alice".into());
        // No names map needed: the enriched name wins.
        let p = build_prompt("who?", &[s], &HashMap::new());
        assert!(p.contains("Alice"));
        assert!(!p.contains("someone we haven't identified yet"));
        assert_no_machine_values(&p);
    }

    #[test]
    fn empty_sources_instructs_decline() {
        let p = build_prompt("anything?", &[], &HashMap::new());
        assert!(p.to_lowercase().contains("don't have information"));
        assert!(p.contains("anything?"));
    }

    #[test]
    fn conversation_prompt_summarizes_in_order() {
        let mut a = src("morning plan");
        a.speaker_name = Some("Morgan".into());
        a.time_label = "today at 9:00 AM".into();
        let mut b = src("sounds good");
        b.speaker_name = Some("Sarah".into());
        b.time_label = "today at 9:01 AM".into();
        let p = build_conversation_prompt("what did we last discuss?", &[a, b], &HashMap::new());
        assert!(p.contains("most recent recorded conversation"));
        assert!(p.contains("[1] (Morgan, today at 9:00 AM) morning plan"));
        assert!(p.contains("[2] (Sarah, today at 9:01 AM) sounds good"));
        assert!(p.to_lowercase().contains("summary"));
        assert_no_machine_values(&p);
    }

    #[test]
    fn conversation_prompt_empty_declines() {
        let p = build_conversation_prompt("what did we last discuss?", &[], &HashMap::new());
        assert!(p.to_lowercase().contains("don't have"));
    }

    #[test]
    fn build_prompt_renders_visual_context_suffix() {
        let mut s = src("let's meet tuesday");
        s.speaker_name = Some("Morgan".into());
        s.visual_context = Some("on camera: Bob; in view: laptop".into());
        let p = build_prompt("what was said?", &[s], &HashMap::new());
        assert!(p.contains("let's meet tuesday — on camera: Bob; in view: laptop"));
        assert_no_machine_values(&p);
    }

    #[test]
    fn objects_prompt_renders_label_and_time() {
        let mut s = src("car");
        s.time_label = "yesterday at 3:14 PM".into();
        let p = build_objects_prompt("when did i see a car?", &[s]);
        assert!(p.contains("[1] (car, yesterday at 3:14 PM)"));
        assert!(p.contains("when did i see a car?"));
        assert_no_machine_values(&p);
    }

    #[test]
    fn objects_prompt_frame_rows_render_generically() {
        let mut s = src("__frame__");
        s.time_label = "this morning at 9:00 AM".into();
        let p = build_objects_prompt("a red mug?", &[s]);
        assert!(p.contains("something in view"));
        assert!(!p.contains("__frame__"));
    }

    #[test]
    fn objects_prompt_empty_instructs_decline() {
        let p = build_objects_prompt("a unicorn?", &[]);
        assert!(p.to_lowercase().contains("didn't see that"));
    }

    #[test]
    fn reflection_prompt_includes_digest_and_question() {
        let p = build_reflection_prompt(
            "how have i been?",
            "TALK BALANCE: you spoke 38% of conversation time.",
            &[],
            &HashMap::new(),
        );
        assert!(p.contains("TALK BALANCE: you spoke 38%"));
        assert!(p.contains("how have i been?"));
        // No excerpts -> no example block.
        assert!(!p.contains("Example moments"));
    }

    #[test]
    fn reflection_prompt_attributes_excerpt_as_you() {
        let p = build_reflection_prompt(
            "what can i improve?",
            "MOOD (you): 31% positive.",
            &[src("i think the demo went well")],
            &HashMap::new(),
        );
        assert!(p.contains("Example moments"));
        assert!(p.contains("[1] (you) i think the demo went well"));
    }
}
