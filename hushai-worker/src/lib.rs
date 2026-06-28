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
pub mod sentiment;
pub mod speaker;
pub mod speaker_match;
pub mod vad;
pub mod vision;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::Notify;
use tracing_subscriber::EnvFilter;

use crate::asr::Transcriber;
use crate::config::WorkerConfig;
use crate::embed::Embedder;
use crate::sentiment::SentimentClassifier;
use crate::speaker::{SpeakerEmbedder, VoiceDetector};
use crate::vision::write::VisionModels;

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
    // Same for the raw per-segment voiceprints (speaker_segments is RANGE-partitioned too).
    sqlx::query("SELECT ensure_speaker_segment_partitions(3)")
        .execute(&pool)
        .await
        .context("ensuring speaker_segment partitions")?;
    // And the vision tables (person_segments + scene_objects are RANGE-partitioned, migration 0009).
    sqlx::query("SELECT ensure_person_segment_partitions(3)")
        .execute(&pool)
        .await
        .context("ensuring person_segment partitions")?;
    sqlx::query("SELECT ensure_scene_object_partitions(3)")
        .execute(&pool)
        .await
        .context("ensuring scene_object partitions")?;

    tracing::info!(
        whisper_model = %cfg.whisper_model_path,
        embed_model = %cfg.embed_model,
        embed_ollama = %cfg.embed_ollama_base_url,
        concurrency = cfg.worker_concurrency,
        "hushai-worker starting"
    );

    let transcriber = Transcriber::new(&cfg.whisper_model_path)?;
    let embedder = Embedder::new(&cfg.embed_ollama_base_url, &cfg.embed_model)?;
    let sentiment_clf = SentimentClassifier::new(
        &cfg.llm_ollama_base_url,
        &cfg.sentiment_model,
        Duration::from_millis(cfg.sentiment_timeout_ms),
        cfg.sentiment_enabled,
    )?;
    let speaker_embedder = SpeakerEmbedder::new(&cfg.speaker_model_path)
        .context("loading speaker embedding model (SPEAKER_MODEL_PATH)")?;
    let voice_detector = VoiceDetector::new(
        &cfg.vad_model_path,
        cfg.vad_threshold,
        cfg.vad_min_silence_secs,
        cfg.vad_min_speech_secs,
    )
    .context("loading VAD model (VAD_MODEL_PATH)")?;

    // Vision models (face detect + embed). Resilient: if disabled or any model/dylib is missing,
    // vision self-disables with a warning and the audio path keeps running (so an audio-only
    // deployment without vision models still works).
    let vision_models: Option<VisionModels> = if cfg.vision_enabled {
        match build_vision_models(&cfg) {
            Ok(m) => {
                tracing::info!(
                    detect = %cfg.face_detect_model_path,
                    embed = %cfg.face_embed_model_path,
                    "vision enabled (face identity)"
                );
                Some(m)
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "vision disabled: model load failed — audio path continues");
                None
            }
        }
    } else {
        None
    };

    let backfilled = claim::ensure_status_rows(&pool).await?;
    tracing::info!(newly_tracked = backfilled, "backlog status rows ensured");
    if vision_models.is_some() {
        let v = claim::ensure_vision_status_rows(&pool).await?;
        tracing::info!(newly_tracked = v, "vision backlog status rows ensured");
    }

    // One-shot (after a calibration change): clear reject tombstones so those segments are
    // re-evaluated under the current gates. The reconcile below then re-queues them.
    if cfg.speaker_reprocess_rejects_on_start {
        let cleared = claim::clear_reject_tombstones(&pool).await?;
        tracing::info!(
            cleared,
            "speaker reprocess: cleared reject tombstones for re-evaluation"
        );
    }

    // Self-heal: re-queue already-transcribed audio that never got a voiceprint (e.g.
    // transcribed while the speaker stage was down), so those voices can still be
    // assigned/minted and reach the catalog. Convergent — a no-op once each segment
    // has a speaker_segments row.
    if cfg.speaker_backfill_on_start || cfg.speaker_reprocess_rejects_on_start {
        let requeued = claim::reconcile_missing_speaker_segments(&pool).await?;
        tracing::info!(
            requeued,
            "speaker backfill: re-queued done segments missing a voiceprint"
        );
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    spawn_shutdown_watcher(shutdown.clone());

    // New segments NOTIFY on commit (see hushai-backend); idle workers wait on this
    // instead of polling, so keep-up latency is ~immediate. The poll interval remains
    // a backstop for missed/lost notifications.
    let wake = Arc::new(Notify::new());
    spawn_ingest_listener(backend_cfg.database_url.clone(), wake.clone());

    let cfg = Arc::new(cfg);

    // Liveness heartbeat: the worker has no HTTP port, so it self-reports into `worker_heartbeat`
    // every cfg.heartbeat_interval. The viewer's /api/dashboard reads it (idle != dead). One row
    // per process (not per loop), keyed by cfg.worker_id (default "<host>:<pid>").
    spawn_heartbeat(pool.clone(), cfg.clone(), shutdown.clone());

    let mut handles = Vec::with_capacity(cfg.worker_concurrency);
    for worker_id in 0..cfg.worker_concurrency.max(1) {
        handles.push(tokio::spawn(worker_loop(
            worker_id,
            pool.clone(),
            transcriber.clone(),
            embedder.clone(),
            sentiment_clf.clone(),
            speaker_embedder.clone(),
            voice_detector.clone(),
            cfg.clone(),
            shutdown.clone(),
            wake.clone(),
        )));
    }
    // Vision worker (VIDEO/MUXED → face identity). One loop: vision inference is CPU-heavy and the
    // global person advisory lock serializes minting anyway; bump if profiling warrants.
    if let Some(models) = vision_models {
        handles.push(tokio::spawn(vision_worker_loop(
            pool.clone(),
            models,
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

/// Load the vision ONNX models (ORT load-dynamic + YuNet + ArcFace). Fails if the dylib or a model
/// file is missing — the caller treats that as "vision disabled" rather than aborting the worker.
fn build_vision_models(cfg: &WorkerConfig) -> anyhow::Result<VisionModels> {
    use crate::vision::{
        detect::FaceDetector,
        face_embed::FaceEmbedder,
        model,
        objects::{ClipEmbedder, ObjectDetector},
    };
    model::init_ort(&cfg.ort_dylib_path);
    let detector = FaceDetector::new(
        model::load_session(&cfg.face_detect_model_path, cfg.vision_coreml)
            .context("loading face-detect model (FACE_DETECT_MODEL_PATH)")?,
        cfg.face_min_det_score,
    );
    let embedder = FaceEmbedder::new(
        model::load_session(&cfg.face_embed_model_path, cfg.vision_coreml)
            .context("loading face-embed model (FACE_EMBED_MODEL_PATH)")?,
    );

    // Optional open-vocab object lane (RF-DETR + CLIP). Enabled only when BOTH models load; any
    // failure disables objects with a warning but keeps the (required) face lane running, so an
    // operator who hasn't provisioned RF-DETR/CLIP still gets person identity.
    let (object_detector, clip) = match (
        model::load_session(&cfg.object_det_model_path, cfg.vision_coreml),
        model::load_session(&cfg.clip_image_model_path, cfg.vision_coreml),
    ) {
        (Ok(od), Ok(cl)) => {
            tracing::info!(
                object_det = %cfg.object_det_model_path,
                clip = %cfg.clip_image_model_path,
                max_per_frame = cfg.object_max_per_frame,
                min_box_px = cfg.object_min_box_px,
                "vision object lane enabled (open-vocab objects) — validate the decode against the real export (see AGENTS.md vision)"
            );
            (
                Some(Arc::new(ObjectDetector::new(
                    od,
                    cfg.object_det_input_size,
                    cfg.object_min_det_score,
                    cfg.object_max_per_frame,
                    cfg.object_min_box_px,
                ))),
                Some(Arc::new(ClipEmbedder::new(cl))),
            )
        }
        _ if cfg.object_required => {
            // OBJECT_REQUIRED: the operator asked for objects; fail the vision subsystem loudly
            // (audio still runs) instead of silently degrading to face-only.
            anyhow::bail!(
                "OBJECT_REQUIRED=true but RF-DETR/CLIP models failed to load \
                 (OBJECT_DET_MODEL_PATH={}, CLIP_IMAGE_MODEL_PATH={})",
                cfg.object_det_model_path,
                cfg.clip_image_model_path
            );
        }
        _ => {
            tracing::warn!(
                "vision object lane disabled: RF-DETR/CLIP models not provisioned — face identity still runs"
            );
            (None, None)
        }
    };

    Ok(VisionModels {
        detector: Arc::new(detector),
        embedder: Arc::new(embedder),
        object_detector,
        clip,
    })
}

/// Drain VIDEO/MUXED segments through the face-identity pipeline (claim → process → done/error),
/// NOTIFY-woken with a poll backstop — mirrors `worker_loop` for the vision queue.
async fn vision_worker_loop(
    pool: PgPool,
    models: VisionModels,
    cfg: Arc<WorkerConfig>,
    shutdown: Arc<AtomicBool>,
    wake: Arc<Notify>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match claim::claim_one_vision(&pool, cfg.max_attempts, cfg.lease_timeout_secs).await {
            Ok(Some(segment_id)) => {
                match crate::vision::write::process_vision_segment(&pool, &models, &cfg, segment_id)
                    .await
                {
                    Ok(n) => {
                        if let Err(e) = claim::mark_vision_done(&pool, segment_id).await {
                            tracing::warn!(%segment_id, error = %e, "marking vision done failed");
                        }
                        tracing::debug!(%segment_id, faces = n, "vision segment processed");
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        tracing::warn!(%segment_id, error = %msg, "vision processing failed");
                        let _ = claim::mark_vision_error(&pool, segment_id, &msg).await;
                    }
                }
            }
            Ok(None) => {
                tokio::select! {
                    _ = wake.notified() => {}
                    _ = tokio::time::sleep(cfg.poll_interval) => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "vision claim failed; backing off");
                tokio::time::sleep(cfg.poll_interval).await;
            }
        }
    }
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

/// Self-report liveness into `worker_heartbeat` every `cfg.heartbeat_interval`. The worker has no
/// HTTP port, so this row is how the viewer's /api/dashboard tells "idle but healthy" from "dead".
/// One row per PROCESS (not per loop), keyed by `cfg.worker_id` (default `"<host>:<pid>"`); a
/// restart reuses the row via upsert. `started_at` is preserved on conflict (uptime). On clean
/// shutdown the row is deleted so the worker shows "down" immediately rather than going stale.
fn spawn_heartbeat(pool: PgPool, cfg: Arc<WorkerConfig>, shutdown: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let pid = std::process::id() as i32;
        let host = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .unwrap_or_else(|_| "worker".to_string());
        let worker_id = cfg
            .worker_id
            .clone()
            .unwrap_or_else(|| format!("{host}:{pid}"));
        let version = env!("CARGO_PKG_VERSION");
        let concurrency = cfg.worker_concurrency.max(1) as i32;

        while !shutdown.load(Ordering::SeqCst) {
            // Best-effort self-reported backlog (NULL if the count fails; never blocks the beat).
            let queue_depth: Option<i32> = sqlx::query_scalar(
                r#"
                SELECT (
                    (SELECT count(*) FROM segment_transcription_status WHERE status IN ('pending','processing'))
                  + (SELECT count(*) FROM segment_vision_status        WHERE status IN ('pending','processing'))
                )::int
                "#,
            )
            .fetch_one(&pool)
            .await
            .ok();

            let res = sqlx::query(
                r#"
                INSERT INTO worker_heartbeat
                    (worker_id, instance, pid, version, started_at, last_beat, concurrency, queue_depth)
                VALUES ($1, $2, $3, $4, now(), now(), $5, $6)
                ON CONFLICT (worker_id) DO UPDATE SET
                    instance    = EXCLUDED.instance,
                    pid         = EXCLUDED.pid,
                    version     = EXCLUDED.version,
                    last_beat   = now(),
                    concurrency = EXCLUDED.concurrency,
                    queue_depth = EXCLUDED.queue_depth
                "#,
            )
            .bind(&worker_id)
            .bind(&host)
            .bind(pid)
            .bind(version)
            .bind(concurrency)
            .bind(queue_depth)
            .execute(&pool)
            .await;
            if let Err(e) = res {
                tracing::warn!(error = %e, "heartbeat upsert failed; retry next interval");
            }

            tokio::time::sleep(cfg.heartbeat_interval).await;
        }

        let _ = sqlx::query("DELETE FROM worker_heartbeat WHERE worker_id = $1")
            .bind(&worker_id)
            .execute(&pool)
            .await;
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

#[allow(clippy::too_many_arguments)]
async fn worker_loop(
    worker_id: usize,
    pool: PgPool,
    transcriber: Transcriber,
    embedder: Embedder,
    sentiment_clf: SentimentClassifier,
    speaker_embedder: SpeakerEmbedder,
    voice_detector: VoiceDetector,
    cfg: Arc<WorkerConfig>,
    shutdown: Arc<AtomicBool>,
    wake: Arc<Notify>,
) {
    // Only worker 0 runs the going-forward auto-merge, so it's never run concurrently and
    // needs no shared state. Initialized to now() so the first pass waits one interval.
    let mut last_autoheal = std::time::Instant::now();
    while !shutdown.load(Ordering::SeqCst) {
        match claim::claim_one(&pool, cfg.max_attempts, cfg.lease_timeout_secs).await {
            Ok(Some(segment_id)) => {
                match process::process_segment(
                    &pool,
                    &transcriber,
                    &embedder,
                    &sentiment_clf,
                    &speaker_embedder,
                    &voice_detector,
                    &cfg,
                    segment_id,
                )
                .await
                {
                    Ok(n) => {
                        tracing::info!(%segment_id, sentences = n, worker_id, "processed segment");
                    }
                    Err(e) => {
                        tracing::error!(%segment_id, worker_id, error = format!("{e:#}"), "segment failed");
                        if let Err(e2) =
                            claim::mark_error(&pool, segment_id, &format!("{e:#}")).await
                        {
                            tracing::error!(%segment_id, error = %e2, "could not record error status");
                        }
                    }
                }
            }
            Ok(None) => {
                // Queue drained. Opportunistically auto-merge near-certain duplicate voices
                // among RECENTLY-ACTIVE speakers (so new static-induced splits fold into the
                // right identity without sweeping the historical backlog). Worker 0 only,
                // rate-limited; the backend fn takes the global advisory lock so it's safe.
                if worker_id == 0
                    && cfg.speaker_autoheal_enabled
                    && last_autoheal.elapsed().as_secs() >= cfg.speaker_autoheal_interval_secs
                {
                    last_autoheal = std::time::Instant::now();
                    let opts = hushai_backend::speakers::AutoMergeOpts {
                        edge_distance: cfg.speaker_autoheal_distance,
                        knn_k: cfg.speaker_autoheal_knn_k,
                        min_link_count: cfg.speaker_autoheal_min_links,
                        recent_secs: cfg.speaker_autoheal_recent_secs,
                    };
                    match hushai_backend::speakers::auto_merge_recent(&pool, opts).await {
                        Ok(stats) if stats.ids_removed > 0 => {
                            tracing::info!(
                                ids_removed = stats.ids_removed,
                                "auto-merged near-certain duplicate voices"
                            );
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "auto-merge pass failed"),
                    }
                }
                // Wait for a new-segment NOTIFY or the poll backstop, whichever comes first.
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
