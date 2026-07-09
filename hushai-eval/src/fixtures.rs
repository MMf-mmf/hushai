//! Fixture + ground-truth format.
//!
//! A test case is a directory `fixtures/<split>/<case-id>/` containing:
//!   - the media file (e.g. `media.mp4` / `media.wav`)
//!   - `meta.json`     — how to inject + which modalities to score
//!   - `expected.json` — ground truth (every modality key is independently optional)
//!   - optional `refs/` — reference assets for identity enrollment
//!
//! All ground-truth time windows are expressed as nanosecond OFFSETS from
//! `meta.base_capture_unix_nanos`, so a single base shift relocates the whole fixture in the
//! timeline without rewriting ground truth. Scorers add the base before querying.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

// ----- meta.json -------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Meta {
    pub case_id: String,
    #[serde(default)]
    pub description: String,
    pub device_id: String,
    pub media_file: String,
    /// "audio" | "video" | "muxed" — only affects which lane(s) process the segments.
    #[serde(default = "d_muxed")]
    pub media_kind: String,
    #[serde(default = "d_seg_seconds")]
    pub seg_seconds: u32,
    #[serde(default)]
    pub limit: Option<u32>,
    /// FIXED capture-start so timestamps (and 30s event buckets) are deterministic.
    pub base_capture_unix_nanos: i64,
    /// Deterministic id seed; defaults to `case_id` when absent.
    #[serde(default)]
    pub segment_id_seed: Option<String>,
    /// Which lanes to poll + score. e.g. ["transcript","speakers","sentiment","events"].
    pub modalities: Vec<String>,
    /// "fast" | "full" — speed-tier membership.
    #[serde(default = "d_full")]
    pub tier: String,
    /// Per-case worker-config overrides (folded into the config-hash; informational here).
    #[serde(default)]
    pub config: serde_json::Map<String, serde_json::Value>,
    /// Reference-identity enrollment performed after reset, before injection.
    #[serde(default)]
    pub enroll: Vec<EnrollSpec>,
    /// Optional multi-clip timeline. When NON-EMPTY it SUPERSEDES `media_file`: the harness injects
    /// each spec in order at its own pinned capture-start, so one case can stage the same
    /// voice/face/plate across hours/days AND across multiple cameras. Empty (default) => the
    /// existing single `media_file` path, byte-for-byte unchanged.
    #[serde(default)]
    pub injections: Vec<InjectionSpec>,
    #[serde(default)]
    pub poll: PollSpec,
}

impl Meta {
    /// Build an in-memory Meta for the `probe` flow (no meta.json on disk yet).
    pub fn synthetic(device_id: &str, case_id: &str, media_kind: &str, base_ns: i64, modalities: Vec<String>) -> Self {
        Meta {
            case_id: case_id.into(),
            description: "probe".into(),
            device_id: device_id.into(),
            media_file: "media.mp4".into(),
            media_kind: media_kind.into(),
            seg_seconds: 2,
            limit: None,
            base_capture_unix_nanos: base_ns,
            segment_id_seed: Some(case_id.into()),
            modalities,
            tier: "full".into(),
            config: serde_json::Map::new(),
            enroll: vec![],
            injections: vec![],
            poll: PollSpec::default(),
        }
    }

    pub fn seed(&self) -> String {
        self.segment_id_seed.clone().unwrap_or_else(|| self.case_id.clone())
    }
    pub fn modality(&self, name: &str) -> bool {
        self.modalities.iter().any(|m| m == name)
    }
    /// True if any scored modality requires the vision lane. `chat` counts: RAG answers ride
    /// on whatever the pipeline perceived, so a chat-only fixture must still wait for the
    /// lanes to drain (a chat-only fixture used to wait on NOTHING — the poller quiesced at 0
    /// processed and the case went inconclusive with `audio_done=0 injected=N`).
    pub fn needs_vision(&self) -> bool {
        ["persons", "faces", "objects", "plates"].iter().any(|m| self.modality(m)) || self.needs_rag() || self.needs_graph()
    }
    /// True if any scored modality requires the audio lane (see `needs_vision` on `chat`).
    /// `conversations` rides on transcript_sentences, so it waits on the audio lane too.
    pub fn needs_audio(&self) -> bool {
        ["transcript", "speakers", "sentiment", "conversations"].iter().any(|m| self.modality(m)) || self.needs_rag() || self.needs_graph()
    }

