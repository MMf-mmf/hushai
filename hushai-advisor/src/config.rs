//! Advisor service configuration. DB config is reused from `hushai_backend::config::Config`.

use std::net::SocketAddr;

use anyhow::anyhow;

#[derive(Debug, Clone)]
pub struct AdvisorConfig {
    /// Base URL of the Ollama server used to embed queries/chunks/memories. From
    /// `EMBED_OLLAMA_BASE_URL`, falling back to `OLLAMA_BASE_URL` — the SAME instance
    /// the worker/rag embed against (shared embed model/cache, one 1024-dim space).
    pub embed_ollama_base_url: String,
    /// Base URL of the Ollama server used for the advisor's LLM calls. From
    /// `LLM_OLLAMA_BASE_URL`, falling back to `OLLAMA_BASE_URL`.
    pub llm_ollama_base_url: String,
    /// Embedding model — MUST match the worker/rag's (same 1024-dim space).
    pub embed_model: String,
    /// Chat model for drafting/refining answers (and every step without a judge override).
    pub llm_model: String,
    /// Optional larger model for the JUDGE steps (sufficiency gate + answer critique):
    /// verdict quality matters more than fluency there, and a 7B judge tends to
    /// rubber-stamp its own drafts. `None` -> falls back to `llm_model`. Mirrors the
    /// `REFLECTION_LLM_MODEL` per-role-override precedent (hushai-rag/src/config.rs).
    pub judge_model: Option<String>,
    /// LLM sampling temperature applied to EVERY agent build. Default 0.0 = greedy decode,
    /// so routing and answers are reproducible run-to-run (what lets the eval harness gate
    /// on advisor answers/routing without flaking).
    pub llm_temperature: f64,
    /// Optional Ollama sampling seed (passed as `options.seed`). Belt-and-suspenders
    /// with temp 0 for determinism.
    pub llm_seed: Option<i64>,
    /// Ollama context window (`options.num_ctx`) for the advisor's LLM calls. NOTHING else
    /// in the repo sets num_ctx, so Ollama's default (4096) governs there — fine for the
    /// rag service's char-budgeted prompts, but the advisor feeds 2–6 FULL book chapters
    /// (~1–3K tokens each) plus history into one prompt, which a 4096 default would
    /// SILENTLY truncate (no error; the model just answers from partial context).
    /// KV-cache cost at 16K on a 7B q4 is roughly 2–3 GB extra.
    pub llm_num_ctx: i64,

    /// Address/port the HTTP server binds to.
    pub bind_addr: SocketAddr,
    /// Optional native TLS (`ADVISOR_TLS_CERT_PATH`/`ADVISOR_TLS_KEY_PATH`, falling back
    /// to the bare `TLS_CERT_PATH`/`TLS_KEY_PATH`).
    pub tls: Option<hushai_backend::tls::TlsPaths>,
    /// Optional bearer token. When set, requests must present it; when unset, auth is off
    /// (loopback-only unless ADVISOR_ALLOW_INSECURE — enforced at startup, see lib.rs).
    pub advisor_token: Option<String>,

    /// Max follow-up (Yenta) rounds per consultation before the pipeline force-proceeds.
    pub max_followup_rounds: i64,
    /// Max clarifying questions surfaced per follow-up round.
    pub max_questions_per_round: usize,
    /// Max Traffic-Controller→draft→critique iterations per answer (the spec's
    /// "loop back 3–5 times", bounded; converges early when routing proposes no new chapters).
    pub max_refine_iters: usize,
    /// Max chapters the Traffic Controller may propose in ONE routing call.
    pub max_chapters_per_route: usize,
    /// Max distinct chapters accumulated across all refine iterations of one answer.
    pub max_total_chapters: usize,

    /// Reject chat messages longer than this (defensive; mirrors the rag chat guard).
    pub max_message_chars: usize,
    /// Number of trailing user+assistant *turns* loaded into the LLM context per request.
    pub history_turns: i64,
    /// Per-chapter character budget in the draft prompt; a chapter longer than this is
    /// represented by its nearest chunks instead of its full text.
    pub chapter_max_chars: usize,
    /// Total character budget across all chapter texts in the draft prompt.
    pub context_max_total_chars: usize,

    /// Semantic-candidate widening for the Traffic Controller: top-k chunk hits whose
    /// chapters are offered as extra candidates (the anti-tunnel-vision signal).
    pub route_semantic_top_k: i64,

    /// Past-consultation memory: retrieval top-k, cosine-distance cutoff, master switch.
    pub memory_top_k: i64,
    pub memory_distance_threshold: f64,
    pub memory_enabled: bool,
}

impl AdvisorConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let advisor_token = std::env::var("ADVISOR_TOKEN")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let ollama_base_url = opt("OLLAMA_BASE_URL", "http://localhost:11434");
        Ok(Self {
            embed_ollama_base_url: opt("EMBED_OLLAMA_BASE_URL", &ollama_base_url),
            llm_ollama_base_url: opt("LLM_OLLAMA_BASE_URL", &ollama_base_url),
            embed_model: opt("EMBED_MODEL", "mxbai-embed-large"),
            llm_model: opt("ADVISOR_LLM_MODEL", "qwen2.5:7b"),
            judge_model: std::env::var("ADVISOR_JUDGE_MODEL")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            llm_temperature: parse("ADVISOR_LLM_TEMPERATURE", "0.0")?,
            llm_seed: std::env::var("ADVISOR_LLM_SEED")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().parse::<i64>())
                .transpose()
                .map_err(|e| anyhow!("env var ADVISOR_LLM_SEED is invalid: {e}"))?,
            llm_num_ctx: parse("ADVISOR_NUM_CTX", "16384")?,
            bind_addr: parse("ADVISOR_BIND_ADDR", "0.0.0.0:8095")?,
            tls: hushai_backend::tls::TlsPaths::from_env("ADVISOR_")?,
            advisor_token,
            max_followup_rounds: parse("ADVISOR_MAX_FOLLOWUP_ROUNDS", "2")?,
            max_questions_per_round: parse("ADVISOR_MAX_QUESTIONS_PER_ROUND", "3")?,
            max_refine_iters: parse("ADVISOR_MAX_REFINE_ITERS", "3")?,
            max_chapters_per_route: parse("ADVISOR_MAX_CHAPTERS_PER_ROUTE", "4")?,
            max_total_chapters: parse("ADVISOR_MAX_TOTAL_CHAPTERS", "6")?,
            max_message_chars: parse("ADVISOR_MAX_MESSAGE_CHARS", "4000")?,
            history_turns: parse("ADVISOR_HISTORY_TURNS", "8")?,
            chapter_max_chars: parse("ADVISOR_CHAPTER_MAX_CHARS", "12000")?,
            context_max_total_chars: parse("ADVISOR_CONTEXT_MAX_TOTAL_CHARS", "36000")?,
            route_semantic_top_k: parse("ADVISOR_ROUTE_SEMANTIC_TOP_K", "8")?,
            memory_top_k: parse("ADVISOR_MEMORY_TOP_K", "3")?,
            memory_distance_threshold: parse("ADVISOR_MEMORY_DISTANCE_THRESHOLD", "0.6")?,
            memory_enabled: parse("ADVISOR_MEMORY_ENABLED", "true")?,
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
