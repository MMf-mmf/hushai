//! RAG service configuration. DB config is reused from `hushai_backend::config::Config`.

use std::net::SocketAddr;

use anyhow::anyhow;

#[derive(Debug, Clone)]
pub struct RagConfig {
    /// Base URL of the Ollama server used to embed the *query*. From
    /// `EMBED_OLLAMA_BASE_URL`, falling back to `OLLAMA_BASE_URL`. Point this at the
    /// same instance the worker embeds against (shared embed model/cache).
    pub embed_ollama_base_url: String,
    /// Base URL of the Ollama server used for *answer generation*. From
    /// `LLM_OLLAMA_BASE_URL`, falling back to `OLLAMA_BASE_URL`. Separating it from
    /// the embed endpoint keeps sustained ingest-time embedding load from starving
    /// query answering (run the two on different Ollama instances under load).
    pub llm_ollama_base_url: String,
    /// Embedding model — MUST match the worker's (same 1024-dim space).
    pub embed_model: String,
    /// Chat model used to synthesize the grounded answer.
    pub rag_llm_model: String,
    /// LLM sampling temperature applied to EVERY Rig agent build (answer, chat, reflection, and —
    /// most importantly — the auto-router `classify_agent`). Default 0.0 = greedy decode, so routing
    /// and answers are reproducible run-to-run; this is what lets the eval harness gate on RAG
    /// answers/routing without flaking. Raise for production if more varied phrasing is wanted.
    pub rag_llm_temperature: f64,
    /// Optional Ollama sampling seed (passed as `options.seed`). Belt-and-suspenders with temp 0 for
    /// determinism. `None` -> Ollama's default (nondeterministic seed).
    pub rag_llm_seed: Option<i64>,
    /// Address/port the HTTP server binds to.
    pub bind_addr: SocketAddr,
    /// Optional native TLS (`RAG_TLS_CERT_PATH`/`RAG_TLS_KEY_PATH`, falling back to the
    /// bare `TLS_CERT_PATH`/`TLS_KEY_PATH`). Both set ⇒ HTTPS; neither ⇒ cleartext.
    pub tls: Option<hushai_backend::tls::TlsPaths>,
    /// Default number of nearest passages to retrieve.
    pub top_k_default: i64,
    /// Cosine-distance cutoff; matches beyond this are dropped from context/sources.
    pub distance_threshold: f64,
    /// HNSW `ef_search` for retrieval. Raised above top_k so a filtered ANN walk
    /// (with iterative_scan) fills top_k instead of returning a short/empty set.
    pub hnsw_ef_search: i64,
    /// Per-query `statement_timeout` (ms) on the retrieval transaction, so a
    /// pathological scan can't hang a request.
    pub query_timeout_ms: i64,
    /// Optional bearer token. When set, requests must present it; when unset, auth is off.
    pub rag_token: Option<String>,

    /// Multi-turn chat: number of trailing user+assistant *turns* loaded into the LLM
    /// context per request (bounds context-window growth). The history fetch loads
    /// `2 * this` messages.
    pub chat_history_turns: i64,
    /// Reject chat messages longer than this (defensive; mirrors the TTS char guard).
    pub chat_max_message_chars: usize,
    /// Multi-turn query CONDENSATION (flaw F4): on a follow-up turn, rewrite the latest message into
    /// a STANDALONE query (carrying the subject + resolving relative time from the prior turns) BEFORE
    /// routing + retrieval — otherwise "…and the week before?" re-embeds bare, loses the entity, and
    /// mis-routes. Default on; the rewrite runs at temp 0 (deterministic) and returns the message
    /// unchanged when it's already standalone. Env `RAG_QUERY_CONDENSE`.
    pub query_condense: bool,
    /// Staleness guard on the LLM-visible history window: turns older than this many seconds
    /// are excluded from `history`/`recent_context` (and therefore from condensation and the
    /// router) — a stale morning session must not color an afternoon question, no matter how
    /// the client manages its session pointer. `0` disables. The persisted transcript is
    /// untouched. Env `RAG_CHAT_HISTORY_MAX_AGE_SECS`.
    pub chat_history_max_age_secs: i64,

