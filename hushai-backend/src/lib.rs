//! Hushai data-intake backend — durable, idempotent segment-ingest server.
//!
//! Implements the camera→backend contract v0.1.0 (`contracts/cameraToBackendContract.md`):
//! `POST /v1/segments` accepts a `multipart/form-data` (`manifest` protobuf + opaque
//! `body`) and persists it exactly-once at rest. See the module docs for the durable
//! write path (`storage`), the idempotency transaction (`db`), and the source-agnostic
//! invariant (§7) — the backend NEVER branches on `source_kind`.

pub mod audit;
pub mod auth;
pub mod config;
pub mod db;
pub mod devices;
pub mod error;
pub mod events;
pub mod hints;
pub mod ingest;
pub mod logging;
pub mod observe;
pub mod persons;
pub mod plates;
pub mod proto;
pub mod routes;
pub mod speakers;
pub mod state;
pub mod storage;
pub mod tls;
pub mod watchlist;

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tokio::sync::Semaphore;

use crate::auth::TokenStore;
use crate::config::Config;
use crate::state::AppState;

/// Initialise tracing once, honouring `RUST_LOG` / `LOG_FORMAT` / `LOG_DIR`. Delegates to the
/// shared [`logging`] module so every service formats, files, and panic-captures identically.
pub fn init_tracing() {
    logging::init("hushai-backend", "info,hushai_backend=debug");
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

    // Per-device tokens (DEVICE_TOKENS, `label:token,…`) supersede the single
    // DEVICE_TOKEN when set, so a lost device can be revoked individually.
    let tokens = Arc::new(match &config.device_tokens {
        Some(spec) => TokenStore::from_spec(spec),
        None => TokenStore::single(config.device_token.clone()),
    });
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
    let tls = config.tls.clone();
    // Redacted startup banner: enough to debug a misconfigured deploy, with no secrets
    // (DEVICE_TOKEN / DATABASE_URL credentials are deliberately omitted).
    tracing::info!(
        service = "hushai-backend",
        version = env!("CARGO_PKG_VERSION"),
        %bind_addr,
        tls = tls.is_some(),
        blob_dir = %config.blob_dir.display(),
        logging = %logging::summary(),
        "starting"
    );
    let state = build_state(config).await?;

    // Observability (roadmap B1): self-describe the ingest metrics so `/metrics` is documented.
    observe::record_build_info("backend");
    observe::describe(
        "hushai_segments_ingested_total",
        "counter",
        "Segments accepted at /v1/segments, by source/media/result(new|duplicate).",
    );
    observe::describe(
        "hushai_ingest_bytes_total",
        "counter",
        "Bytes of NEW segment media durably stored, by source/media.",
    );

    // Per-device retention: a background task that purges footage past each device's keep-last-N-days
    // policy (once at startup, then on an interval). Idempotent, so it's safe regardless of restarts.
    devices::spawn_retention_task(state.clone());

    let app = routes::router(state);

    tracing::info!(%bind_addr, tls = tls.is_some(), "hushai-backend listening");
    crate::tls::serve(bind_addr, app, tls, shutdown_signal()).await
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