    /// True if this case scores the Gotham entity graph (the `graph` modality). The graph is FOLDED
    /// from the pipeline's `events` (0014) + CLOSED `conversations` (0025), so a graph fixture must
    /// wait on BOTH producing lanes AND on the graph inputs settling before an authoritative rebuild
    /// (see `poll::wait_graph_inputs_settled` + `query::trigger_graph_rebuild`).
    pub fn needs_graph(&self) -> bool {
        self.modality("graph")
    }

    /// True if this case scores live RAG answers (the `chat` / `rag` modality).
    pub fn needs_rag(&self) -> bool {
        self.modality("chat") || self.modality("rag")
    }

    /// True if this case scores the live advisor service (the `advisor` modality). Advisor cases
    /// are SERVICE-level scripted conversations grounded in the pre-ingested book corpus — no
    /// media injection, no lane polling (lib.rs branches before the media pipeline). Standalone:
    /// don't mix `advisor` with media modalities in one fixture.
    pub fn needs_advisor(&self) -> bool {
        self.modality("advisor")
    }

    /// The concrete list of clips to inject, in order. A single, fully-resolved plan whether the
    /// fixture is legacy single-clip (`injections` empty) or multi-clip. Legacy resolves to EXACTLY
    /// today's parameters (same device, base, seg_seconds, seed, label) so existing baselines are
    /// untouched; multi-clip specs inherit meta defaults and get an index-namespaced seed/label.
    pub fn effective_injections(&self) -> Vec<ResolvedInjection> {
        let base = self.base_capture_unix_nanos;
        if self.injections.is_empty() {
            return vec![ResolvedInjection {
                media_file: self.media_file.clone(),
                device_id: self.device_id.clone(),
                base_ns: base,
                seg_seconds: self.seg_seconds,
                limit: self.limit,
                seed: self.seed(),
                label: self.case_id.clone(),
            }];
        }
        self.injections
            .iter()
            .enumerate()
            .map(|(i, s)| ResolvedInjection {
                media_file: s.media_file.clone(),
                device_id: s.device_id.clone().unwrap_or_else(|| self.device_id.clone()),
                base_ns: s.resolved_base_ns(base),
                seg_seconds: s.seg_seconds.unwrap_or(self.seg_seconds),
                limit: s.limit.or(self.limit),
                seed: s.seed.clone().unwrap_or_else(|| format!("{}::inj{i}", self.seed())),
                label: format!("{}-inj{i}", self.case_id),
            })
            .collect()
    }
}

/// One clip in a multi-clip timeline (see [`Meta::injections`]). Capture-start is a nanosecond
/// OFFSET from `meta.base_capture_unix_nanos` (preferred — a single base shift relocates the whole
/// scenario), OR an absolute pin. `device_id`/`seg_seconds`/`limit`/`seed` fall back to meta.
#[derive(Debug, Clone, Deserialize)]
pub struct InjectionSpec {
    pub media_file: String,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub capture_start_offset_ns: Option<i64>,
    #[serde(default)]
    pub absolute_capture_unix_nanos: Option<i64>,
    #[serde(default)]
    pub seg_seconds: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub seed: Option<String>,
}

impl InjectionSpec {
    /// Absolute capture-start: the absolute pin wins; else `meta_base + offset` (offset defaults 0).
    pub fn resolved_base_ns(&self, meta_base: i64) -> i64 {
        self.absolute_capture_unix_nanos
            .unwrap_or_else(|| meta_base + self.capture_start_offset_ns.unwrap_or(0))
    }
}

