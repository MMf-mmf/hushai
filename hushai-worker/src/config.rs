//! Worker-specific configuration (model paths, polling, concurrency, retry).
//!
//! DB/connection config is reused from `hushai_backend::config::Config` so we don't
//! duplicate the schema-owning crate's knobs; this struct holds only what the
//! transcription/embedding worker adds on top.

use std::time::Duration;

use anyhow::anyhow;

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Path to the whisper.cpp GGML model file (local ASR).
    pub whisper_model_path: String,
    /// Base URL of the Ollama server the worker sends embedding requests to.
    /// From `EMBED_OLLAMA_BASE_URL`, falling back to `OLLAMA_BASE_URL`. Keeping this
    /// separate from the RAG answer-LLM endpoint lets always-on embedding load be
    /// routed to its own instance so it can't starve query answering (see RagConfig).
    pub embed_ollama_base_url: String,
    /// Embedding model name (must produce 1024-dim vectors to match the schema).
    pub embed_model: String,
    /// Number of concurrent per-segment pipelines.
    pub worker_concurrency: usize,
    /// How long to wait between polls when the queue is drained (keep-up mode).
    pub poll_interval: Duration,
    /// Max attempts before a segment is left in `error` and no longer retried.
    pub max_attempts: i32,
    /// A `processing` claim older than this (seconds) is considered crashed and re-leased.
    pub lease_timeout_secs: f64,
    /// ffmpeg binary used to decode segment audio to PCM.
    pub ffmpeg_bin: String,
}

impl WorkerConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let ollama_base_url = opt("OLLAMA_BASE_URL", "http://localhost:11434");
        Ok(Self {
            whisper_model_path: opt("WHISPER_MODEL_PATH", "./models/ggml-base.en.bin"),
            embed_ollama_base_url: opt("EMBED_OLLAMA_BASE_URL", &ollama_base_url),
            embed_model: opt("EMBED_MODEL", "mxbai-embed-large"),
            worker_concurrency: parse("WORKER_CONCURRENCY", "2")?,
            poll_interval: Duration::from_secs(parse("POLL_INTERVAL_SECS", "5")?),
            max_attempts: parse("MAX_ATTEMPTS", "5")?,
            lease_timeout_secs: parse("LEASE_TIMEOUT_SECS", "300")?,
            ffmpeg_bin: opt("FFMPEG_BIN", "ffmpeg"),
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
