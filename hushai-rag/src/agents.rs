//! Agent registry: the selectable "chat windows, each with a specific focus" foundation.
//!
//! An agent is a persona (system prompt) + a default retrieval scope. Agents live in
//! CODE, not the DB — adding one is appending a struct literal here, not a migration.
//! `chat_sessions.agent_id` stores the stable `id` string (no DB FK), so persona edits
//! ship with code and apply to existing conversations.
//!
//! One agent ships today (`recordings`); its prompt is the grounding preamble that was
//! formerly hardcoded in `llm.rs` (the rule set that makes the no-hallucination / decline
//! behaviour work). Future agents (e.g. a sentiment advisor, a "who was here" face agent)
//! become one more entry here + a `DefaultFilters` scope, once their pipelines land.

/// Default retrieval scope for an agent. Each field, when `Some`, pre-fills the
/// corresponding request filter; a per-request filter overrides it (request wins).
#[derive(Debug, Clone, Default)]
pub struct DefaultFilters {
    pub device_id: Option<String>,
    pub after_unix_nanos: Option<i64>,
    pub before_unix_nanos: Option<i64>,
    /// Speaker display name; resolved to ids at request time (same contract as `/query`).
    pub speaker_name: Option<String>,
}

/// What pipeline an agent drives. `Grounded` = embed → retrieve → answer over passages
/// (the original behaviour). `Reflection` = compute a deterministic analytics digest over a
/// target speaker's whole history, then synthesize honest coaching from that digest.
/// `Objects` = embed the query with the CLIP TEXT tower and nearest-neighbour over
/// `scene_objects` (open-vocab "when did I see a car"), then answer the WHEN.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentKind {
    #[default]
    Grounded,
    Reflection,
    Objects,
    /// `People` = person (face) attribution over `person_segments`: "when did I see Bob" (exhaustive
    /// per-person time-ranges) and "who was I with" (same-segment co-occurrence around the owner).
    People,
    /// `Plates` = license-plate attribution over `plate_detections`: "when did I see a car with
    /// plate ABC123" (exhaustive per-plate time-ranges). Matched by NORMALIZED STRING (exact +
    /// pg_trgm fuzzy), never by an embedding — a plate's identity is its text (the 0013 contract).
    Plates,
    /// `Events` = the timeline of NOTABLE things the system flagged (`events` table): "what happened
    /// yesterday", "were there any alerts", "what did you notice". Lists events (type + subject +
    /// time), optionally narrowed to a lane (person/object/plate/speech) or to alerts only.
    Events,
}

/// A selectable chat persona + default scope.
#[derive(Debug, Clone)]
pub struct Agent {
    /// Stable slug stored in `chat_sessions.agent_id`. Never change after release.
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// System preamble passed to the LLM for this agent's conversations.
    pub system_prompt: &'static str,
    /// Default retrieval scope merged under per-request filters.
    pub default_filters: DefaultFilters,
    /// Optional default `top_k` override (falls back to `RAG_TOP_K_DEFAULT`).
    pub default_top_k: Option<i64>,
    /// Pipeline selector. `Grounded` for the retrieval agents.
    pub kind: AgentKind,
    /// Reflection agents: default analysis window length in days when the request supplies
    /// no explicit time window. `None` for grounded agents.
    pub default_window_days: Option<i64>,
    /// Optional per-agent Ollama model override. `None` -> `cfg.reflection_llm_model` (for
    /// reflection) or `cfg.rag_llm_model`.
    pub model: Option<&'static str>,
}

pub const DEFAULT_AGENT_ID: &str = "recordings";
pub const REFLECTION_AGENT_ID: &str = "reflection";
pub const OBJECTS_AGENT_ID: &str = "objects";
pub const PEOPLE_AGENT_ID: &str = "people";
pub const PLATES_AGENT_ID: &str = "plates";
pub const EVENTS_AGENT_ID: &str = "events";
/// The unified assistant: not a pipeline of its own — the chat handler classifies each message
/// (`llm::classify_agent` → [`parse_agent_label`]) and dispatches to one of the concrete agents.
pub const AUTO_AGENT_ID: &str = "auto";