/// A fully-resolved injection (meta defaults folded in). Built by [`Meta::effective_injections`].
#[derive(Debug, Clone)]
pub struct ResolvedInjection {
    pub media_file: String,
    pub device_id: String,
    pub base_ns: i64,
    pub seg_seconds: u32,
    pub limit: Option<u32>,
    pub seed: String,
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnrollSpec {
    /// "speaker" | "person" | "plate"
    pub modality: String,
    pub name: String,
    /// Path (relative to the case dir) of a clip to inject-then-rename.
    #[serde(default)]
    pub r#ref: Option<String>,
    /// For plates: directly seed the catalog with this normalized string (no clip).
    #[serde(default)]
    pub plate_text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PollSpec {
    #[serde(default = "d_poll_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "d_poll_interval")]
    pub interval_secs: u64,
    /// Consecutive polls with unchanged event count before declaring quiescence.
    #[serde(default = "d_quiesce_polls")]
    pub quiesce_polls: u32,
}
impl Default for PollSpec {
    fn default() -> Self {
        Self { timeout_secs: d_poll_timeout(), interval_secs: d_poll_interval(), quiesce_polls: d_quiesce_polls() }
    }
}

// ----- expected.json ---------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Expected {
    pub transcript: Option<TranscriptGt>,
    pub speakers: Option<SpeakersGt>,
    pub conversations: Option<ConversationsGt>,
    pub sentiment: Option<SentimentGt>,
    pub persons: Option<PersonsGt>,
    pub objects: Option<ObjectsGt>,
    pub plates: Option<PlatesGt>,
    pub events: Option<EventsGt>,
    /// Live RAG-chat ground truth (scored only when the `chat`/`rag` modality is listed). Accepts
    /// either `"chat"` or `"rag"` as the JSON key.
    #[serde(default, alias = "rag")]
    pub chat: Option<ChatGt>,
    /// Live advisor ground truth (scored only when the `advisor` modality is listed).
    pub advisor: Option<AdvisorGt>,
    /// Entity-graph ground truth (scored only when the `graph` modality is listed; Gotham G1).
    pub graph: Option<GraphGt>,
}

// ----- chat / rag ground truth -----------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ChatGt {
    pub questions: Vec<ChatQ>,
    /// When set, answer-vs-reference cosine similarity is a FLOORED metric (gates); otherwise it's
    /// Info-only. Keep it high enough to catch a wrong answer but not so tight it flakes on phrasing.
    #[serde(default)]
    pub similarity_floor: Option<f64>,
    /// Run an LLM-as-judge per question (rubric-scored). ALWAYS Info-only — never gates the verdict,
    /// so its run-to-run wobble can't flip a pass/fail. A dashboard signal for the agent loop.
    #[serde(default)]
    pub judge_enabled: bool,
}

/// One question fired at the live RAG chat + its assertions. Deterministic-first: the `must_*` /
/// `expect_*` / `min_citations` / `citation_must_attribute` checks test FACTS + STRUCTURE and are
/// robust to LLM wording; `reference_answer` (cosine) + `judge_rubric` are soft signals.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatQ {
    pub ask: String,
    /// `"auto"` (default) exercises the product's auto-router; a concrete id pins one capability.
    #[serde(default = "d_auto")]
    pub agent_id: String,
    #[serde(default)]
    pub filters: Option<ChatFilters>,
    /// Simulated viewer playback state (the browser sends this on every turn): the on-screen
    /// camera + playhead. Exercises the deictic clip anchor ("who was speaking in this clip").
    #[serde(default)]
    pub playback: Option<ChatPlayback>,
    /// Simulated caller context (the voice client sends this): `{kind, owner_verified}`. Exercises
    /// the spoken-style suffix, the owner prompt line, and the deterministic "what's my name" answer.
    #[serde(default)]
    pub caller: Option<ChatCaller>,
    #[serde(default)]
    pub top_k: Option<i64>,
    /// Session label. Questions sharing a label share ONE `/v1/rag/chat` session: the first
    /// labeled question opens it (no `session_id` sent) and the harness threads the returned
    /// `session_id` into every later question with the same label — multi-turn coreference,
    /// condensation, and history are exercised for real. Distinct labels are guaranteed-distinct
    /// sessions (cross-session isolation negatives). Omitted (default) = stateless single-shot,
    /// byte-identical to the pre-session harness. Labeled questions rely on list ORDER (the
    /// opener must come first) — and metric keys are position-indexed anyway: never reorder
    /// existing questions, only append.
    #[serde(default)]
    pub session: Option<String>,

    // ---- deterministic assertions ----
    /// Every listed string must appear (normalized substring) in the answer.
    #[serde(default)]
    pub must_contain: Vec<String>,
    /// AT LEAST ONE of these must appear — for assertions whose correct surface form varies
    /// (e.g. a decline phrased "don't have" / "do not have" / "no information"). Use
    /// `must_contain` for content words; this for phrasing-class checks.
    #[serde(default)]
    pub must_contain_any: Vec<String>,
    /// None of these may appear (hallucination / decline markers).
    #[serde(default)]
    pub must_not_contain: Vec<String>,
    /// A count that must appear in the answer as digits OR an English number-word.
    #[serde(default)]
    pub expect_number: Option<i64>,
    /// The concrete agent the auto-router SHOULD land on (e.g. "people"/"plates"/"objects"/
    /// "reflection"/"recordings"). Scored against the SSE `routed_agent_id`.
    #[serde(default)]
    pub expect_routed_agent: Option<String>,
    /// The returned `sources` array must have at least this many entries.
    #[serde(default)]
    pub min_citations: Option<i64>,
    /// Each listed name must appear across the returned sources' `speaker_name` (normalized).
    #[serde(default)]
    pub citation_must_attribute: Vec<String>,
    /// All cited segments' `conversation_id`s must collapse to exactly ONE non-NULL id (the
    /// answer stayed inside a single threaded conversation). Degrades to Info when EVERY cited
    /// segment is unthreaded (NULL) — the `routed` pattern for a threader that hasn't run.
    #[serde(default)]
    pub citations_single_conversation: bool,
    /// Additionally: that single id must equal the dominant observed id of this GT conversation
    /// label (needs `expected.conversations.utterances` for the label matching).
    #[serde(default)]
    pub citation_conversation_label: Option<String>,

    // ---- soft signals ----
    #[serde(default)]
    pub reference_answer: Option<String>,
    #[serde(default)]
    pub judge_rubric: Option<String>,
}

