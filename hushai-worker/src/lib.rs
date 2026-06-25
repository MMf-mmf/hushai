//! Hushai transcription + embedding worker.
//!
//! A durable, resumable, idempotent background service that drains the segment
//! backlog stored by `hushai-backend`, transcribes each segment's audio (local
//! whisper.cpp), embeds the sentences (local Ollama, 1024-dim), and writes
//! `transcript_sentences`. After the backlog is drained it keeps running and picks
//! up newly-ingested segments on a poll interval.
//!
//! Modules are public so integration tests can exercise the claim/lease + atomic
//! write logic directly against a live DB.

pub mod asr;
pub mod chunk;
pub mod claim;
pub mod config;
pub mod embed;
pub mod media;
pub mod process;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::Notify;
use tracing_subscriber::EnvFilter;

use crate::asr::Transcriber;
use crate::config::WorkerConfig;
use crate::embed::Embedder;

pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hushai_worker=debug"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Load config, connect, migrate, and run worker tasks until Ctrl-C / SIGTERM.
pub async fn run() -> anyhow::Result<()> {
    // Load env from cwd (.env) and fall back to the backend's local env file so
    // `cargo run -p hushai-worker` from the workspace root just works.
    dotenvy::dotenv().ok();
    dotenvy::from_path("hushai-backend/.env").ok();
    init_tracing();

    let cfg = WorkerConfig::from_env()?;

    // Reuse the schema-owning crate's Config + pool builder (the path dependency).
    let backend_cfg = hushai_backend::config::Config::from_env()
        .context("loading shared backend config (DATABASE_URL/BLOB_DIR/DEVICE_TOKEN)")?;
    let pool = hushai_backend::db::connect(&backend_cfg)
        .await
        .context("connecting to Postgres")?;

    // Ensure migrations (status table, HNSW index, partitioning) are applied.
    sqlx::migrate!("../hushai-backend/migrations")
        .run(&pool)
        .await
        .context("running migrations")?;

    // Keep monthly partitions of transcript_sentences ahead of incoming inserts. This
    // buys a few months of headroom; a scheduled job (cron / pg_cron) should also call
    // `SELECT ensure_transcript_partitions(N)` so long-running deployments never fall
    // back to the DEFAULT partition. Retention is `SELECT drop_transcript_partitions_before($cutoff)`.
    sqlx::query("SELECT ensure_transcript_partitions(3)")
        .execute(&pool)
        .await
        .context("ensuring transcript partitions")?;

    tracing::info!(
        whisper_model = %cfg.whisper_model_path,
        embed_model = %cfg.embed_model,
        embed_ollama = %cfg.embed_ollama_base_url,
        concurrency = cfg.worker_concurrency,
        "hushai-worker starting"
    );

    let transcriber = Transcriber::new(&cfg.whisper_model_path)?;
    let embedder = Embedder::new(&cfg.embed_ollama_base_url, &cfg.embed_model)?;

    let backfilled = claim::ensure_status_rows(&pool).await?;
    tracing::info!(newly_tracked = backfilled, "backlog status rows ensured");

    let shutdown = Arc::new(AtomicBool::new(false));
    spawn_shutdown_watcher(shutdown.clone());

    // New segments NOTIFY on commit (see hushai-backend); idle workers wait on this
    // instead of polling, so keep-up latency is ~immediate. The poll interval remains
    // a backstop for missed/lost notifications.
    let wake = Arc::new(Notify::new());
    spawn_ingest_listener(backend_cfg.database_url.clone(), wake.clone());

    let cfg = Arc::new(cfg);
    let mut handles = Vec::with_capacity(cfg.worker_concurrency);
    for worker_id in 0..cfg.worker_concurrency.max(1) {
        handles.push(tokio::spawn(worker_loop(
            worker_id,
            pool.clone(),
            transcriber.clone(),
            embedder.clone(),
            cfg.clone(),
            shutdown.clone(),
            wake.clone(),
        )));
    }
    for h in handles {
        let _ = h.await;
    }

    tracing::info!("hushai-worker stopped");
    Ok(())
}

fn spawn_shutdown_watcher(shutdown: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        #[cfg(unix)]
        let term = async {
            if let Ok(mut s) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                s.recv().await;
            }
        };
        #[cfg(not(unix))]
        let term = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {}
            _ = term => {}
        }
        tracing::info!("shutdown signal received; finishing in-flight work then exiting");
        shutdown.store(true, Ordering::SeqCst);
    });
}

/// Listen for `hushai_segment_ingested` notifications and wake idle workers. Runs for
/// the life of the process, reconnecting on connection loss (so a DB blip just falls
/// back to poll-interval keep-up until the listener re-establishes).
fn spawn_ingest_listener(database_url: String, wake: Arc<Notify>) {
    tokio::spawn(async move {
        loop {
            match PgListener::connect(&database_url).await {
                Ok(mut listener) => {
                    if let Err(e) = listener.listen("hushai_segment_ingested").await {
                        tracing::warn!(error = %e, "LISTEN failed; retrying");
                    } else {
                        tracing::debug!("listening for new-segment notifications");
                        loop {
                            match listener.recv().await {
                                Ok(_) => wake.notify_waiters(),
                                Err(e) => {
                                    tracing::warn!(error = %e, "notify recv failed; reconnecting");
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "PgListener connect failed; retrying"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
}

async fn worker_loop(
    worker_id: usize,
    pool: PgPool,
    transcriber: Transcriber,
    embedder: Embedder,
    cfg: Arc<WorkerConfig>,
    shutdown: Arc<AtomicBool>,
    wake: Arc<Notify>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match claim::claim_one(&pool, cfg.max_attempts, cfg.lease_timeout_secs).await {
            Ok(Some(segment_id)) => {
                match process::process_segment(&pool, &transcriber, &embedder, &cfg, segment_id)
                    .await
                {
                    Ok(n) => {
                        tracing::info!(%segment_id, sentences = n, worker_id, "processed segment");
                    }
                    Err(e) => {
                        tracing::error!(%segment_id, worker_id, error = format!("{e:#}"), "segment failed");
                        if let Err(e2) = claim::mark_error(&pool, segment_id, &format!("{e:#}")).await
                        {
                            tracing::error!(%segment_id, error = %e2, "could not record error status");
                        }
                    }
                }
            }
            Ok(None) => {
                // Queue drained. Wait for a new-segment NOTIFY or the poll backstop,
                // whichever comes first. Status rows are now created at ingest, so no
                // periodic full-table backfill scan is needed here.
                tokio::select! {
                    _ = wake.notified() => {}
                    _ = tokio::time::sleep(cfg.poll_interval) => {}
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "claim query failed");
                tokio::time::sleep(cfg.poll_interval).await;
            }
        }
    }
}