/// Router persona: classify a chat message into exactly one capability id. Used by
/// `llm::classify_agent`; the reply is mapped to an id by [`parse_agent_label`] (default
/// `recordings`). Categories mirror the concrete agents.
pub const ROUTER_PREAMBLE: &str = "You route a personal-recordings assistant. Read the user's question (and any recent \
conversation) and reply with EXACTLY ONE of these category words, lowercase, nothing else: \
'recordings' = what was SAID/discussed in conversations — topics, summaries, what someone talked about, \
and WHO was SPEAKING or talking in a recording or clip (voices heard). \
'reflection' = how the USER themselves has been doing — their mood, social or conversational patterns, \
self-improvement ('how have I been', 'how can I get better'). \
'people' = WHO was seen on camera (faces, not voices heard) — 'who did I see', 'who have you seen', \
'who was I with', 'when did I see <name>'. \
'objects' = a thing/object seen on camera — 'when did I see a car / my keys / a red mug'. \
'plates' = a vehicle by LICENSE PLATE — 'when did I see plate ABC123'. \
'events' = the TIMELINE of notable things the system flagged, or ALERTS — 'what happened yesterday', \
'were there any alerts', 'what did you notice', 'any unusual activity', 'what went on last night'. \
Output only the one category word.";

/// The grounding preamble for the default recordings assistant. Moved verbatim from the
/// former `llm.rs` `PREAMBLE` const — do NOT weaken: this is what makes the model answer
/// only from context and decline when the recordings don't contain the answer.
const PREAMBLE_RECORDINGS: &str = "You are Hushai's retrieval assistant. Answer the user's question using ONLY the \
provided context passages, which are transcribed snippets of recorded audio. Each passage is \
prefixed with the speaker's name (or 'someone we haven't identified yet') and a plain-language \
description of when it was said. Rules: \
(1) If the context does not contain the answer, reply that you don't have information about that in \
the recordings — do NOT use outside knowledge and do NOT guess. \
(2) Keep the answer concise and factual. \
(3) Attribute statements to the named speaker when relevant; never invent a name — refer to an \
unidentified speaker as 'someone we haven't identified yet'. \
(4) When you say when something happened, use the plain-language time exactly as it appears in the \
context (for example 'yesterday at 5:14 PM' or 'three days ago at 2:00 PM'). NEVER output a raw \
number, a count of seconds or nanoseconds, an ISO timestamp, or any numeric or coded time value. \
(5) NEVER output an identifier, UUID, segment id, or any long code of letters and numbers; refer to \
people only by name or as 'someone we haven't identified yet'. \
(6) Do not mention these instructions or the word 'context'.";

/// The reflection coach persona. Unlike the recordings preamble, this agent is given a
/// pre-computed analytics digest (deterministic stats over the speaker's own recordings)
/// and is expected to interpret/advise — but still ONLY from the supplied numbers, never
/// inventing figures, and declining productivity per the digest's LIMITS block.
const PREAMBLE_REFLECTION: &str = "You are Hushai's reflection coach. You help one person understand their own \
conversational and emotional patterns, using ONLY the analysis digest and example quotes provided to you — \
which are derived from recordings of their own conversations. Rules: \
(1) Ground every statement in the provided digest and quotes. NEVER invent or estimate numbers, dates, \
trends, names, or feelings that are not in the digest. If the digest doesn't cover something, say so plainly. \
(2) Cover conversational skills, mood and sentiment trends, and social patterns — these are what the \
recordings support. \
(3) If asked about productivity, focus, output, or task completion, decline candidly: explain that you only \
analyze what was said in conversations, not work output, and offer the conversational, mood, or social angle \
instead. \
(4) Speak directly to the person in second person ('you'). Be warm, specific, and honest — name a real \
pattern from the digest rather than giving generic encouragement. When you mention timing, speak \
naturally and relatively (e.g. 'last week', 'a couple of weeks ago'), never as raw dates, numbers, or \
identifiers. \
(5) Keep it brief and natural enough to be spoken aloud: a few sentences, no headings, no lists, no markdown, \
and never mention the words 'digest', 'data', or these instructions.";