/// Chat request filters. Time bounds are OFFSETS from `base_capture_unix_nanos` (the query step
/// adds the base), identical to every other GT window in this crate.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatFilters {
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub after_offset_ns: Option<i64>,
    #[serde(default)]
    pub before_offset_ns: Option<i64>,
    #[serde(default)]
    pub speaker_name: Option<String>,
    #[serde(default)]
    pub person_name: Option<String>,
    #[serde(default)]
    pub plate_text: Option<String>,
}

/// Simulated viewer playback context. `playhead_offset_ns` is an OFFSET from
/// `base_capture_unix_nanos` (the query step adds the base), identical to `ChatFilters` bounds.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatPlayback {
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub playhead_offset_ns: Option<i64>,
}

/// Simulated caller context (the voice client's `caller` block). CONTEXT, not a filter.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatCaller {
    /// e.g. "voice" — a spoken client whose answers are read aloud.
    #[serde(default)]
    pub kind: Option<String>,
    /// The on-device owner voice check passed for this turn.
    #[serde(default)]
    pub owner_verified: bool,
}

// ----- advisor ground truth ----------------------------------------------------

/// Scripted advisor conversation (the `advisor` modality): each turn fires one message at the
/// live hushai-advisor's `POST /v1/advisor/chat`; ALL turns thread ONE session (the first turn's
/// `session` SSE event mints it, the harness threads the id into every later turn — the gate's
/// follow-up rounds and the memory layer are exercised for real). Advisor fixtures are service-
/// level: no media is injected, and the corpus (`book_chunks`) is probed before querying so an
/// empty/unmigrated corpus is INCONCLUSIVE ("run ingest-book"), never a false FAIL.
#[derive(Debug, Clone, Deserialize)]
pub struct AdvisorGt {
    pub turns: Vec<AdvisorTurn>,
}

