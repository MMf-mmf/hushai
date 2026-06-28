//! Hushai data-intake backend — durable, idempotent segment-ingest server.
//!
//! Implements the camera→backend contract v0.1.0 (`contracts/cameraToBackendContract.md`):
//! `POST /v1/segments` accepts a `multipart/form-data` (`manifest` protobuf + opaque
//! `body`) and persists it exactly-once at rest. See the module docs for the durable
//! write path (`storage`), the idempotency transaction (`db`), and the source-agnostic
//! invariant (§7) — the backend NEVER branches on `source_kind`.

pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod ingest;
pub mod persons;
pub mod proto;
pub mod routes;
pub mod speakers;
pub mod state;
pub mod storage;

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tokio::sync::Semaphore;
use tracing_subscriber::EnvFilter;

use crate::auth::TokenStore;
use crate::config::Config;
use crate::state::AppState;

/// Initialise tracing once, honouring `RUST_LOG`, defaulting to a sensible filter.
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hushai_backend=debug"));
    // `try_init` so repeated calls (e.g. in tests) don't panic.
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Build a fully-wired [`AppState`] from config: connect the pool, run migrations,
/// prepare the blob layout. Reused by both `run()` and integration tests.
pub async fn build_state(config: Config) -> anyhow::Result<AppState> {
    storage::ensure_layout(&config.blob_dir)
        .await
        .with_context(|| format!("preparing blob layout at {}", config.blob_dir.display()))?;
    let blob_root: Arc<Path> =
        Arc::from(std::fs::canonicalize(&config.blob_dir).context("canonicalize BLOB_DIR")?);

    let pool = db::connect(&config)
        .await
        .context("connecting to Postgres")?;
    sqlx::migrate!()
        .run(&pool)
        .await
        .context("running database migrations")?;

    let tokens = Arc::new(TokenStore::single(config.device_token.clone()));
    let limiter = Arc::new(Semaphore::new(config.concurrency_cap));

    Ok(AppState {
        pool,
        blob_root,
        tokens,
        limiter,
        config: Arc::new(config),
    })
}

/// Load config from the environment, build state, bind, and serve until SIGTERM/Ctrl-C.
pub async fn run() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    init_tracing();

    let config = Config::from_env()?;
    let bind_addr = config.bind_addr;
    let state = build_state(config).await?;
    let app = routes::router(state);

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;
    tracing::info!(%bind_addr, "hushai-backend listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    Ok(())
}

/// Resolve when the process should begin a graceful shutdown: Ctrl-C or SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl-C, shutting down gracefully"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down gracefully"),
    }
}
