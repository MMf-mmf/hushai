//! Viewer-specific configuration. DB/blob config is reused from
//! `hushai_backend::config::Config`; this struct holds only what the viewer adds.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::anyhow;

/// One hour and two hours, in nanoseconds — the default and max playable window.
const HOUR_NANOS: i64 = 3_600_000_000_000;

#[derive(Debug, Clone)]
pub struct ViewerConfig {
    /// Address/port the HTTP server binds to. Defaults to **localhost-only**; set
    /// `VIEWER_BIND_ADDR=0.0.0.0:8070` to let admin computers reach it (the IP allowlist
    /// + password gate then restrict who actually gets in — see `auth.rs`).
    pub bind_addr: SocketAddr,
    /// **Cosmetic only.** Friendly host shown in the startup "open …" log so it matches what a
    /// user types (e.g. `hushai.local` → "open https://hushai.local/"). From `VIEWER_HOSTNAME`.
    /// Does NOT affect the bind address or routing; `None` ⇒ log the bind addr. Resolution
    /// (Bonjour) + the no-port 443→8070 redirect are set up out-of-process by
    /// `local_dev/setup_hostname.sh`.
    pub display_host: Option<String>,
    /// Optional native TLS (`VIEWER_TLS_CERT_PATH`/`VIEWER_TLS_KEY_PATH`, falling back to
    /// the bare `TLS_CERT_PATH`/`TLS_KEY_PATH`). Both set ⇒ HTTPS; neither ⇒ cleartext.
    pub tls: Option<hushai_backend::tls::TlsPaths>,

    // --- Admin access control (the viewer IS the admin panel) ---
    /// Exact IPs and/or CIDRs that may reach ANY viewer route. From
    /// `VIEWER_ADMIN_IP_ALLOWLIST` (comma-separated, e.g. "192.168.1.10,192.168.1.0/24").
    /// Empty ⇒ loopback only (with `allow_loopback`).
    pub admin_ip_allowlist: Vec<ipnet::IpNet>,
    /// Always allow `127.0.0.1`/`::1` regardless of the list (host box + curl). Default true.
    pub allow_loopback: bool,
    /// Effective argon2 PHC hash of the admin password. From `VIEWER_ADMIN_PASSWORD_HASH`,
    /// else derived at startup from `VIEWER_ADMIN_PASSWORD`. `None` only when auth is disabled.
    pub admin_password_hash: Option<String>,
    /// HMAC key for signing session cookies. From `VIEWER_SESSION_SECRET` (set it for stable
    /// logins); a random per-boot secret is used (with a WARN) when unset.
    pub session_secret: Vec<u8>,
    /// Session lifetime (seconds). From `VIEWER_SESSION_TTL_SECS`, default 7 days.
    pub session_ttl_secs: i64,
    /// Disable the password gate for pure-local dev (`VIEWER_AUTH_DISABLED`). The IP gate
    /// still applies (loopback-only by default). Default false.
    pub auth_disabled: bool,
    /// Add `Secure` to the session cookie. From `VIEWER_COOKIE_SECURE`; defaults to whether
    /// TLS is configured.
    pub cookie_secure: bool,
    /// Directory of the bundled static UI (served at `/`).
    pub ui_dir: PathBuf,
    /// Where remuxed `.ts` segments are cached (pure derived data; safe to delete).
    pub cache_dir: PathBuf,
    /// ffmpeg binary used to remux blobs to MPEG-TS. Shared `FFMPEG_BIN` with the worker.
    pub ffmpeg_bin: String,
    /// Max concurrent ffmpeg processes (separate budget from the ingest backend).
    pub ffmpeg_concurrency: usize,
    /// Max concurrent footage EXPORTS. Each holds one permit for its whole (long) stream and
    /// internally drives per-segment remuxes on `ffmpeg_concurrency`, so it needs its own small
    /// budget — otherwise N exports fork N unbounded ffmpegs and starve the remux pool.
    pub export_concurrency: usize,
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
    /// Optional CA bundle (PEM) the proxy + dashboard-probe HTTP client trusts when the
    /// sibling backend/rag serve TLS with a private LAN CA (`VIEWER_UPSTREAM_CA`). Needed
    /// when `RAG_BASE_URL`/`BACKEND_BASE_URL` are `https://` with the self-signed CA.
    pub upstream_ca: Option<PathBuf>,
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

        // Admin access control (the viewer is the admin panel; see auth.rs). Resolve
        // these before the struct so `cookie_secure` can default off whether TLS is on.
        let tls = hushai_backend::tls::TlsPaths::from_env("VIEWER_")?;
        let auth_disabled = parse("VIEWER_AUTH_DISABLED", "false")?;
        let admin_ip_allowlist = parse_ip_allowlist("VIEWER_ADMIN_IP_ALLOWLIST")?;
        let allow_loopback = parse("VIEWER_ALLOW_LOOPBACK", "true")?;
        let cookie_secure = match std::env::var("VIEWER_COOKIE_SECURE") {
            Ok(v) if !v.trim().is_empty() => v
                .trim()
                .parse::<bool>()
                .map_err(|e| anyhow!("env var VIEWER_COOKIE_SECURE={v:?} is invalid: {e}"))?,
            _ => tls.is_some(),
        };
        let session_secret = resolve_session_secret();
        let admin_password_hash = resolve_admin_password_hash(auth_disabled)?;