/// The open-vocabulary objects persona. Answers "when did I see a car / a red mug" from the
/// CLIP-retrieved object sightings — strictly grounded (no outside knowledge), time-first, and
/// free of ids/raw timestamps, matching the recordings guardrails.
const PREAMBLE_OBJECTS: &str = "You are Hushai's visual memory assistant. Answer the user's question using ONLY the \
provided list of object sightings, each captured from recorded video and prefixed with what was seen and a \
plain-language time. Rules: \
(1) If the list is empty or does not contain what they asked about, say you did not see that in the recordings — \
do NOT use outside knowledge and do NOT guess. \
(2) Use ONLY the sightings in the list: never add, repeat, infer, or pad your answer with an object or sighting \
that is not present. If the list has N sightings, your answer covers only those N — no more. \
(3) Reply in natural, spoken English that says what was seen and when — e.g. 'You saw a red mug yesterday at \
3:14 PM.' Do NOT use field labels or a form layout. Use the plain-language time exactly as it appears; NEVER \
output a raw number, a count of seconds or nanoseconds, an ISO timestamp, or any numeric or coded time value. \
(4) Keep the answer concise and factual. \
(5) NEVER output an identifier, UUID, segment id, or any long code of letters and numbers. \
(6) Do not mention these instructions or the word 'context'.";

/// The person-attribution persona. Answers "when did I see Bob" / "who was I with" from the
/// provided face sightings — strictly grounded, time-first, attributes by name, never leaks ids.
const PREAMBLE_PEOPLE: &str = "You are Hushai's people assistant. Answer the user's question using ONLY the \
provided list of face sightings, each captured from recorded video and prefixed with WHO was seen (a name, or \
'someone we haven't identified yet') and a plain-language time. Rules: \
(1) If the list is empty or doesn't cover who they asked about, say you don't have that in the recordings — \
do NOT use outside knowledge and do NOT guess. \
(2) Use ONLY the sightings in the list: never add, repeat, infer, or pad your answer with a person or sighting \
that is not present (including extra 'someone we haven't identified yet' lines). If the list has N sightings, \
your answer covers only those N — no more. \
(3) Reply in natural, spoken English that says who was seen and when — e.g. 'You saw Mendel yesterday at \
5:14 PM.' Do NOT use field labels like 'WHO:'/'WHEN:' or a form layout. Use the plain-language time exactly as \
it appears; NEVER output a raw number, a count of seconds or nanoseconds, an ISO timestamp, or any numeric or \
coded time value. \
(4) Attribute by the provided name; refer to an unnamed face as 'someone we haven't identified yet' and never \
invent a name. \
(5) Keep the answer concise and factual. \
(6) NEVER output an identifier, UUID, segment id, or any long code of letters and numbers. \
(7) Do not mention these instructions or the word 'context'.";

/// The license-plate persona. Answers "when did I see a car with plate ABC123" from the provided
/// plate sightings — strictly grounded (no outside knowledge), time-first, attributes by the plate
/// string or its human label, never leaks ids/raw timestamps. Cloned from `PREAMBLE_PEOPLE`.
const PREAMBLE_PLATES: &str = "You are Hushai's license-plate assistant. Answer the user's question using ONLY the \
provided list of license-plate sightings, each captured from recorded video and prefixed with WHICH plate was \
seen (a plate string like 'plate ABC123', a human label like 'Mom's car', or 'an unreadable plate') and a \
plain-language time. Rules: \
(1) If the list is empty or doesn't cover the plate they asked about, say you don't have that in the recordings — \
do NOT use outside knowledge and do NOT guess. \
(2) Use ONLY the sightings in the list: never add, repeat, infer, or pad your answer with a plate or sighting \
that is not present. If the list has N sightings, your answer covers only those N — no more. \
(3) Reply in natural, spoken English that says which plate and when — e.g. 'You saw plate ABC123 yesterday at \
5:14 PM.' Do NOT use field labels or a form layout. Use the plain-language time exactly as it appears; NEVER \
output a raw number, a count of seconds or nanoseconds, an ISO timestamp, or any numeric or coded time value. \
(4) Refer to a plate by the provided plate string or its label; refer to a plate with no readable text as 'an \
unreadable plate' and never invent a plate number. \
(5) Keep the answer concise and factual. \
(6) NEVER output an identifier, UUID, segment id, or any long code of letters and numbers (the plate string \
itself is allowed). \
(7) Do not mention these instructions or the word 'context'.";