/// One scripted turn + its assertions. Every assertion is optional (a turn may exist purely to
/// feed the gate more context). Deterministic-first, like `ChatQ`: the checks test STRUCTURE
/// (did a follow-up round fire, what grounded the answer) and FACTS (substrings), never prose
/// shape. Metric keys are position-indexed (`advisor.t{i}.*`) so baselines line up — never
/// reorder a fixture's turns once a baseline exists, only append.
#[derive(Debug, Clone, Deserialize)]
pub struct AdvisorTurn {
    pub message: String,
    /// A `questions` follow-up round must (true) / must not (false) fire this turn.
    #[serde(default)]
    pub expect_questions: Option<bool>,
    /// This turn must (true) / must not (false) end in a streamed final answer (token text).
    /// Stronger than "answer non-empty is nice": true asserts the gate stopped asking and
    /// actually answered; false asserts a questions turn streamed NO answer text.
    #[serde(default)]
    pub expect_final_answer: Option<bool>,
    /// The FINAL grounding (LAST `chapters` event — refine iterations may emit several) must
    /// contain AT LEAST ONE of these chapter numbers.
    #[serde(default)]
    pub expect_chapters_any: Vec<i64>,
    /// The final grounding must contain ALL of these chapter numbers.
    #[serde(default)]
    pub expect_chapters_all: Vec<i64>,
    /// Case-insensitive (normalized) substrings that must each appear in the final answer text.
    #[serde(default)]
    pub expect_substrings: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TranscriptGt {
    pub full_text: String,
    #[serde(default = "d_max_wer")]
    pub max_wer: f64,
    #[serde(default = "d_min_sim")]
    pub min_similarity: f64,
    #[serde(default)]
    pub windows: Vec<TextWindow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TextWindow {
    pub start_ns: i64,
    pub end_ns: i64,
    #[serde(default)]
    pub contains: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpeakersGt {
    pub distinct_count: i64,
    #[serde(default)]
    pub count_tolerance: i64,
    #[serde(default)]
    pub utterances: Vec<UttGt>,
    #[serde(default)]
    pub named: Vec<NamedGt>,
    #[serde(default = "d_min_purity")]
    pub min_purity: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UttGt {
    pub label: String,
    pub text_contains: String,
    pub window_ns: [i64; 2],
}

#[derive(Debug, Clone, Deserialize)]
pub struct NamedGt {
    pub label: String,
    pub expect_display_name: String,
}

/// Conversation-threading ground truth (migration 0025: `transcript_sentences.conversation_id`,
/// assigned by the worker's batch threader). Assignment-invariant like `SpeakersGt`: metrics never
/// key on minted conversation UUIDs — labels map to observed ids via dominant-id/optimal-assignment.
#[derive(Debug, Clone, Deserialize)]
pub struct ConversationsGt {
    pub distinct_count: i64,
    #[serde(default)]
    pub count_tolerance: i64,
    #[serde(default)]
    pub utterances: Vec<ConvUttGt>,
    #[serde(default = "d_min_conv")]
    pub min_pairwise_f1: f64,
    #[serde(default = "d_min_conv")]
    pub min_coverage: f64,
    /// Label pairs that must NEVER share an observed conversation id (disentanglement negatives).
    #[serde(default)]
    pub must_not_merge: Vec<[String; 2]>,
    /// Label pairs whose dominant observed ids must be equal and non-NULL (threading positives).
    #[serde(default)]
    pub must_merge: Vec<[String; 2]>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConvUttGt {
    pub label: String,
    pub text_contains: String,
    pub window_ns: [i64; 2],
}

#[derive(Debug, Clone, Deserialize)]
pub struct SentimentGt {
    pub windows: Vec<SentWindow>,
    #[serde(default = "d_min_acc")]
    pub min_accuracy: f64,
    #[serde(default = "d_true")]
    pub allow_null: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SentWindow {
    pub start_ns: i64,
    pub end_ns: i64,
    pub label: String,
    #[serde(default)]
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PersonsGt {
    pub distinct_count: i64,
    #[serde(default)]
    pub count_tolerance: i64,
    #[serde(default = "d_one")]
    pub min_sightings: i64,
    #[serde(default)]
    pub named: Vec<PersonNamedGt>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PersonNamedGt {
    pub expect_display_name: String,
    #[serde(default = "d_one")]
    pub min_sightings: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObjectsGt {
    pub windows: Vec<ObjWindow>,
    #[serde(default = "d_min_f1")]
    pub min_label_f1: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObjWindow {
    pub start_ns: i64,
    pub end_ns: i64,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlatesGt {
    pub expected: Vec<PlateGt>,
    #[serde(default)]
    pub require_exact: bool,
    #[serde(default = "d_one")]
    pub max_norm_edit_distance: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlateGt {
    pub text: String,
    #[serde(default)]
    pub text_norm: Option<String>,
    #[serde(default = "d_one")]
    pub min_reads: i64,
    #[serde(default)]
    pub expect_display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventsGt {
    pub expected: Vec<EventGt>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventGt {
    pub event_type: String,
    #[serde(default)]
    pub subject_type: Option<String>,
    #[serde(default)]
    pub subject_label: Option<String>,
    #[serde(default = "d_one")]
    pub min_count: i64,
    #[serde(default)]
    pub max_count: Option<i64>,
    #[serde(default = "d_info")]
    pub min_severity: String,
}

// ----- graph ground truth (Gotham G1) ----------------------------------------

/// Entity-graph ground truth (scored only when the `graph` modality is listed). ASSIGNMENT-INVARIANT
/// like every identity modality: edges are asserted by DENORMALIZED names (enrolled `display_name`s)
/// / device_ids via [`EntityRef`], never by minted UUIDs — the scorer resolves the name to its
/// catalog id at query time (the `clip_speaker_roster` `enroll:` precedent). Same clips + pinned
/// timestamps + locked `GRAPH_*` knobs ⇒ byte-identical edges (Gotham.md §3.1).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct GraphGt {
    /// `expect_entity`: each must resolve to a live catalog id (enrollment + re-identification worked).
    #[serde(default)]
    pub entities: Vec<EntityRef>,
    /// `expect_edge`: each must be present with `>= min_evidence` observations (and, for a binding,
    /// the required `status` when set).
    #[serde(default)]
    pub edges: Vec<EdgeExpect>,
    /// `expect_no_edge`: counter-assertions — below-threshold pairs must NOT have bound.
    #[serde(default)]
    pub no_edges: Vec<EdgeExpect>,
    /// `expect_anomaly` (Gotham G2): a `pattern_anomaly` event of this kind for this subject present.
    #[serde(default)]
    pub anomalies: Vec<AnomalyExpect>,
    /// `expect_no_anomaly` (G2): the subject must have NO `pattern_anomaly` of this kind (the
    /// non-over-firing counter-assertion — F6 sealed holdout).
    #[serde(default)]
    pub no_anomalies: Vec<AnomalyExpect>,
    /// `expect_baseline` (G2): assert the subject's recomputed `entity_baselines` row.
    #[serde(default)]
    pub baselines: Vec<BaselineExpect>,
    /// `expect_briefing` (G2 / Phase E): assert the pinned-date daily digest's structured `sections`.
    #[serde(default)]
    pub briefing: Option<BriefingGt>,
}

/// A daily-digest assertion (G2 / Phase E) against `daily_digests.sections` for a PINNED civil date.
/// The runner forces generation of `date`'s digest (a backend POST) after the authoritative rebuild,
/// then the scorer checks structured counts + label mentions — NEVER the prose `rendered_text` (a
/// narration surface). `date` is `YYYY-MM-DD` under the eval's fixed tz offset (0).
#[derive(Debug, Clone, Deserialize)]
pub struct BriefingGt {
    pub date: String,
    /// Exact-match assertions on `sections.counts.*` (each field independently optional).
    #[serde(default)]
    pub counts: BriefingCounts,
    /// Labels (enrolled `display_name`s) that MUST appear somewhere in `sections` — assignment-
    /// invariant, the way the graph modality names entities.
    #[serde(default)]
    pub mentions: Vec<String>,
}

/// Exact-count assertions against `daily_digests.sections.counts`. Every field is optional; the
/// calibration protocol freezes the observed value (widen, never narrow) — RECURSIVE_TESTING §4.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct BriefingCounts {
    #[serde(default)]
    pub new_entities: Option<i64>,
    #[serde(default)]
    pub anomalies: Option<i64>,
    #[serde(default)]
    pub top_visitors: Option<i64>,
    #[serde(default)]
    pub conversations: Option<i64>,
    #[serde(default)]
    pub first_time_pairings: Option<i64>,
    #[serde(default)]
    pub journeys: Option<i64>,
}

/// A `pattern_anomaly` assertion (G2). `subject` names the entity by enrolled `display_name`
/// (resolved to a catalog id, assignment-invariant); `kind` is the `metadata.kind`
/// (`off_schedule_presence` / `first_time_pairing` / `unknown_person_cluster` / `new_vehicle_for_person`).
#[derive(Debug, Clone, Deserialize)]
pub struct AnomalyExpect {
    pub subject: EntityRef,
    pub kind: String,
}

/// A baseline assertion (G2) against the subject's recomputed `entity_baselines` row.
#[derive(Debug, Clone, Deserialize)]
pub struct BaselineExpect {
    pub subject: EntityRef,
    /// `visits_in_window >=` this.
    #[serde(default)]
    pub min_visits: Option<i64>,
    /// The modal (peak) hour-of-week bucket's HOUR-OF-DAY (`peak_bucket % 24`) must equal this —
    /// weekday-agnostic so a weekly-cadence fixture asserts "arrives ~09:00" without pinning the day.
    #[serde(default)]
    pub peak_hour_of_day: Option<i64>,
}

/// One graph node reference. For `person`/`speaker`/`plate` the `name` is the enrolled `display_name`
/// (resolved to a catalog id by the scorer); for `device` the `name` IS the literal `device_id`.
#[derive(Debug, Clone, Deserialize)]
pub struct EntityRef {
    /// "person" | "speaker" | "plate" | "device"
    pub kind: String,
    pub name: String,
}

/// An edge assertion. Endpoint order is not significant for the undirected edge types
/// (`co_present`, `conversed_with`, `same_identity_candidate`) — the scorer matches either
/// direction. `min_evidence` is the `observation_count` floor (default 1); `expect_no_edge`
/// ignores it. `status` optionally pins a binding's review-queue state.
#[derive(Debug, Clone, Deserialize)]
pub struct EdgeExpect {
    pub from: EntityRef,
    pub to: EntityRef,
    /// "co_present" | "conversed_with" | "arrived_with_vehicle" | "same_identity_candidate" | "visits_place"
    pub kind: String,
    #[serde(default = "d_one")]
    pub min_evidence: i64,
    /// For `same_identity_candidate`: require the edge to carry this status ("candidate"/"confirmed"/"rejected").
    #[serde(default)]
    pub status: Option<String>,
}

// ----- loading + discovery ---------------------------------------------------

#[derive(Debug, Clone)]
pub struct Fixture {
    pub dir: PathBuf,
    pub split: String, // "train" | "holdout" | "staging" (opt-in, never gates)
    pub meta: Meta,
    pub expected: Expected,
}

impl Fixture {
    pub fn media_path(&self) -> PathBuf {
        self.dir.join(&self.meta.media_file)
    }
    /// Total clip span in nanoseconds (best-effort: segments * seg_seconds; a slack is added at query time).
    pub fn nominal_span_ns(&self) -> i64 {
        // Without decoding the media we don't know the exact count; the query window uses a
        // generous slack on top of this, and the poll set comes from the injector's emitted ids.
        let secs = self.meta.limit.unwrap_or(64) as i64 * self.meta.seg_seconds as i64;
        secs * 1_000_000_000
    }
}

pub fn load(dir: &Path, split: &str) -> Result<Fixture> {
    let meta: Meta = read_json(&dir.join("meta.json"))
        .with_context(|| format!("reading meta.json in {}", dir.display()))?;
    let expected: Expected = read_json(&dir.join("expected.json"))
        .with_context(|| format!("reading expected.json in {}", dir.display()))?;
    Ok(Fixture { dir: dir.to_path_buf(), split: split.to_string(), meta, expected })
}

/// Discover fixtures under `<root>/<split>/*/` for the given splits.
pub fn discover(root: &Path, splits: &[&str]) -> Result<Vec<Fixture>> {
    let mut out = Vec::new();
    for split in splits {
        let base = root.join(split);
        if !base.is_dir() {
            continue;
        }
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&base)
            .with_context(|| format!("reading {}", base.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir() && p.join("meta.json").is_file())
            .collect();
        dirs.sort();
        for d in dirs {
            out.push(load(&d, split)?);
        }
    }
    Ok(out)
}

fn read_json<T: serde::de::DeserializeOwned>(p: &Path) -> Result<T> {
    let s = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    Ok(serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display()))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The advisor staging fixtures are media-less, so nothing else exercises their JSON until a
    /// live `--fixtures staging` run — parse them here so a schema/typo drift fails fast.
    #[test]
    fn advisor_staging_fixtures_parse() {
        let root = crate::ctx::repo_root().join("hushai-eval/fixtures/staging");
        for case in ["advisor_followup", "advisor_direct"] {
            let fx = load(&root.join(case), "staging").expect(case);
            assert!(fx.meta.needs_advisor(), "{case} must list the advisor modality");
            let gt = fx.expected.advisor.as_ref().expect("advisor GT block");
            assert!(!gt.turns.is_empty(), "{case} must script at least one turn");
        }
    }

    /// The graph (Gotham G1) staging fixtures are media-less until a live rig calibration run, so
    /// nothing else exercises their JSON — parse them here so a `GraphGt` schema/typo drift fails
    /// fast. Also asserts the every-edge invariant: a known set of edge kinds + resolvable endpoint
    /// kinds, so a typo in a fixture (`vist_place`, `pesron`) is caught at unit time, not on the rig.
    #[test]
    fn graph_fixtures_parse() {
        const EDGE_KINDS: &[&str] =
            &["co_present", "conversed_with", "arrived_with_vehicle", "same_identity_candidate", "visits_place"];
        const NODE_KINDS: &[&str] = &["person", "speaker", "plate", "device"];
        const ANOMALY_KINDS: &[&str] =
            &["off_schedule_presence", "first_time_pairing", "unknown_person_cluster", "new_vehicle_for_person"];
        // G1 (F1-F3) + G2 (F4/F5/F7 train, F6 holdout). Loads meta+expected only (no media needed).
        let cases: &[(&str, &str)] = &[
            ("train", "graph_face_voice_bind"),
            ("train", "graph_cross_camera_fusion"),
            ("train", "graph_person_vehicle"),
            ("train", "graph_baseline_rhythm"),
            ("train", "anomaly_novel_time"),
            ("train", "briefing_daily"),
            ("train", "anomaly_first_pairing"),
            ("holdout", "anomaly_negatives"),
        ];
        for (split, case) in cases {
            let root = crate::ctx::repo_root().join(format!("hushai-eval/fixtures/{split}"));
            let fx = load(&root.join(case), split).expect(case);
            assert!(fx.meta.needs_graph(), "{case} must list the graph modality");
            let gt = fx.expected.graph.as_ref().unwrap_or_else(|| panic!("{case}: graph GT block"));
            assert!(
                !gt.entities.is_empty()
                    || !gt.edges.is_empty()
                    || !gt.no_edges.is_empty()
                    || !gt.anomalies.is_empty()
                    || !gt.no_anomalies.is_empty()
                    || !gt.baselines.is_empty()
                    || gt.briefing.is_some(),
                "{case} must assert at least one entity/edge/anomaly/baseline/briefing"
            );
            for e in gt.edges.iter().chain(gt.no_edges.iter()) {
                assert!(EDGE_KINDS.contains(&e.kind.as_str()), "{case}: bad edge kind {}", e.kind);
                for ep in [&e.from, &e.to] {
                    assert!(NODE_KINDS.contains(&ep.kind.as_str()), "{case}: bad node kind {}", ep.kind);
                }
            }
            for ent in &gt.entities {
                assert!(NODE_KINDS.contains(&ent.kind.as_str()), "{case}: bad entity kind {}", ent.kind);
            }
            for a in gt.anomalies.iter().chain(gt.no_anomalies.iter()) {
                assert!(ANOMALY_KINDS.contains(&a.kind.as_str()), "{case}: bad anomaly kind {}", a.kind);
                assert!(NODE_KINDS.contains(&a.subject.kind.as_str()), "{case}: bad anomaly subject kind {}", a.subject.kind);
            }
            for b in &gt.baselines {
                assert!(NODE_KINDS.contains(&b.subject.kind.as_str()), "{case}: bad baseline subject kind {}", b.subject.kind);
            }
            if let Some(br) = &gt.briefing {
                let d = br.date.as_bytes();
                assert!(
                    d.len() == 10 && d[4] == b'-' && d[7] == b'-',
                    "{case}: briefing.date must be YYYY-MM-DD, got {}",
                    br.date
                );
                assert!(
                    !br.mentions.is_empty()
                        || br.counts.new_entities.is_some()
                        || br.counts.anomalies.is_some()
                        || br.counts.top_visitors.is_some()
                        || br.counts.conversations.is_some()
                        || br.counts.first_time_pairings.is_some()
                        || br.counts.journeys.is_some(),
                    "{case}: briefing must assert at least one count or mention"
                );
            }
        }
    }
}

// ----- serde defaults --------------------------------------------------------

fn d_muxed() -> String { "muxed".into() }
fn d_full() -> String { "full".into() }
fn d_auto() -> String { "auto".into() }
fn d_info() -> String { "info".into() }
fn d_seg_seconds() -> u32 { 2 }
fn d_poll_timeout() -> u64 { 180 }
fn d_poll_interval() -> u64 { 2 }
fn d_quiesce_polls() -> u32 { 2 }
fn d_true() -> bool { true }
fn d_one() -> i64 { 1 }
fn d_max_wer() -> f64 { 0.15 }
fn d_min_sim() -> f64 { 0.85 }
fn d_min_purity() -> f64 { 0.80 }
fn d_min_conv() -> f64 { 0.90 }
fn d_min_acc() -> f64 { 0.5 }
fn d_min_f1() -> f64 { 0.5 }
