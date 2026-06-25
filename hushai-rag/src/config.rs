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
    /// Address/port the HTTP server binds to.
    pub bind_addr: SocketAddr,
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
}

impl RagConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let rag_token = std::env::var("RAG_TOKEN").ok().filter(|s| !s.trim().is_empty());
        let ollama_base_url = opt("OLLAMA_BASE_URL", "http://localhost:11434");
        Ok(Self {
            embed_ollama_base_url: opt("EMBED_OLLAMA_BASE_URL", &ollama_base_url),
            llm_ollama_base_url: opt("LLM_OLLAMA_BASE_URL", &ollama_base_url),
            embed_model: opt("EMBED_MODEL", "mxbai-embed-large"),
            rag_llm_model: opt("RAG_LLM_MODEL", "llama3.2:3b"),
            bind_addr: parse("RAG_BIND_ADDR", "0.0.0.0:8090")?,
            top_k_default: parse("RAG_TOP_K_DEFAULT", "8")?,
            distance_threshold: parse("RAG_DISTANCE_THRESHOLD", "0.6")?,
            hnsw_ef_search: parse("RAG_HNSW_EF_SEARCH", "100")?,
            query_timeout_ms: parse("RAG_QUERY_TIMEOUT_MS", "10000")?,
            rag_token,
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