    /// Reflection agent — owner identity for the "how have *I* been" case. `OWNER_SPEAKER_ID`
    /// (a speaker uuid as text) wins; else `OWNER_SPEAKER_NAME` is resolved against the
    /// `speakers` catalog at request time. Both optional — with neither set the reflection
    /// agent requires an explicit speaker filter and otherwise declines gracefully.
    pub owner_speaker_id: Option<String>,
    pub owner_speaker_name: Option<String>,
    /// Default reflection analysis window (days) when a request supplies no time filter.
    pub analysis_window_days_default: i64,
    /// Conversation segmentation: a silence/gap longer than this (seconds) starts a new
    /// "conversation" when analytics groups sentences into social interactions.
    pub conversation_gap_secs: i64,
    /// Presence visit coalescing: consecutive sightings of the same subject (person/plate/
    /// object) closer together than this (seconds) are ONE continuous visit, not N separate
    /// "seen N times" events — the capture pipeline re-detects every ~2s segment, so raw
    /// sighting counts are an artifact of segmentation, not of the world.
    pub presence_visit_gap_secs: i64,
    /// Chat-time entity-profile freshen: before answering a "tell me about <name>" question,
    /// fold any settled-but-unconsumed events into that identity's profile so the answer is
    /// never staler than the events table (and eval runs don't race the worker's interval).
    pub profile_chat_refresh: bool,
    /// Grace window (seconds) for the chat-time profile freshen: events updated more recently
    /// than this are left for a later pass (they may still be UPSERT-extending). The eval
    /// profile pins this to 0 — injected fixtures are fully settled before questions fire.
    pub profile_grace_secs: i64,
    /// Window-summary ("what have we spoken about today"): at most this many of the window's
    /// most recent conversations are stitched into the summary prompt.
    pub summary_max_convos: usize,
    /// Window-summary: total character budget across all stitched conversation excerpts
    /// (keeps the prompt bounded on a chatty day).
    pub summary_max_total_chars: usize,
    /// Fixed timezone offset (seconds, e.g. -14400 for EDT) applied before hour-of-day /
    /// weekly bucketing. Nanos are UTC; this is a deterministic offset, NOT full DST.
    pub analysis_tz_offset_secs: i64,
    /// Optional larger Ollama model for reflection synthesis (narrating a stats digest into
    /// coaching is harder than extractive QA). `None` -> falls back to `rag_llm_model`.
    /// Must be pulled on the LLM Ollama instance.
    pub reflection_llm_model: Option<String>,

    /// Whether `/v1/tts` synthesizes spoken answers. When false (or when the model
    /// dir is missing), the TTS engine isn't loaded and `/v1/tts` returns 503.
    pub tts_enabled: bool,
    /// Directory holding the Kokoro bundle (model.onnx, voices.bin, tokens.txt,
    /// espeak-ng-data/). Populate with `local_dev/fetch_tts_model.sh`.
    pub tts_dir: String,
    /// Kokoro speaker id. For `kokoro-en-v0_19`, neutral American male am_michael=6,
    /// am_adam=5; British males bm_george=9, bm_lewis=10.
    pub tts_sid: i32,
    /// Speech rate multiplier (1.0 = natural; >1 faster, <1 slower).
    pub tts_speed: f32,
    /// onnxruntime intra-op threads for synthesis.
    pub tts_threads: i32,

    /// Open-vocabulary object retrieval (Phase B query side). When false (or the model/tokenizer
    /// are absent), the CLIP text tower isn't loaded and the `objects` agent returns 503.
    pub clip_text_enabled: bool,
    /// CLIP TEXT tower ONNX (exported by local_dev/export_clip.py). Same checkpoint as the worker's
    /// CLIP image tower so query and document vectors share one 512-d space.
    pub clip_text_model_path: String,
    /// HF CLIP tokenizer.json (local_dev/fetch_clip_tokenizer.sh).
    pub clip_tokenizer_path: String,
    /// ONNX Runtime 1.20 dylib `ort` dlopen()s (load-dynamic). The SAME dylib the worker uses;
    /// MUST be 1.20.x and distinct from sherpa's bundled 1.17.1.
    pub ort_dylib_path: String,
    /// Cosine-distance cutoff for object NN. CLIP cosine is looser than mxbai text, so this is its
    /// own knob (~0.75) rather than reusing `distance_threshold` — prunes hallucinated sightings.
    pub object_distance_threshold: f64,
    /// Default number of object sightings to retrieve per object query.
    pub object_top_k_default: i64,