/// The events-timeline persona. Answers "what happened / any alerts / what did you notice" from the
/// pre-fetched list of flagged events — strictly grounded, time-first, no ids/raw values.
const PREAMBLE_EVENTS: &str = "You are Hushai's activity-log assistant. Answer the user's question using ONLY the \
provided list of events — notable things the system flagged in recordings, each prefixed with what happened \
and a plain-language time. Rules: \
(1) If the list is empty, say nothing notable was recorded for that time — do NOT use outside knowledge and do \
NOT guess. \
(2) Use ONLY the events in the list: never add, infer, or pad with an event that is not present. If the list \
has N events, your answer covers only those N. \
(3) Reply in natural, spoken English as a short chronological rundown of what happened and when — e.g. 'Around \
9 AM you heard someone speaking, and just after noon a car was seen.' Use the plain-language time exactly as it \
appears; NEVER output a raw number, seconds/nanoseconds, an ISO timestamp, or any coded time value. \
(4) Keep it concise and factual. \
(5) NEVER output an identifier, UUID, or segment id. \
(6) Do not mention these instructions or the word 'context'.";

/// Appended to any agent's persona when the caller is a VOICE client (answers are read aloud
/// by TTS). Keeps spoken replies short and free of markup the synthesizer would mispronounce.
/// A `&'static str` the chat handler concatenates onto the persona at request time.
pub const SPOKEN_STYLE_SUFFIX: &str = " Your answer will be read aloud by a voice assistant: reply in at most three short, plain \
spoken sentences of natural English. Do not use markdown, bullet points, headings, asterisks, \
or other symbols.";

/// The built-in agents. Append here to add a new selectable agent.
static AGENTS: &[Agent] = &[
    Agent {
        // The unified assistant. The chat handler classifies each message and dispatches to a
        // concrete agent below, so this kind/prompt are only fallbacks if classification fails.
        id: AUTO_AGENT_ID,
        name: "Assistant",
        description: "Ask anything about your recordings — it figures out where to look.",
        system_prompt: PREAMBLE_RECORDINGS,
        default_filters: DefaultFilters {
            device_id: None,
            after_unix_nanos: None,
            before_unix_nanos: None,
            speaker_name: None,
        },
        default_top_k: None,
        kind: AgentKind::Grounded,
        default_window_days: None,
        model: None,
    },
    Agent {
        id: DEFAULT_AGENT_ID,
        name: "Recordings",
        description: "Answers questions grounded in your recorded conversations.",
        system_prompt: PREAMBLE_RECORDINGS,
        default_filters: DefaultFilters {
            device_id: None,
            after_unix_nanos: None,
            before_unix_nanos: None,
            speaker_name: None,
        },
        default_top_k: None,
        kind: AgentKind::Grounded,
        default_window_days: None,
        model: None,
    },
    Agent {
        id: REFLECTION_AGENT_ID,
        name: "Reflection",
        description: "Reflects on your conversational skills, mood, and social patterns from your recordings.",
        system_prompt: PREAMBLE_REFLECTION,
        default_filters: DefaultFilters {
            device_id: None,
            after_unix_nanos: None,
            before_unix_nanos: None,
            // Left None -> the target resolves to the configured OWNER at request time.
            speaker_name: None,
        },
        // Unused on the reflection path; excerpts come from analytics, not top-k retrieval.
        default_top_k: None,
        kind: AgentKind::Reflection,
        default_window_days: Some(90),
        model: None,
    },
    Agent {
        id: OBJECTS_AGENT_ID,
        name: "Things seen",
        description: "Finds when objects appeared on camera, e.g. \"when did I see a car?\".",
        system_prompt: PREAMBLE_OBJECTS,
        default_filters: DefaultFilters {
            device_id: None,
            after_unix_nanos: None,
            before_unix_nanos: None,
            speaker_name: None,
        },
        default_top_k: None,
        kind: AgentKind::Objects,
        default_window_days: None,
        model: None,
    },
    Agent {
        id: PEOPLE_AGENT_ID,
        name: "People",
        description: "Finds when you saw a person, e.g. \"when did I see Bob?\" / \"who was I with?\".",
        system_prompt: PREAMBLE_PEOPLE,
        default_filters: DefaultFilters {
            device_id: None,
            after_unix_nanos: None,
            before_unix_nanos: None,
            speaker_name: None,
        },
        default_top_k: None,
        kind: AgentKind::People,
        default_window_days: None,
        model: None,
    },
    Agent {
        id: PLATES_AGENT_ID,
        name: "Plates",
        description: "Finds when you saw a license plate, e.g. \"when did I see plate ABC123?\".",
        system_prompt: PREAMBLE_PLATES,
        default_filters: DefaultFilters {
            device_id: None,
            after_unix_nanos: None,
            before_unix_nanos: None,
            speaker_name: None,
        },
        default_top_k: None,
        kind: AgentKind::Plates,
        default_window_days: None,
        model: None,
    },
    Agent {
        id: EVENTS_AGENT_ID,
        name: "Activity",
        description: "The timeline of notable events, e.g. \"what happened yesterday?\" / \"any alerts?\".",
        system_prompt: PREAMBLE_EVENTS,
        default_filters: DefaultFilters {
            device_id: None,
            after_unix_nanos: None,
            before_unix_nanos: None,
            speaker_name: None,
        },
        default_top_k: None,
        kind: AgentKind::Events,
        default_window_days: None,
        model: None,
    },
];