        Ok(Self {
            bind_addr: parse("VIEWER_BIND_ADDR", "127.0.0.1:8070")?,
            display_host: std::env::var("VIEWER_HOSTNAME")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            tls,
            admin_ip_allowlist,
            allow_loopback,
            admin_password_hash,
            session_secret,
            session_ttl_secs: parse("VIEWER_SESSION_TTL_SECS", "604800")?,
            auth_disabled,
            cookie_secure,
            ui_dir: PathBuf::from(opt("VIEWER_UI_DIR", "hushai-viewer/ui")),
            cache_dir: std::env::var("VIEWER_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or(default_cache),
            ffmpeg_bin: opt("FFMPEG_BIN", "ffmpeg"),
            ffmpeg_concurrency: parse("VIEWER_FFMPEG_CONCURRENCY", &default_cpus.to_string())?,
            export_concurrency: parse("VIEWER_EXPORT_CONCURRENCY", "2")?,
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
            upstream_ca: std::env::var("VIEWER_UPSTREAM_CA")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from),
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

/// Parse a comma-separated admin allowlist of IPs and/or CIDRs. A bare IP becomes a
/// host route (/32 or /128). Fails closed: a malformed entry is a hard config error.
fn parse_ip_allowlist(key: &str) -> anyhow::Result<Vec<ipnet::IpNet>> {
    let raw = std::env::var(key).unwrap_or_default();
    let mut out = Vec::new();
    for entry in raw.split(',') {
        let e = entry.trim();
        if e.is_empty() {
            continue;
        }
        if let Ok(net) = e.parse::<ipnet::IpNet>() {
            out.push(net);
        } else if let Ok(ip) = e.parse::<std::net::IpAddr>() {
            out.push(ipnet::IpNet::from(ip));
        } else {
            return Err(anyhow!("{key}: '{e}' is not a valid IP or CIDR"));
        }
    }
    Ok(out)
}

/// HMAC key for the session cookie: `VIEWER_SESSION_SECRET` if set (use a long random
/// value for stable logins), else a random per-boot secret (sessions die on restart).
fn resolve_session_secret() -> Vec<u8> {
    match std::env::var("VIEWER_SESSION_SECRET") {
        Ok(s) if !s.trim().is_empty() => {
            let bytes = s.trim().as_bytes().to_vec();
            if bytes.len() < 32 {
                tracing::warn!(
                    "VIEWER_SESSION_SECRET is short (<32 bytes); use a longer random secret"
                );
            }
            bytes
        }
        _ => {
            use rand::RngCore;
            let mut b = vec![0u8; 32];
            rand::rng().fill_bytes(&mut b);
            tracing::warn!(
                "VIEWER_SESSION_SECRET unset — using a random per-boot secret; logins are \
                 invalidated on restart. Set it (e.g. in .env) for stable sessions."
            );
            b
        }
    }
}

/// Resolve the effective admin password hash: a supplied argon2 PHC hash wins; else
/// derive one from the plaintext `VIEWER_ADMIN_PASSWORD`; else error (unless auth is
/// disabled, in which case there is no password gate and we return `None`).
fn resolve_admin_password_hash(auth_disabled: bool) -> anyhow::Result<Option<String>> {
    if let Ok(h) = std::env::var("VIEWER_ADMIN_PASSWORD_HASH") {
        let h = h.trim().to_string();
        if !h.is_empty() {
            argon2::PasswordHash::new(&h).map_err(|e| {
                anyhow!("VIEWER_ADMIN_PASSWORD_HASH is not a valid argon2 PHC string: {e}")
            })?;
            return Ok(Some(h));
        }
    }
    if let Ok(p) = std::env::var("VIEWER_ADMIN_PASSWORD") {
        let p = p.trim();
        if !p.is_empty() {
            return Ok(Some(hash_password(p)?));
        }
    }
    if auth_disabled {
        tracing::warn!(
            "VIEWER_AUTH_DISABLED=true — the viewer password gate is OFF (the IP allowlist \
             still applies). Do not use this in a shared-network deployment."
        );
        return Ok(None);
    }
    Err(anyhow!(
        "no admin password configured: set VIEWER_ADMIN_PASSWORD (or \
         VIEWER_ADMIN_PASSWORD_HASH), or VIEWER_AUTH_DISABLED=true for pure-local dev"
    ))
}

fn hash_password(plain: &str) -> anyhow::Result<String> {
    use argon2::password_hash::SaltString;
    use argon2::{Argon2, PasswordHasher};
    use rand::RngCore;
    // Generate the salt with rand 0.9 (already a dep) so we don't need password-hash's
    // `getrandom`-gated `OsRng` (rand's ThreadRng is itself a CSPRNG seeded from the OS).
    let mut salt_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut salt_bytes);
    let salt =
        SaltString::encode_b64(&salt_bytes).map_err(|e| anyhow!("encoding password salt: {e}"))?;
    let hash = Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|e| anyhow!("hashing admin password: {e}"))?
        .to_string();
    Ok(hash)
}