    /// Person attribution — owner identity for "who was I with" (the face co-occurrence anchor).
    /// `OWNER_PERSON_ID` (a person uuid as text) wins; else `OWNER_PERSON_NAME` is resolved against
    /// the `persons` catalog at request time. Both optional — with neither set, "who was I with"
    /// declines gracefully (a targeted "when did I see X" still works).
    pub owner_person_id: Option<String>,
    pub owner_person_name: Option<String>,
    /// Default number of person sightings to list per person query.
    pub person_top_k_default: i64,

    /// License-plate attribution — default number of plate sightings to list per plate query.
    /// (Plates have no "who was I with" owner anchor; resolution is by plate string, not identity.)
    pub plate_top_k_default: i64,

    /// Recency path ("what did we last discuss"): the most recent gap-grouped conversation is
    /// summarized rather than semantically retrieved. `recency_scan_limit` bounds the DESC
    /// backscan from the latest sentence; `recency_max_sentences`/`recency_max_chars` cap what
    /// the LLM summarizes (spoken answers are short, and the small model context is finite).
    pub recency_scan_limit: i64,
    pub recency_max_sentences: usize,
    pub recency_max_chars: usize,

    /// Assistant context layer: a per-turn "Facts (reliable, from the system)" briefing (date,
    /// owner, known-voice/person rosters, cameras, last-conversation anchor) prepended to the
    /// answer prompt, plus same-segment vision annotation of retrieved passages. All bounded /
    /// env-gated so a small local model is never flooded; disabling yields byte-identical prompts.
    pub context_briefing_enabled: bool,
    pub context_max_chars: usize,
    /// Max named entries listed per roster (voices, people) in the briefing.
    pub context_roster_max: i64,
    /// Annotate retrieved transcript passages with same-segment vision detections.
    pub context_vision_enrich_enabled: bool,
}