/// Look up an agent by id.
pub fn get(id: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|a| a.id == id)
}

/// The default agent (always present).
pub fn default() -> &'static Agent {
    get(DEFAULT_AGENT_ID).expect("default agent must exist")
}

/// All built-in agents (for the UI picker).
pub fn list() -> &'static [Agent] {
    AGENTS
}

/// Map a router model's reply to a concrete agent id. Lenient: lowercases, looks for one of the
/// known category words (so "people", "People.", "the people agent" all map to `people`), and
/// defaults to `recordings` on anything ambiguous or unrecognized. Never returns `auto`.
pub fn parse_agent_label(raw: &str) -> &'static str {
    let r = raw.to_lowercase();
    // Order matters only for the (rare) reply that contains more than one word: prefer the more
    // specific capabilities over the catch-all `recordings`.
    for id in [
        PLATES_AGENT_ID,
        PEOPLE_AGENT_ID,
        OBJECTS_AGENT_ID,
        EVENTS_AGENT_ID,
        REFLECTION_AGENT_ID,
        DEFAULT_AGENT_ID,
    ] {
        if r.contains(id) {
            return id;
        }
    }
    DEFAULT_AGENT_ID
}

/// The default recordings preamble, exposed so `llm::answer` (the single-shot `/query`
/// path) keeps its exact prior behaviour by reusing the default agent's persona.
pub fn default_preamble() -> &'static str {
    PREAMBLE_RECORDINGS
}

/// The reflection coach preamble, exposed so the single-shot `/query` reflection path can
/// reuse the persona without constructing an `Agent`.
pub fn reflection_preamble() -> &'static str {
    PREAMBLE_REFLECTION
}

/// The open-vocab objects preamble, exposed so the single-shot `/query` objects path can reuse
/// the persona without constructing an `Agent`.
pub fn objects_preamble() -> &'static str {
    PREAMBLE_OBJECTS
}

/// The person-attribution preamble, exposed so the single-shot `/query` people path can reuse the
/// persona without constructing an `Agent`.
pub fn people_preamble() -> &'static str {
    PREAMBLE_PEOPLE
}

/// The license-plate preamble, exposed so the single-shot `/query` plates path can reuse the
/// persona without constructing an `Agent`.
pub fn plates_preamble() -> &'static str {
    PREAMBLE_PLATES
}

