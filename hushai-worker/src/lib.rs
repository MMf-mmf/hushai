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

pub mod alerts;
pub mod asr;
pub mod chunk;
pub mod claim;
pub mod config;
pub mod delivery;
pub mod embed;
pub mod events_producer;
pub mod governor;
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

use crate::asr::Transcriber;
use crate::config::WorkerConfig;
use crate::embed::Embedder;
use crate::governor::Governor;
use crate::sentiment::SentimentClassifier;
use crate::speaker::{SpeakerEmbedder, VoiceDetector};
use crate::vision::write::VisionModels;

/// Honours `RUST_LOG` / `LOG_FORMAT` / `LOG_DIR` via the shared [`hushai_backend::logging`] module.
pub fn init_tracing() {
    hushai_backend::logging::init("hushai-worker", "info,hushai_worker=debug");
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
    // And the ALPR detections table (plate_detections is RANGE-partitioned, migration 0013).
    sqlx::query("SELECT ensure_plate_detection_partitions(3)")
        .execute(&pool)
        .await
        .context("ensuring plate_detection partitions")?;

    tracing::info!(
        whisper_model = %cfg.whisper_model_path,
        embed_model = %cfg.embed_model,
        embed_ollama = %cfg.embed_ollama_base_url,
        concurrency = cfg.worker_concurrency,
        "hushai-worker starting"
    );

    // CPU thread budget: with N audio + V vision loops each running inference, "all cores per call"
    // oversubscribes the box. We size whisper n_threads and ORT intra-op threads to
    // cores/(audio+vision) so the loops fill the CPU instead of thrashing it. Log it once — it's
    // the first thing to check when reading a loadtest run.
    tracing::info!(
        cores = WorkerConfig::cores(),
        audio_loops = cfg.worker_concurrency,
        vision_loops = cfg.vision_concurrency,
        asr_threads = cfg.asr_n_threads(),
        ort_intra_threads = cfg.ort_intra_op_threads(),
        "concurrency budget"
    );

    let transcriber = Transcriber::new(&cfg.whisper_model_path, cfg.asr_n_threads())?.with_quality(
        crate::asr::DecodeQuality {
            no_speech_thold: cfg.whisper_no_speech_thold,
            logprob_thold: cfg.whisper_logprob_thold,
            entropy_thold: cfg.whisper_entropy_thold,
            suppress_nst: cfg.whisper_suppress_nst,
        },
    );
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

    // Device load governor: a monitor task publishes a Normal/Elevated/Saturated level the worker
    // loops read each iteration to pace themselves (pause the expensive vision lane + inter-segment
    // cooldown) under load, so the box is never thrashed. Nothing is dropped — deferral only delays;
    // the durable queue drains oldest-first once load clears. Disabled ⇒ legacy always-claim behavior.
    let governor = Arc::new(Governor::new(cfg.governor.clone()));
    governor::spawn_load_monitor(pool.clone(), governor.clone(), shutdown.clone());

    // Liveness heartbeat: the worker has no HTTP port, so it self-reports into `worker_heartbeat`
    // every cfg.heartbeat_interval. The viewer's /api/dashboard reads it (idle != dead). One row
    // per process (not per loop), keyed by cfg.worker_id (default "<host>:<pid>").
    spawn_heartbeat(pool.clone(), cfg.clone(), shutdown.clone());

    // Notification delivery (roadmap A4): drain the alert_deliveries outbox → outbound webhook POSTs
    // (retry + backoff). Independent of the segment-processing loops; no-op when disabled.
    delivery::spawn_delivery_loop(pool.clone(), cfg.delivery.clone(), shutdown.clone());

    // Observability (roadmap B1/B5): the worker has no axum router, so it gets a tiny raw-tokio
    // /metrics + /healthz server. Describe its metrics so the scrape is self-documenting.
    if let Some(addr) = cfg.metrics_addr {
        use hushai_backend::observe;
        observe::record_build_info("worker");
        observe::describe("hushai_worker_queue_depth", "gauge", "Pending+processing segments, by lane (audio|vision).");
        observe::describe("hushai_segments_processed_total", "counter", "Segments processed by a lane, by lane + result(ok|error).");
        observe::describe("hushai_audio_segments_skipped_silent_total", "counter", "Audio segments skipped before whisper because no speech was detected, by reason(rms|vad).");
        observe::describe("hushai_worker_segments_skipped_static_total", "counter", "Vision segments skipped because the scene was unchanged vs the camera's last analyzed frame, by lane(vision).");
        observe::describe("hushai_events_produced_total", "counter", "Event emit operations by event_type (incl. session re-extension UPSERTs, so >= distinct events).");
        observe::describe("hushai_alerts_fired_total", "counter", "Alert deliveries created by the rule evaluator (sum across rules x channels).");
        observe::describe("hushai_deliveries_total", "counter", "Webhook delivery outcomes: sent|failed are TERMINAL; retry counts scheduled (non-terminal) retries.");
        // Load-test / capacity instrumentation: per-stage and end-to-end latency histograms.
        observe::describe("hushai_worker_stage_seconds", "histogram", "Per-stage processing latency (s), by lane(audio|vision) + stage.");
        observe::describe("hushai_worker_segment_seconds", "histogram", "End-to-end per-segment processing wall-clock (s), by lane.");
        observe::describe("hushai_worker_capture_lag_seconds", "histogram", "Capture-to-done latency (s), incl. queue wait, by lane — the realtime-keep-up signal.");
        observe::describe("hushai_worker_load_level", "gauge", "Device load governor level: 0=Normal, 1=Elevated, 2=Saturated.");
        observe::describe("hushai_worker_throttle_total", "counter", "Times a lane paused/cooled to protect the device, by lane + reason(vision_paused|audio_paused|cooldown).");
        observe::spawn_metrics_server(addr, shutdown.clone());
    }

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
            governor.clone(),
        )));
    }
    // Vision workers (VIDEO/MUXED → face/object/plate identity). Fan out `VISION_CONCURRENCY`
    // loops over the SKIP-LOCKED vision queue, mirroring the audio fan-out above. `VisionModels`
    // is a cheap `Arc` clone (ORT `Session` is `Send+Sync` and `Run` is thread-safe), so all loops
    // share ONE set of models with no duplication; each segment is still processed start-to-finish
    // on a single loop, so intra-segment frame ordering (plate clustering) is preserved. The global
    // person advisory lock still serializes minting across loops — by design (cross-device identity).
    // Vision loops are identity-less: the "worker 0 only" auto-merge lives solely in the audio loop.
    if let Some(models) = vision_models {
        for _ in 0..cfg.vision_concurrency.max(1) {
            handles.push(tokio::spawn(vision_worker_loop(
                pool.clone(),
                models.clone(),
                cfg.clone(),
                shutdown.clone(),
                wake.clone(),
                governor.clone(),
            )));
        }
    }

    for h in handles {
        let _ = h.await;
    }

    tracing::info!("hushai-worker stopped");
    Ok(())
}

