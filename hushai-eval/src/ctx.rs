//! Shared evaluation context: DB pool, HTTP client, service URLs, tokens, and repo paths.

use anyhow::{Context, Result};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::path::{Path, PathBuf};

pub struct Ctx {
    pub pool: PgPool,
    pub http: reqwest::Client,
    pub database_url: String,
    pub backend_url: String,
    pub rag_url: String,
    pub advisor_url: String,
    pub ollama_url: String,
    pub device_token: String,
    pub rag_token: Option<String>,
    pub advisor_token: Option<String>,
    pub repo_root: PathBuf,
    pub fixtures_root: PathBuf,
    pub baselines_root: PathBuf,
    pub feed_script: PathBuf,
    pub scratch: PathBuf,
}

impl Ctx {
    pub async fn connect(repo_root: PathBuf) -> Result<Self> {
        let database_url = env_required("DATABASE_URL")?;
        // Invariant 1: never score against the dev DB. The reset routine TRUNCATEs everything.
        if !database_url.contains("_test") && std::env::var("HUSHAI_EVAL_ALLOW_NONTEST_DB").is_err() {
            anyhow::bail!(
                "refusing to run eval against `{database_url}` — not a *_test database.\n\
                 The harness TRUNCATEs result + catalog tables every run. Point DATABASE_URL at \
                 hushai_test (see local_dev/eval.env), or set HUSHAI_EVAL_ALLOW_NONTEST_DB=1 to override."
            );
        }
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .with_context(|| format!("connecting to {database_url}"))?;
        // Whole-request ceiling (includes streaming body). Advisor SSE answers run the full
        // pipeline (route→recall→ground→draft→review→refine→polish→memorize) and legitimately
        // take 130-170s on CPU with qwen2.5:7b at ADVISOR_NUM_CTX=16384, so 120s would abort a
        // valid stream mid-flight. Kept above the advisor path's own ASK_TIMEOUT_SECS=300
        // (query_advisor.rs) so that per-turn tokio guard — not this client ceiling — governs.
        // Env override (HUSHAI_EVAL_HTTP_TIMEOUT_SECS) for a very slow CPU endpoint where a single
        // advisor turn can exceed 360 s; keep it >= the advisor path's ASK_TIMEOUT_SECS so that
        // per-turn tokio guard governs.
        let http_timeout_secs: u64 = std::env::var("HUSHAI_EVAL_HTTP_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(360);
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(http_timeout_secs))
            .build()?;
        let scratch = repo_root.join("hushai-eval/.work");
        std::fs::create_dir_all(&scratch).ok();
        Ok(Self {
            pool,
            http,
            database_url,
            backend_url: env_or("HUSHAI_BACKEND_URL", "http://localhost:8080"),
            rag_url: env_or("HUSHAI_RAG_URL", "http://localhost:8090"),
            advisor_url: env_or("HUSHAI_ADVISOR_URL", "http://localhost:8095"),
            ollama_url: env_or("OLLAMA_BASE_URL", "http://localhost:11434"),
            device_token: env_or("DEVICE_TOKEN", "dev-secret-token"),
            rag_token: std::env::var("RAG_TOKEN").ok().filter(|s| !s.is_empty()),
            advisor_token: std::env::var("ADVISOR_TOKEN").ok().filter(|s| !s.is_empty()),
            fixtures_root: repo_root.join("hushai-eval/fixtures"),
            baselines_root: repo_root.join("hushai-eval/baselines"),
            feed_script: repo_root.join("local_dev/feed_segments.py"),
            scratch,
            repo_root,
        })
    }
}

/// Locate the repo root from the eval crate's manifest dir (its parent).
pub fn repo_root() -> PathBuf {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.parent().map(|p| p.to_path_buf()).unwrap_or(here)
}

/// Layer env files WITHOUT overriding already-set process env. `eval.env` is loaded BEFORE `.env`
/// so its `DATABASE_URL=…hushai_test` wins over the dev `.env` (and the process env wins over both).
pub fn load_env_files(repo_root: &Path) {
    for rel in ["local_dev/eval.env", ".env", "hushai-backend/.env"] {
        let p = repo_root.join(rel);
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            if std::env::var(k).is_err() {
                // SAFETY: single-threaded startup, before the tokio runtime spawns workers.
                unsafe { std::env::set_var(k, v) };
            }
        }
    }
}

fn env_required(k: &str) -> Result<String> {
    std::env::var(k).with_context(|| format!("required env var {k} is not set"))
}
fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| d.to_string())
}
