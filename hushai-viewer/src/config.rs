//! Viewer-specific configuration. DB/blob config is reused from
//! `hushai_backend::config::Config`; this struct holds only what the viewer adds.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::anyhow;

/// One hour and two hours, in nanoseconds — the default and max playable window.
const HOUR_NANOS: i64 = 3_600_000_000_000;

#[derive(Debug, Clone)]
pub struct ViewerConfig {
    /// Address/port the HTTP server binds to. Defaults to **localhost-only** since the
    /// viewer is an unauthenticated local admin UI with read access to all media.
    pub bind_addr: SocketAddr,
    /// Directory of the bundled static UI (served at `/`).
    pub ui_dir: PathBuf,
    /// Where remuxed `.ts` segments are cached (pure derived data; safe to delete).
    pub cache_dir: PathBuf,
    /// ffmpeg binary used to remux blobs to MPEG-TS. Shared `FFMPEG_BIN` with the worker.
    pub ffmpeg_bin: String,
    /// Max concurrent ffmpeg processes (separate budget from the ingest backend).
    pub ffmpeg_concurrency: usize,
    /// Default timeline/playlist window when the client omits `from`/`to`.
    pub default_window_nanos: i64,
    /// Hard cap on a *playable* (HLS) window so a single playlist can never blow up.
    pub max_window_nanos: i64,
    /// Base URL of the hushai-rag service that `/v1/*` is reverse-proxied to (one origin
    /// for the browser; no CORS). Defaults to the rag default bind on localhost.
    pub rag_base_url: String,
    /// Optional bearer injected on proxied `/v1/*` requests (so the browser never holds
    /// the RAG token). Same `RAG_TOKEN` the rag service enforces; `None` = no auth.
    pub rag_token: Option<String>,
    /// Base URL of the hushai-backend service. Speaker-admin endpoints (`/v1/speakers*`)
    /// live there, not in rag, so the proxy routes those to this upstream. Defaults to the
    /// backend's default bind on localhost.
    pub backend_base_url: String,
    /// Bearer injected on proxied `/v1/speakers*` requests. Defaults to `DEVICE_TOKEN`
    /// (the token the backend enforces, already dotenv-loaded from `hushai-backend/.env`),
    /// overridable via `BACKEND_TOKEN`. `None` = no auth (backend will 401 if it requires one).
    pub backend_token: Option<String>,
    /// Backstop cap on detection rows returned *per vision table* by `/api/.../detections`.
    /// The UI fetches a small rolling window, but the endpoint is reachable with any range
    /// (clamped to `max_window_nanos`), so this bounds worst-case payload size. On a hit the
    /// response carries `truncated: true` and the server logs a WARN (never a silent cap).
    pub detections_max_rows: i64,
    /// Backstop cap on segments scanned *per lane* by `/api/.../processing`. Rows are tiny
    /// (one status token + small counts per ~2s segment), so this can be generous; on a hit
    /// the response carries `truncated: true` and the server logs a WARN (never a silent cap).
    pub processing_max_rows: i64,

    // --- System dashboard (`/api/dashboard`) ---
    /// Blob root (from the shared backend config) — the disk probe reports free space here.
    pub blob_dir: PathBuf,
    /// Free-space watermark (from the shared backend config) below which disk shows `degraded`.
    pub disk_watermark_bytes: u64,
    /// Ollama base URL the dashboard probes (`/api/tags`). Best-effort/optional.
    pub ollama_base_url: String,
    /// A camera whose last upload is within this many seconds counts as `connected`.
    pub dash_connected_secs: i64,
    /// A camera within this many seconds (but beyond `connected`) counts as `idle`; else `offline`.
    pub dash_idle_secs: i64,
    /// A `worker_heartbeat` row older than this many seconds is treated as `down`.
    pub dash_worker_stale_secs: i64,
    /// Per-service HTTP probe timeout (ms) so one hung sibling can't stall `/api/dashboard`.
    pub dash_probe_timeout_ms: u64,
}

impl ViewerConfig {
    /// `blob_dir` (from the shared backend config) seeds the default cache location and the
    /// dashboard's disk probe; `disk_watermark_bytes` (also from the shared config) is the
    /// free-space floor the dashboard flags as `degraded`.
    pub fn from_env(blob_dir: &Path, disk_watermark_bytes: u64) -> anyhow::Result<Self> {
        let default_cache = blob_dir.join("viewer-cache");
        let default_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Ok(Self {
            bind_addr: parse("VIEWER_BIND_ADDR", "127.0.0.1:8070")?,
            ui_dir: PathBuf::from(opt("VIEWER_UI_DIR", "hushai-viewer/ui")),
            cache_dir: std::env::var("VIEWER_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or(default_cache),
            ffmpeg_bin: opt("FFMPEG_BIN", "ffmpeg"),
            ffmpeg_concurrency: parse("VIEWER_FFMPEG_CONCURRENCY", &default_cpus.to_string())?,
            default_window_nanos: parse("VIEWER_DEFAULT_WINDOW_NANOS", &HOUR_NANOS.to_string())?,
            // 6h cap: comfortably covers a day's worth of intermittent capture as a single
            // playlist on localhost, while still bounding worst-case playlist size.
            max_window_nanos: parse("VIEWER_MAX_WINDOW_NANOS", &(6 * HOUR_NANOS).to_string())?,
            rag_base_url: opt("RAG_BASE_URL", "http://127.0.0.1:8090"),
            rag_token: std::env::var("RAG_TOKEN").ok().filter(|s| !s.trim().is_empty()),
            backend_base_url: opt("BACKEND_BASE_URL", "http://127.0.0.1:8080"),
            backend_token: std::env::var("BACKEND_TOKEN")
                .ok()
                .or_else(|| std::env::var("DEVICE_TOKEN").ok())
                .filter(|s| !s.trim().is_empty()),
            detections_max_rows: parse("VIEWER_DETECTIONS_MAX_ROWS", "50000")?,
            processing_max_rows: parse("VIEWER_PROCESSING_MAX_ROWS", "200000")?,
            blob_dir: blob_dir.to_path_buf(),
            disk_watermark_bytes,
            ollama_base_url: opt("OLLAMA_BASE_URL", "http://127.0.0.1:11434"),
            dash_connected_secs: parse("DASH_CONNECTED_SECS", "10")?,
            dash_idle_secs: parse("DASH_IDLE_SECS", "60")?,
            dash_worker_stale_secs: parse("DASH_WORKER_STALE_SECS", "30")?,
            dash_probe_timeout_ms: parse("DASH_PROBE_TIMEOUT_MS", "1500")?,
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