/// Build the configured face detector (`Arc<dyn FaceDetect>`), falling back to whichever model IS
/// provisioned: a dev box without SCRFD weights still gets identity via YuNet, and vice-versa.
fn build_face_detector(
    cfg: &WorkerConfig,
) -> anyhow::Result<Arc<dyn crate::vision::detect::FaceDetect>> {
    use crate::vision::{
        detect::{FaceDetect, FaceDetector},
        detect_scrfd::ScrfdDetector,
        enhance::DetectorKind,
        model,
    };
    let coreml = cfg.vision_coreml;
    let intra = cfg.ort_intra_op_threads();
    let scrfd = || -> anyhow::Result<Arc<dyn FaceDetect>> {
        Ok(Arc::new(ScrfdDetector::new(
            model::load_session_with_threads(&cfg.face_scrfd_model_path, coreml, intra)?,
            cfg.face_min_det_score,
        )))
    };
    let yunet = || -> anyhow::Result<Arc<dyn FaceDetect>> {
        Ok(Arc::new(FaceDetector::new(
            model::load_session_with_threads(&cfg.face_detect_model_path, coreml, intra)?,
            cfg.face_min_det_score,
        )))
    };
    match cfg.face_detector_kind {
        DetectorKind::Scrfd => match scrfd() {
            Ok(d) => {
                tracing::info!(model = %cfg.face_scrfd_model_path, "face detector: SCRFD");
                Ok(d)
            }
            Err(e) => {
                tracing::warn!(error = %e, "SCRFD failed to load; falling back to YuNet (provision scrfd_10g_bnkps.onnx — see fetch_scrfd.sh)");
                yunet().context("loading YuNet fallback face detector (FACE_DETECT_MODEL_PATH)")
            }
        },
        DetectorKind::YuNet => match yunet() {
            Ok(d) => {
                tracing::info!(model = %cfg.face_detect_model_path, "face detector: YuNet");
                Ok(d)
            }
            Err(e) => {
                tracing::warn!(error = %e, "YuNet failed to load; trying SCRFD");
                scrfd().context("loading SCRFD fallback face detector (FACE_SCRFD_MODEL_PATH)")
            }
        },
    }
}