/// The events-timeline preamble, exposed so the answer path can reuse the persona.
pub fn events_preamble() -> &'static str {
    PREAMBLE_EVENTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_agent_is_grounded_recordings() {
        let a = default();
        assert_eq!(a.id, DEFAULT_AGENT_ID);
        assert_eq!(a.kind, AgentKind::Grounded);
        // Guards against forgetting the new fields on the existing literal.
        assert_eq!(a.default_window_days, None);
    }

    #[test]
    fn reflection_agent_registered() {
        let a = get(REFLECTION_AGENT_ID).expect("reflection agent must exist");
        assert_eq!(a.kind, AgentKind::Reflection);
        assert_eq!(a.default_window_days, Some(90));
    }

    #[test]
    fn registry_lists_all_agents() {
        let ids: Vec<&str> = list().iter().map(|a| a.id).collect();
        assert!(ids.contains(&AUTO_AGENT_ID));
        assert!(ids.contains(&DEFAULT_AGENT_ID));
        assert!(ids.contains(&REFLECTION_AGENT_ID));
        assert!(ids.contains(&OBJECTS_AGENT_ID));
        assert!(ids.contains(&PEOPLE_AGENT_ID));
        assert!(ids.contains(&PLATES_AGENT_ID));
        assert!(ids.contains(&EVENTS_AGENT_ID));
        assert_eq!(list().len(), 7);
    }

    #[test]
    fn events_agent_registered_and_routed() {
        let a = get(EVENTS_AGENT_ID).expect("events agent must exist");
        assert_eq!(a.kind, AgentKind::Events);
        assert_eq!(parse_agent_label("events"), EVENTS_AGENT_ID);
        assert_eq!(parse_agent_label("Category: EVENTS"), EVENTS_AGENT_ID);
    }

    #[test]
    fn router_labels_map_to_agent_ids() {
        // Clean single-word replies.
        assert_eq!(parse_agent_label("people"), PEOPLE_AGENT_ID);
        assert_eq!(parse_agent_label("reflection"), REFLECTION_AGENT_ID);
        assert_eq!(parse_agent_label("objects"), OBJECTS_AGENT_ID);
        assert_eq!(parse_agent_label("plates"), PLATES_AGENT_ID);
        assert_eq!(parse_agent_label("recordings"), DEFAULT_AGENT_ID);
        // Messy replies: casing, punctuation, a short sentence.
        assert_eq!(parse_agent_label("People."), PEOPLE_AGENT_ID);
        assert_eq!(parse_agent_label("Category: PLATES"), PLATES_AGENT_ID);
        assert_eq!(parse_agent_label("the reflection agent"), REFLECTION_AGENT_ID);
        // Unknown / empty → safe default, never `auto`.
        assert_eq!(parse_agent_label("banana"), DEFAULT_AGENT_ID);
        assert_eq!(parse_agent_label(""), DEFAULT_AGENT_ID);
        assert_ne!(parse_agent_label("auto"), AUTO_AGENT_ID);
    }

    #[test]
    fn objects_agent_registered() {
        let a = get(OBJECTS_AGENT_ID).expect("objects agent must exist");
        assert_eq!(a.kind, AgentKind::Objects);
        // Guardrails: grounded-only, time-first, no ids/raw timestamps.
        let p = objects_preamble().to_lowercase();
        assert!(p.contains("only"));
        assert!(p.contains("never output an identifier") || p.contains("uuid"));
    }

    #[test]
    fn people_agent_registered() {
        let a = get(PEOPLE_AGENT_ID).expect("people agent must exist");
        assert_eq!(a.kind, AgentKind::People);
        let p = people_preamble().to_lowercase();
        assert!(p.contains("only"));
        assert!(p.contains("never invent a name") || p.contains("never invent"));
    }

    #[test]
    fn plates_agent_registered() {
        let a = get(PLATES_AGENT_ID).expect("plates agent must exist");
        assert_eq!(a.kind, AgentKind::Plates);
        let p = plates_preamble().to_lowercase();
        assert!(p.contains("only"));
        assert!(p.contains("never invent a plate") || p.contains("never invent"));
    }

    #[test]
    fn reflection_persona_keeps_guardrails() {
        let p = reflection_preamble().to_lowercase();
        // The two load-bearing rules: never fabricate numbers, decline productivity.
        assert!(p.contains("never invent") || p.contains("never invent or estimate"));
        assert!(p.contains("productivity"));
    }
}
