//! Runtime configuration, sourced from the environment (see `.env.example`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, anyhow};

/// All knobs the server needs. Defaults are applied for everything except the
/// three required secrets/locations.
#[derive(Debug, Clone)]
pub struct Config {
    /// Postgres connection string.
    pub database_url: String,
    /// Root directory for content-addressed media blobs.
    pub blob_dir: PathBuf,
    /// Device token accepted as `Authorization: Bearer <token>`. Always required
    /// (the viewer proxy's `BACKEND_TOKEN` default + single-device setups rely on it).
    pub device_token: String,
    /// Optional comma-separated `label:token` per-device allowlist. When set it
    /// supersedes `device_token` for ingest auth, enabling per-device revocation.
    /// See [`crate::auth::TokenStore::from_spec`].
    pub device_tokens: Option<String>,

    /// Address/port the HTTP server binds to.
    pub bind_addr: SocketAddr,
    /// Optional native TLS. Both paths set ⇒ serve HTTPS; neither ⇒ cleartext (dev/USB).
    pub tls: Option<crate::tls::TlsPaths>,
    /// Max accepted request body (bytes). Oversized requests are rejected before any write.
    pub max_body_bytes: usize,
    /// Global in-flight request cap; excess requests are shed with 429.
    pub concurrency_cap: usize,
    /// Minimum free space (bytes) on the blob volume before shedding with 507.
    pub disk_watermark_bytes: u64,
    /// Postgres pool size.
    pub db_max_connections: u32,
    /// How long to wait for a pooled connection before surfacing 507.
    pub db_acquire_timeout_secs: u64,
    /// Overall per-request timeout (seconds).
    pub request_timeout_secs: u64,
    /// Ingest-side AI skip gate driven by device content hints (see `crate::hints`).
    pub hint_gate: crate::hints::HintGateCfg,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            database_url: required("DATABASE_URL")?,
            blob_dir: PathBuf::from(required("BLOB_DIR")?),
            device_token: required("DEVICE_TOKEN")?,
            device_tokens: std::env::var("DEVICE_TOKENS")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            bind_addr: parse_opt("BIND_ADDR", "0.0.0.0:8080")?,
            tls: crate::tls::TlsPaths::from_env("")?,
            max_body_bytes: parse_opt("MAX_BODY_BYTES", "33554432")?,
            concurrency_cap: parse_opt("CONCURRENCY_CAP", "64")?,
            disk_watermark_bytes: parse_opt("DISK_WATERMARK_BYTES", "1073741824")?,
            db_max_connections: parse_opt("DB_MAX_CONNECTIONS", "16")?,
            db_acquire_timeout_secs: parse_opt("DB_ACQUIRE_TIMEOUT_SECS", "2")?,
            request_timeout_secs: parse_opt("REQUEST_TIMEOUT_SECS", "30")?,
            hint_gate: crate::hints::HintGateCfg::from_env()?,
        })
    }
}

fn required(key: &str) -> anyhow::Result<String> {
    std::env::var(key).map_err(|_| anyhow!("required env var {key} is not set"))
}

fn parse_opt<T>(key: &str, default: &str) -> anyhow::Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let raw = std::env::var(key).unwrap_or_else(|_| default.to_string());
    T::from_str(raw.trim())
        .map_err(|e| anyhow!("env var {key}={raw:?} is invalid: {e}"))
        .with_context(|| format!("parsing {key}"))
}