/// Load the vision ONNX models (ORT load-dynamic + detector + ArcFace, plus the optional cleanup +
/// object lanes). Fails if the dylib or a REQUIRED model is missing — the caller treats that as
/// "vision disabled" rather than aborting the worker.
fn build_vision_models(cfg: &WorkerConfig) -> anyhow::Result<VisionModels> {
    use crate::vision::{
        enhance::{FaceRestorer, Upscaler},
        face_embed::FaceEmbedder,
        model,
        objects::{ClipEmbedder, ObjectDetector},
        plates::{detect::PlateDetector, ocr::PlateOcr},
    };
    model::init_ort(&cfg.ort_dylib_path);
    // Per-session ORT intra-op thread budget (see WorkerConfig::ort_intra_op_threads): keeps N
    // parallel vision loops from each spawning an all-core thread pool. CoreML nodes are unaffected.
    let intra = cfg.ort_intra_op_threads();
    let detector = build_face_detector(cfg)?;
    let embedder = FaceEmbedder::new(
        model::load_session_with_threads(&cfg.face_embed_model_path, cfg.vision_coreml, intra)
            .context("loading face-embed model (FACE_EMBED_MODEL_PATH)")?,
    )
    .with_flip_tta(cfg.face_embed_flip_tta);

    // Optional image-cleanup sub-lane (blind-face-restore + super-res). Each self-disables on a
    // missing/unloadable model; low-quality faces are then dropped as before (the face lane runs).
    let restorer = match model::load_session_with_threads(
        &cfg.face_restore_model_path,
        cfg.vision_coreml,
        intra,
    ) {
        Ok(s) => {
            tracing::info!(
                model = %cfg.face_restore_model_path,
                kind = ?cfg.face_restore_kind,
                "face restoration enabled (recover-then-embed for low-quality faces) — validate the decode against the real export"
            );
            Some(Arc::new(FaceRestorer::new(
                s,
                cfg.face_restore_kind,
                cfg.face_restore_codeformer_w,
            )))
        }
        Err(e) => {
            tracing::warn!(error = %e, "face restoration disabled: model not provisioned (FACE_RESTORE_MODEL_PATH) — low-quality faces are dropped as before");
            None
        }
    };
    let upscaler = match model::load_session_with_threads(
        &cfg.face_upscale_model_path,
        cfg.vision_coreml,
        intra,
    ) {
        Ok(s) => {
            tracing::info!(model = %cfg.face_upscale_model_path, "face super-resolution enabled (Real-ESRGAN)");
            Some(Arc::new(Upscaler::new(s)))
        }
        Err(_) => None,
    };

    // Optional open-vocab object lane (RF-DETR + CLIP). Enabled only when BOTH models load; any
    // failure disables objects with a warning but keeps the (required) face lane running, so an
    // operator who hasn't provisioned RF-DETR/CLIP still gets person identity.
    let (object_detector, clip) = match (
        model::load_session_with_threads(&cfg.object_det_model_path, cfg.vision_coreml, intra),
        model::load_session_with_threads(&cfg.clip_image_model_path, cfg.vision_coreml, intra),
    ) {
        (Ok(od), Ok(cl)) => {
            let mut detector = ObjectDetector::new(
                od,
                cfg.object_det_input_size,
                cfg.object_min_det_score,
                cfg.object_max_per_frame,
                cfg.object_min_box_px,
            )
            .with_nms_iou(cfg.object_nms_iou);
            // Apply the authoritative column→label map (RF-DETR's COCO 91-slot layout). The built-in
            // map is already correct; this honors a custom export's classes file when present.
            match crate::vision::objects::load_class_map(std::path::Path::new(&cfg.object_classes_path)) {
                Ok(names) => detector = detector.with_class_names(names),
                Err(e) => tracing::warn!(
                    path = %cfg.object_classes_path,
                    error = %e,
                    "object class map not loaded; using built-in COCO-91 map"
                ),
            }
            tracing::info!(
                object_det = %cfg.object_det_model_path,
                clip = %cfg.clip_image_model_path,
                classes = detector.named_class_count(),
                max_per_frame = cfg.object_max_per_frame,
                min_box_px = cfg.object_min_box_px,
                "vision object lane enabled (open-vocab objects, COCO-91 class map)"
            );
            (
                Some(Arc::new(detector)),
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

    // Optional ALPR lane (plate detector + OCR). Needs RF-DETR (object_detector) for vehicle ROIs;
    // self-disables if either plate model (or the OCR charset) is missing — faces/objects still run.
    let (plate_detector, plate_ocr) = if !cfg.plate_enabled {
        (None, None)
    } else {
        let det = model::load_session_with_threads(
            &cfg.plate_detect_model_path,
            cfg.vision_coreml,
            intra,
        )
        .map(|s| {
            Arc::new(PlateDetector::new(
                s,
                cfg.plate_detect_input_size,
                cfg.plate_min_det_score,
                cfg.plate_detect_end2end,
            ))
        });
        let ocr = load_plate_charset(&cfg.plate_ocr_charset_path).and_then(|charset| {
            model::load_session_with_threads(&cfg.plate_ocr_model_path, cfg.vision_coreml, intra)
                .map(|s| Arc::new(PlateOcr::new(s, charset).with_ctc(cfg.plate_ocr_ctc)))
        });
        match (det, ocr) {
            (Ok(d), Ok(o)) => {
                tracing::info!(
                    detector = %cfg.plate_detect_model_path,
                    ocr = %cfg.plate_ocr_model_path,
                    "vision plate lane enabled (ALPR) — validate the decode against the real export (see AGENTS.md vision)"
                );
                (Some(d), Some(o))
            }
            _ if cfg.plate_required => anyhow::bail!(
                "PLATE_REQUIRED=true but the plate detector/OCR failed to load \
                 (PLATE_DETECT_MODEL_PATH={}, PLATE_OCR_MODEL_PATH={}, PLATE_OCR_CHARSET_PATH={})",
                cfg.plate_detect_model_path,
                cfg.plate_ocr_model_path,
                cfg.plate_ocr_charset_path
            ),
            _ => {
                tracing::warn!(
                    "vision plate lane disabled: detector/OCR/charset not provisioned — faces + objects still run"
                );
                (None, None)
            }
        }
    };

    Ok(VisionModels {
        detector,
        embedder: Arc::new(embedder),
        restorer,
        upscaler,
        object_detector,
        clip,
        plate_detector,
        plate_ocr,
        // One per-camera fingerprint map for the whole process, shared by every vision loop via
        // the cheap `Arc` clone in `run()`'s fan-out.
        motion_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    })
}

/// Load the plate-OCR class→char map (a JSON array of single-character strings) from the export's
/// sidecar. Each entry maps to its first char; missing/invalid file disables the OCR lane.
fn load_plate_charset(path: &str) -> anyhow::Result<Vec<char>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading plate OCR charset {path}"))?;
    let arr: Vec<String> = serde_json::from_str(&raw)
        .with_context(|| format!("parsing plate OCR charset {path} (want a JSON array of strings)"))?;
    anyhow::ensure!(!arr.is_empty(), "plate OCR charset {path} is empty");
    Ok(arr.iter().map(|s| s.chars().next().unwrap_or('?')).collect())
}

/// Drain VIDEO/MUXED segments through the face-identity pipeline (claim → process → done/error),
/// NOTIFY-woken with a poll backstop — mirrors `worker_loop` for the vision queue.
async fn vision_worker_loop(
    pool: PgPool,
    models: VisionModels,
    cfg: Arc<WorkerConfig>,
    shutdown: Arc<AtomicBool>,
    wake: Arc<Notify>,
    governor: Arc<Governor>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        // Load governor: under saturation, pause the expensive vision lane entirely so audio keeps
        // up and the box recovers. Pending vision rows just wait (never dropped) and drain
        // oldest-first once load clears — the organic quiet-time drain.
        if governor.vision_should_pause() {
            hushai_backend::observe::counter("hushai_worker_throttle_total", &[("lane", "vision"), ("reason", "vision_paused")]);
            tokio::select! {
                _ = wake.notified() => {}
                _ = tokio::time::sleep(cfg.poll_interval) => {}
            }
            continue;
        }
        match claim::claim_one_vision(&pool, cfg.max_attempts, cfg.lease_timeout_secs).await {
            Ok(Some(segment_id)) => {
                match crate::vision::write::process_vision_segment(&pool, &models, &cfg, segment_id)
                    .await
                {
                    Ok(n) => {
                        hushai_backend::observe::counter("hushai_segments_processed_total", &[("lane", "vision"), ("result", "ok")]);
                        if let Err(e) = claim::mark_vision_done(&pool, segment_id).await {
                            tracing::warn!(%segment_id, error = %e, "marking vision done failed");
                        }
                        tracing::debug!(%segment_id, faces = n, "vision segment processed");
                    }
                    Err(e) => {
                        if !claim::segment_exists(&pool, segment_id).await {
                            // Deleted mid-flight (footage/device delete or retention); status row
                            // is cascade-gone. Benign skip, not a failure.
                            tracing::debug!(%segment_id, "vision segment vanished mid-flight; skipping");
                        } else {
                            hushai_backend::observe::counter("hushai_segments_processed_total", &[("lane", "vision"), ("result", "error")]);
                            let msg = format!("{e:#}");
                            tracing::warn!(%segment_id, error = %msg, "vision processing failed");
                            let _ = claim::mark_vision_error(&pool, segment_id, &msg).await;
                        }
                    }
                }
                // Governor cooldown: pace the lane between segments at Elevated so the box keeps
                // headroom (no-op at Normal / when disabled; vision is fully paused at Saturated above).
                let cd = governor.cooldown();
                if !cd.is_zero() {
                    hushai_backend::observe::counter("hushai_worker_throttle_total", &[("lane", "vision"), ("reason", "cooldown")]);
                    tokio::time::sleep(cd).await;
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

            // Per-lane backlog gauges for Prometheus (roadmap B1) — only when the metrics server is on.
            // Literal SQL per lane (sqlx 0.9 requires &'static str — no format!).
            if cfg.metrics_addr.is_some() {
                let lanes: [(&str, &str); 2] = [
                    ("audio", "SELECT count(*) FROM segment_transcription_status WHERE status IN ('pending','processing')"),
                    ("vision", "SELECT count(*) FROM segment_vision_status WHERE status IN ('pending','processing')"),
                ];
                for (lane, sql) in lanes {
                    let depth: Option<i64> = sqlx::query_scalar(sql).fetch_one(&pool).await.ok();
                    if let Some(d) = depth {
                        hushai_backend::observe::gauge("hushai_worker_queue_depth", &[("lane", lane)], d);
                    }
                }
            }

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
    governor: Arc<Governor>,
) {
    // Only worker 0 runs the going-forward auto-merge, so it's never run concurrently and
    // needs no shared state. Initialized to now() so the first pass waits one interval.
    let mut last_autoheal = std::time::Instant::now();
    while !shutdown.load(Ordering::SeqCst) {
        // Load governor: by default audio is the lane we keep running (vision pauses first), so this
        // only fires when the operator inverted the priority (LOAD_PAUSE_VISION_FIRST=false).
        if governor.audio_should_pause() {
            hushai_backend::observe::counter("hushai_worker_throttle_total", &[("lane", "audio"), ("reason", "audio_paused")]);
            tokio::select! {
                _ = wake.notified() => {}
                _ = tokio::time::sleep(cfg.poll_interval) => {}
            }
            continue;
        }
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
                        hushai_backend::observe::counter("hushai_segments_processed_total", &[("lane", "audio"), ("result", "ok")]);
                        tracing::info!(%segment_id, sentences = n, worker_id, "processed segment");
                    }
                    Err(e) => {
                        if !claim::segment_exists(&pool, segment_id).await {
                            // Deleted mid-flight (footage/device delete or retention). The status row
                            // is cascade-gone, so there's nothing to mark; not a failure.
                            tracing::debug!(%segment_id, worker_id, "segment vanished mid-flight; skipping");
                        } else {
                            hushai_backend::observe::counter("hushai_segments_processed_total", &[("lane", "audio"), ("result", "error")]);
                            tracing::error!(%segment_id, worker_id, error = format!("{e:#}"), "segment failed");
                            if let Err(e2) =
                                claim::mark_error(&pool, segment_id, &format!("{e:#}")).await
                            {
                                tracing::error!(%segment_id, error = %e2, "could not record error status");
                            }
                        }
                    }
                }
                // Governor cooldown: pace the audio lane between segments under load so the box
                // keeps headroom (no-op at Normal / when disabled).
                let cd = governor.cooldown();
                if !cd.is_zero() {
                    hushai_backend::observe::counter("hushai_worker_throttle_total", &[("lane", "audio"), ("reason", "cooldown")]);
                    tokio::time::sleep(cd).await;
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
