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

pub mod config;
pub mod dashboard;
pub mod detections;
pub mod error;
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
use tracing_subscriber::EnvFilter;

use crate::config::ViewerConfig;
use crate::state::ViewerState;

pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hushai_viewer=debug,tower_http=info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Load config, connect, ensure the cache dir, and serve until Ctrl-C / SIGTERM.
pub async fn run() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    dotenvy::from_path("hushai-backend/.env").ok();
    init_tracing();

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
    let ffmpeg_sem = Arc::new(Semaphore::new(cfg.ffmpeg_concurrency.max(1)));
    let http = reqwest::Client::new();

    tracing::info!(
        %bind_addr,
        ffmpeg = %cfg.ffmpeg_bin,
        ffmpeg_concurrency = cfg.ffmpeg_concurrency,
        cache_dir = %cfg.cache_dir.display(),
        ui_dir = %cfg.ui_dir.display(),
        rag_base_url = %cfg.rag_base_url,
        "hushai-viewer starting"
    );

    let state = ViewerState {
        pool,
        cfg: Arc::new(cfg),
        http,
        ffmpeg_sem,
        inflight: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;
    tracing::info!(%bind_addr, "hushai-viewer listening — open http://{bind_addr}/");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    Ok(())
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