impl RagConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let rag_token = std::env::var("RAG_TOKEN")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let ollama_base_url = opt("OLLAMA_BASE_URL", "http://localhost:11434");
        Ok(Self {
            embed_ollama_base_url: opt("EMBED_OLLAMA_BASE_URL", &ollama_base_url),
            llm_ollama_base_url: opt("LLM_OLLAMA_BASE_URL", &ollama_base_url),
            embed_model: opt("EMBED_MODEL", "mxbai-embed-large"),
            // qwen2.5:7b (modern, strongly instruction-following) is far more faithful at the strict
            // extractive grounding the answer paths need — the small llama3.2:3b would embellish
            // attribution answers with sightings not in the sources. Override with RAG_LLM_MODEL.
            rag_llm_model: opt("RAG_LLM_MODEL", "qwen2.5:7b"),
            rag_llm_temperature: parse("RAG_LLM_TEMPERATURE", "0.0")?,
            rag_llm_seed: std::env::var("RAG_LLM_SEED")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().parse::<i64>())
                .transpose()
                .map_err(|e| anyhow!("env var RAG_LLM_SEED is invalid: {e}"))?,
            bind_addr: parse("RAG_BIND_ADDR", "0.0.0.0:8090")?,
            tls: hushai_backend::tls::TlsPaths::from_env("RAG_")?,
            top_k_default: parse("RAG_TOP_K_DEFAULT", "8")?,
            distance_threshold: parse("RAG_DISTANCE_THRESHOLD", "0.6")?,
            hnsw_ef_search: parse("RAG_HNSW_EF_SEARCH", "100")?,
            query_timeout_ms: parse("RAG_QUERY_TIMEOUT_MS", "10000")?,
            rag_token,
            chat_history_turns: parse("RAG_CHAT_HISTORY_TURNS", "8")?,
            chat_max_message_chars: parse("RAG_CHAT_MAX_MESSAGE_CHARS", "4000")?,
            query_condense: parse("RAG_QUERY_CONDENSE", "true")?,
            chat_history_max_age_secs: parse("RAG_CHAT_HISTORY_MAX_AGE_SECS", "3600")?,
            owner_speaker_id: std::env::var("OWNER_SPEAKER_ID")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            owner_speaker_name: std::env::var("OWNER_SPEAKER_NAME")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            analysis_window_days_default: parse("ANALYSIS_WINDOW_DAYS_DEFAULT", "90")?,
            conversation_gap_secs: parse("CONVERSATION_GAP_SECS", "300")?,
            presence_visit_gap_secs: parse("PRESENCE_VISIT_GAP_SECS", "120")?,
            profile_chat_refresh: parse("PROFILE_CHAT_REFRESH", "true")?,
            profile_grace_secs: parse("PROFILE_GRACE_SECS", "90")?,
            summary_max_convos: parse("RAG_SUMMARY_MAX_CONVOS", "8")?,
            summary_max_total_chars: parse("RAG_SUMMARY_MAX_TOTAL_CHARS", "8000")?,
            analysis_tz_offset_secs: parse("ANALYSIS_TZ_OFFSET_SECS", "0")?,
            reflection_llm_model: std::env::var("REFLECTION_LLM_MODEL")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            tts_enabled: parse("RAG_TTS_ENABLED", "true")?,
            tts_dir: opt("RAG_TTS_DIR", "models/kokoro-en-v0_19"),
            tts_sid: parse("RAG_TTS_SID", "6")?,
            tts_speed: parse("RAG_TTS_SPEED", "1.0")?,
            tts_threads: parse("RAG_TTS_THREADS", "2")?,
            clip_text_enabled: parse("RAG_OBJECTS_ENABLED", "true")?,
            clip_text_model_path: opt("CLIP_TEXT_MODEL_PATH", "./models/clip_vit_b32_text.onnx"),
            clip_tokenizer_path: opt("CLIP_TOKENIZER_PATH", "./models/clip_tokenizer.json"),
            ort_dylib_path: opt(
                "ORT_DYLIB_PATH",
                "./models/onnxruntime/onnxruntime-osx-arm64-1.20.0/lib/libonnxruntime.1.20.0.dylib",
            ),
            object_distance_threshold: parse("RAG_OBJECT_DISTANCE_THRESHOLD", "0.75")?,
            object_top_k_default: parse("RAG_OBJECT_TOP_K_DEFAULT", "12")?,
            owner_person_id: std::env::var("OWNER_PERSON_ID")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            owner_person_name: std::env::var("OWNER_PERSON_NAME")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            person_top_k_default: parse("RAG_PERSON_TOP_K_DEFAULT", "50")?,
            plate_top_k_default: parse("RAG_PLATE_TOP_K_DEFAULT", "50")?,
            recency_scan_limit: parse("RAG_RECENCY_SCAN_LIMIT", "400")?,
            recency_max_sentences: parse("RAG_RECENCY_MAX_SENTENCES", "40")?,
            recency_max_chars: parse("RAG_RECENCY_MAX_CHARS", "4000")?,
            context_briefing_enabled: parse("RAG_CONTEXT_BRIEFING_ENABLED", "true")?,
            context_max_chars: parse("RAG_CONTEXT_MAX_CHARS", "1200")?,
            context_roster_max: parse("RAG_CONTEXT_ROSTER_MAX", "12")?,
            context_vision_enrich_enabled: parse("RAG_CONTEXT_VISION_ENRICH_ENABLED", "true")?,
        })
    }
}

fn opt(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn parse<T>(key: &str, default: &str) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = std::env::var(key).unwrap_or_else(|_| default.to_string());
    raw.trim()
        .parse::<T>()
        .map_err(|e| anyhow!("env var {key}={raw:?} is invalid: {e}"))
}
