//! Hushai timeline viewer — a read-only browser NVR.
//!
//! Stitches the stored ~2s segments of a device into one scrubbable HLS timeline:
//! each blob is lazily remuxed to an MPEG-TS segment (`-c copy`, cached by content
//! hash) and exposed via a windowed VOD playlist with absolute `PROGRAM-DATE-TIME`
//! and `DISCONTINUITY` at gaps/session boundaries. The bundled UI (served at `/`)
//! plays it with hls.js and maps wall-clock time to seeks.
//!
//! Reuses `hushai-backend` as a library for the DB `Config` + pool + shared migrations,
//! mirroring `hushai-worker`/`hushai-rag`. Strictly read-only against DB and blobs.

pub mod auth;
pub mod config;
pub mod dashboard;
pub mod detections;
pub mod error;
pub mod export;
pub mod playlist;
pub mod processing;
pub mod proxy;
pub mod remux;
pub mod routes;
pub mod state;
pub mod timeline;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tokio::sync::Semaphore;

use crate::config::ViewerConfig;
use crate::state::ViewerState;

/// Honours `RUST_LOG` / `LOG_FORMAT` / `LOG_DIR` via the shared [`hushai_backend::logging`] module.
pub fn init_tracing() {
    hushai_backend::logging::init("hushai-viewer", "info,hushai_viewer=debug,tower_http=info");
}

/// Load config, connect, ensure the cache dir, and serve until Ctrl-C / SIGTERM.
pub async fn run() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    dotenvy::from_path("hushai-backend/.env").ok();
    init_tracing();

    // Observability (roadmap B1).
    hushai_backend::observe::record_build_info("viewer");
    hushai_backend::observe::describe(
        "hushai_viewer_proxy_total",
        "counter",
        "Requests reverse-proxied by the viewer, by upstream(backend|rag).",
    );

    // Reuse the backend's DB Config + pool (path dependency), exactly like rag/worker.
    let backend_cfg = hushai_backend::config::Config::from_env()
        .context("loading shared backend config (DATABASE_URL/BLOB_DIR/DEVICE_TOKEN)")?;
    let pool = hushai_backend::db::connect(&backend_cfg)
        .await
        .context("connecting to Postgres")?;

    // Apply shared migrations (incl. 0005's device+time index used by the viewer).
    sqlx::migrate!("../hushai-backend/migrations")
        .run(&pool)
        .await
        .context("running migrations")?;

    let cfg = ViewerConfig::from_env(&backend_cfg.blob_dir, backend_cfg.disk_watermark_bytes)?;
    tokio::fs::create_dir_all(cfg.cache_dir.join("ts"))
        .await
        .with_context(|| format!("creating TS cache dir {}", cfg.cache_dir.display()))?;

    let bind_addr = cfg.bind_addr;
    let display_host = cfg.display_host.clone(); // captured before `cfg` moves into ViewerState
    let tls = cfg.tls.clone();
    let ffmpeg_sem = Arc::new(Semaphore::new(cfg.ffmpeg_concurrency.max(1)));
    let export_sem = Arc::new(Semaphore::new(cfg.export_concurrency.max(1)));
    // HTTP client for the `/v1/*` reverse-proxy + dashboard probes. When the sibling
    // backend/rag serve TLS behind a private LAN CA, trust that CA so the loopback
    // proxy/probe calls verify instead of failing the handshake.
    let http = {
        let mut builder = reqwest::Client::builder();
        if let Some(ca_path) = &cfg.upstream_ca {
            let pem = std::fs::read(ca_path)
                .with_context(|| format!("reading VIEWER_UPSTREAM_CA {}", ca_path.display()))?;
            let cert = reqwest::Certificate::from_pem(&pem)
                .with_context(|| format!("parsing VIEWER_UPSTREAM_CA {}", ca_path.display()))?;
            builder = builder.add_root_certificate(cert);
        }
        builder.build().context("building viewer HTTP client")?
    };

    tracing::info!(
        service = "hushai-viewer",
        version = env!("CARGO_PKG_VERSION"),
        %bind_addr,
        ffmpeg = %cfg.ffmpeg_bin,
        ffmpeg_concurrency = cfg.ffmpeg_concurrency,
        cache_dir = %cfg.cache_dir.display(),
        ui_dir = %cfg.ui_dir.display(),
        rag_base_url = %cfg.rag_base_url,
        logging = %hushai_backend::logging::summary(),
        "hushai-viewer starting"
    );

    let state = ViewerState {
        pool,
        cfg: Arc::new(cfg),
        http,
        ffmpeg_sem,
        export_sem,
        inflight: Arc::new(Mutex::new(HashMap::new())),
    };

    // Background TS-cache reaper — the remux cache is content-addressed and never self-evicts, so
    // without this it grows unbounded on the disk the watermark guards. Runs once at startup then on
    // the configured interval; 0 bytes disables it. Blocking prune off the runtime via spawn_blocking.
    if state.cfg.cache_max_bytes > 0 {
        let cache_dir = state.cfg.cache_dir.clone();
        let max_bytes = state.cfg.cache_max_bytes;
        let interval = std::time::Duration::from_secs(state.cfg.cache_reap_interval_secs.max(60));
        tokio::spawn(async move {
            loop {
                let dir = cache_dir.clone();
                match tokio::task::spawn_blocking(move || crate::remux::reap_cache(&dir, max_bytes)).await {
                    Ok(Ok((total, freed))) if freed > 0 => {
                        tracing::info!(total_bytes = total, freed_bytes = freed, "ts-cache reaped")
                    }
                    Ok(Err(e)) => tracing::warn!(error = %e, "ts-cache reap failed"),
                    _ => {}
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    let app = routes::router(state);
    // `serve_with_connect_info` populates `ConnectInfo<SocketAddr>` (the peer IP) on
    // BOTH the cleartext and TLS branches — the IP allowlist middleware (auth.rs) reads
    // it the same way in either world. Keep this serve variant whenever the IP gate is
    // in play; TLS terminates in-process so the peer addr is the real client.
    let scheme = if tls.is_some() { "https" } else { "http" };
    // Prefer the friendly host (VIEWER_HOSTNAME, e.g. hushai.local) in the "open …" hint when set
    // — and drop the port for it, since setup_hostname.sh redirects the default 443 → bind port.
    let open_url = match &display_host {
        Some(h) => format!("{scheme}://{h}/"),
        None => format!("{scheme}://{bind_addr}/"),
    };
    tracing::info!(%bind_addr, tls = tls.is_some(), "hushai-viewer listening — open {open_url}");
    hushai_backend::tls::serve_with_connect_info(bind_addr, app, tls, shutdown_signal()).await
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}
