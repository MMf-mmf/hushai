//! The per-segment vision pipeline + its single idempotent write transaction. The visual analogue
//! of `process::process_segment` + `write_transcript`:
//!   load segment -> sample frames -> (per frame, on the blocking pool) detect faces -> CLEAN UP the
//!   crop (margin -> super-resolve if tiny -> blind-face-restore -> align) -> quality-gate ->
//!   embed -> collect -> in ONE tx, advisory-locked match-or-mint into persons/person_segments.
//!
//! The cleanup is "recover-then-embed instead of reject": a small/blurry/distant face that the old
//! gate dropped is restored and, if it now clears the gate, attributed (it may MATCH a known person;
//! it MINTS a new one only when it was genuinely large + frontal). Already-clean faces skip
//! restoration entirely so their embeddings stay in the legacy ArcFace space. Restoration is
//! best-effort: a missing restorer model or a per-face failure just falls back to the raw crop.
//!
//! Reject-quality faces are dropped here (no row, no mint). A segment with no decodable video or no
//! usable face writes nothing and is not an error — it just marks vision status `done`. The optional
//! open-vocab object lane (RF-DETR + CLIP) writes `scene_objects` in the same tx.

use std::sync::Arc;

use anyhow::{Context, Result};
use image::RgbImage;

use super::detect::{Face, FaceDetect};
use super::enhance::{self, FaceRestorer, Upscaler};
use super::face_embed::{self, FaceEmbedder, FaceGates, FaceQuality};
use super::face_match::{self, FaceWrite};
use super::frames;
use super::geom;
use super::objects::{self, ClipEmbedder, DetectedObject, ObjectDetector};
use super::plates::detect::{PlateBox, PlateDetector};
use super::plates::normalize::{self as plate_norm, PlateGates, PlateQuality, PlateRead};
use super::plates::ocr::PlateOcr;
use super::plates::plate_match::{self, PlateWrite};
use super::plates::{is_vehicle, rectify};
use crate::config::WorkerConfig;
use crate::media;
use hushai_backend::observe;
use sqlx::PgPool;
use uuid::Uuid;

/// Histogram name for per-stage latency; lane is always "vision" in this module.
const STAGE: &str = "hushai_worker_stage_seconds";

/// Record end-to-end per-segment wall-clock + capture->done lag for the vision lane. Called on every
/// completion path so the load-test's per-camera cost includes decode+detect even when nothing is written.
fn record_vision_latency(started: std::time::Instant, capture_start_unix_nanos: i64) {
    observe::observe_duration(
        "hushai_worker_segment_seconds",
        &[("lane", "vision")],
        started.elapsed().as_secs_f64(),
    );
    crate::process::record_capture_lag("vision", capture_start_unix_nanos);
}

/// The vision models, loaded once at startup and cloned (cheap `Arc`) into each worker task.
/// Faces are required; the restoration sub-lane (super-res + face-restore) and the object lane
/// (RF-DETR + CLIP) are OPTIONAL — when their models aren't provisioned they stay `None` and the
/// face lane degrades gracefully (low-quality faces are simply dropped as before).
#[derive(Clone)]
pub struct VisionModels {
    pub detector: Arc<dyn FaceDetect>,
    pub embedder: Arc<FaceEmbedder>,
    /// Blind-face-restoration (GFPGAN / CodeFormer) — clean up low-quality crops before embedding.
    pub restorer: Option<Arc<FaceRestorer>>,
    /// Super-resolution (Real-ESRGAN) — zoom in on tiny crops before restoration; shared by the
    /// face lane AND the plate lane (super-resolving small rectified plates).
    pub upscaler: Option<Arc<Upscaler>>,
    pub object_detector: Option<Arc<ObjectDetector>>,
    pub clip: Option<Arc<ClipEmbedder>>,
    /// License-plate detector (inside vehicle ROIs). Needs `object_detector` (RF-DETR) too.
    pub plate_detector: Option<Arc<PlateDetector>>,
    /// License-plate OCR.
    pub plate_ocr: Option<Arc<PlateOcr>>,
}

/// CLIP image-embedding model tag stored on every `scene_objects` row (the open-vocab space).
const CLIP_MODEL_TAG: &str = "openclip-vit-b32";

/// Parameters for the face cleanup cascade (read once from config, passed into the blocking task).
#[derive(Clone, Copy)]
struct FaceEnhanceParams {
    gates: FaceGates,
    margin_frac: f32,
    restore_max_sharpness: f32,
    restore_min_px: f32,
    hard_min_px: f32,
    hard_min_det_score: f32,
    upscale_min_px: f32,
    mint_max_yaw_deg: f32,
    mint_max_pitch_deg: f32,
}

/// Per-face cascade output: the embedding + provenance + the cleaned crop image to (optionally)
/// persist as the thumbnail.
struct FaceOutcome {
    write: FaceWrite,
    cleaned_crop: RgbImage,
}

/// Parameters for the plate lane (read once from config, passed into the blocking task).
#[derive(Clone, Copy)]
struct PlateLaneParams {
    gates: PlateGates,
    roi_margin: f32,
    sr_min_side: f32,
    detect_whole_frame: bool,
}

/// One per-frame plate detection + read, before cross-frame clustering/voting.
struct PlateCand {
    read: PlateRead,
    det_score: f32,
    vehicle_bbox: Option<[f32; 4]>,
    vehicle_label: Option<String>,
    plate_bbox: [f32; 4],
    plate_corners: Option<[[f32; 2]; 4]>,
    frame_offset_nanos: i64,
    start_unix_nanos: i64,
    end_unix_nanos: i64,
    min_side: f32,
    /// Enhanced rectified plate image (the thumbnail candidate).
    crop: RgbImage,
}

/// One detected object ready to persist into `scene_objects` (region row, or the whole-frame
/// open-vocab row with `object_label = '__frame__'` and NULL bbox/score). `pub` so the event
/// producer (`crate::events_producer`) can read the just-written objects to emit `object_seen`.
pub struct ObjectWrite {
    pub object_label: String,
    pub bbox: Option<[f32; 4]>,
    pub det_score: Option<f32>,
    pub frame_offset_nanos: i64,
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
    pub embedding: Vec<f32>,
}

/// Process one VIDEO/MUXED segment through the face-identity pipeline. Returns the number of face
/// observations written. Idempotent (see `face_match::assign_faces`).
pub async fn process_vision_segment(
    pool: &PgPool,
    models: &VisionModels,
    cfg: &WorkerConfig,
    segment_id: Uuid,
) -> Result<usize> {
    // Whole-pipeline wall-clock for `hushai_worker_segment_seconds`; stage timers below attribute
    // per-camera cost (decode, detect, embed, write) for the capacity/load test.
    let started = std::time::Instant::now();
    let seg = media::load_segment(pool, segment_id).await?;
    let frames = {
        let _t = observe::StageTimer::start(STAGE, &[("lane", "vision"), ("stage", "sample_frames")]);
        frames::sample_frames(cfg, &seg, cfg.frames_per_segment).await?
    };
    if frames.is_empty() {
        return Ok(0); // no decodable video (e.g. audio-only blob) — clean no-op
    }

    let params = FaceEnhanceParams {
        gates: cfg.face_gates(),
        margin_frac: cfg.face_crop_margin_frac,
        restore_max_sharpness: cfg.face_restore_max_sharpness,
        restore_min_px: cfg.face_restore_min_px as f32,
        hard_min_px: cfg.face_hard_min_px as f32,
        hard_min_det_score: cfg.face_hard_min_det_score,
        upscale_min_px: cfg.face_upscale_min_px as f32,
        mint_max_yaw_deg: cfg.face_mint_max_yaw_deg,
        mint_max_pitch_deg: cfg.face_mint_max_pitch_deg,
    };
    let plate_params = PlateLaneParams {
        gates: cfg.plate_gates(),
        roi_margin: cfg.plate_vehicle_roi_margin,
        sr_min_side: cfg.plate_sr_min_side_px,
        detect_whole_frame: cfg.plate_detect_whole_frame,
    };
    let capture_start = seg.capture_start_unix_nanos;
    let objects_ran = models.object_detector.is_some() && models.clip.is_some();
    // The plate lane is active iff its models loaded (build_vision_models leaves them None when
    // PLATE_ENABLED=false or the models aren't provisioned); the per-frame closure checks them.
    // Each entry: the face's write row (crop_uri/is_best_shot filled in later) + its cleaned crop.
    let mut face_outcomes: Vec<FaceOutcome> = Vec::new();
    let mut object_writes: Vec<ObjectWrite> = Vec::new();
    let mut plate_cands: Vec<PlateCand> = Vec::new();

    for frame in frames {
        let offset = frame.offset_nanos;
        let abs = capture_start + offset;
        let img = frame.image;
        let models = models.clone();

        // Detection + cleanup + embedding are CPU-bound ONNX work — off the async runtime.
        let (faces_out, objs_out, plates_out) = tokio::task::spawn_blocking(
            move || -> Result<(Vec<FaceOutcome>, Vec<ObjectWrite>, Vec<PlateCand>)> {
                // ---- faces (required lane), with the cleanup cascade ----
                // Success-only timing for the fallible ONNX detect (a `?` failure shouldn't be
                // booked as a fast stage).
                let faces = {
                    let __t = std::time::Instant::now();
                    let r = models.detector.detect(&img).context("face detect")?;
                    observe::observe_duration(
                        STAGE,
                        &[("lane", "vision"), ("stage", "face_detect")],
                        __t.elapsed().as_secs_f64(),
                    );
                    r
                };
                let n_faces = faces.len();
                // Accumulate per-face enhance+embed time and record ONCE per frame, so a frame with
                // many faces doesn't take the registry lock once per face.
                let mut enhance_secs = 0.0f64;
                let mut fout = Vec::new();
                for face in faces {
                    let __t = std::time::Instant::now();
                    let res = enhance_and_embed(&img, &face, &models, &params);
                    enhance_secs += __t.elapsed().as_secs_f64();
                    match res {
                        Ok(Some(mut outcome)) => {
                            outcome.write.frame_offset_nanos = offset;
                            outcome.write.start_unix_nanos = abs;
                            outcome.write.end_unix_nanos = abs;
                            fout.push(outcome);
                        }
                        Ok(None) => {} // hard-rejected: no row, no mint
                        Err(e) => {
                            tracing::warn!(error = %e, "face cleanup/embed failed; skipping face")
                        }
                    }
                }
                if n_faces > 0 {
                    observe::observe_duration(
                        STAGE,
                        &[("lane", "vision"), ("stage", "face_enhance_embed")],
                        enhance_secs,
                    );
                }

                // ---- RF-DETR runs ONCE and fans out to the object + plate lanes ----
                let dets: Vec<DetectedObject> = match models.object_detector.as_ref() {
                    Some(od) => {
                        let __t = std::time::Instant::now();
                        let r = od.detect(&img).unwrap_or_else(|e| {
                            tracing::warn!(error = %e, "object detect failed; object/plate lanes skip this frame");
                            Vec::new()
                        });
                        observe::observe_duration(
                            STAGE,
                            &[("lane", "vision"), ("stage", "object_detect")],
                            __t.elapsed().as_secs_f64(),
                        );
                        r
                    }
                    None => Vec::new(),
                };

                // ---- objects (optional CLIP lane; NON-FATAL) ----
                let mut oout = Vec::new();
                if let (Some(_od), Some(cl)) =
                    (models.object_detector.as_ref(), models.clip.as_ref())
                {
                    // Accumulate all CLIP embeds (per-region + whole-frame) and record once per frame.
                    let mut clip_secs = 0.0f64;
                    for d in &dets {
                        let region = objects::crop_region(&img, &d.bbox);
                        let __t = std::time::Instant::now();
                        let res = cl.embed(&region);
                        clip_secs += __t.elapsed().as_secs_f64();
                        match res {
                            Ok(emb) => oout.push(ObjectWrite {
                                object_label: d.label.clone(),
                                bbox: Some(d.bbox),
                                det_score: Some(d.score),
                                frame_offset_nanos: offset,
                                start_unix_nanos: abs,
                                end_unix_nanos: abs,
                                embedding: emb,
                            }),
                            Err(e) => tracing::warn!(error = %e, "clip region embed failed; skipping object"),
                        }
                    }
                    let __t = std::time::Instant::now();
                    let frame_emb = cl.embed(&img);
                    clip_secs += __t.elapsed().as_secs_f64();
                    if let Ok(emb) = frame_emb {
                        oout.push(ObjectWrite {
                            object_label: "__frame__".to_string(),
                            bbox: None,
                            det_score: None,
                            frame_offset_nanos: offset,
                            start_unix_nanos: abs,
                            end_unix_nanos: abs,
                            embedding: emb,
                        });
                    }
                    observe::observe_duration(
                        STAGE,
                        &[("lane", "vision"), ("stage", "clip_embed")],
                        clip_secs,
                    );
                }

                // ---- plates (optional lane; NON-FATAL) — zoom into each vehicle, detect+read ----
                let mut pout = Vec::new();
                if let (Some(pd), Some(po)) =
                    (models.plate_detector.as_ref(), models.plate_ocr.as_ref())
                {
                    let __t = std::time::Instant::now();
                    pout = process_plate_lane(
                        &img, &dets, pd, po, models.upscaler.as_deref(), &plate_params, offset, abs,
                    );
                    observe::observe_duration(
                        STAGE,
                        &[("lane", "vision"), ("stage", "plate")],
                        __t.elapsed().as_secs_f64(),
                    );
                }
                Ok((fout, oout, pout))
            },
        )
        .await
        .context("vision blocking task panicked")??;
        face_outcomes.extend(faces_out);
        object_writes.extend(objs_out);
        plate_cands.extend(plates_out);
    }

    // Cluster plate reads across frames + vote into one confident read per plate.
    let mut plate_pairs = cluster_and_vote_plates(plate_cands, &plate_params.gates);
    if cfg.face_best_shot_enabled && !plate_pairs.is_empty() {
        if let Some(best_i) = plate_pairs
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                (a.0.mean_conf * a.0.det_score)
                    .partial_cmp(&(b.0.mean_conf * b.0.det_score))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
        {
            plate_pairs[best_i].0.is_best_shot = true;
        }
    }
    if cfg.face_persist_crop && !plate_pairs.is_empty() {
        persist_plate_crops(&cfg.blob_dir, segment_id, &mut plate_pairs).await;
    }
    let plate_writes: Vec<PlateWrite> = plate_pairs.into_iter().map(|(w, _)| w).collect();

    // Best-shot: tag the single highest-quality face of the segment (drives the sample-face
    // thumbnail). Then persist the cleaned crops to disk so the UI shows the RESTORED image.
    if cfg.face_best_shot_enabled {
        if let Some((best_i, _)) = face_outcomes
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                a.write
                    .quality_score
                    .partial_cmp(&b.write.quality_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        {
            face_outcomes[best_i].write.is_best_shot = true;
        }
    }
    if cfg.face_persist_crop && !face_outcomes.is_empty() {
        persist_face_crops(&cfg.blob_dir, segment_id, &mut face_outcomes).await;
    }

    let face_writes: Vec<FaceWrite> = face_outcomes.into_iter().map(|o| o.write).collect();

    // Nothing to write and no object reconciliation needed — the common always-on no-op.
    if face_writes.is_empty() && !objects_ran && plate_writes.is_empty() {
        record_vision_latency(started, seg.capture_start_unix_nanos);
        return Ok(0);
    }

    // One transaction: advisory-locked match-or-mint into persons/person_segments + the idempotent
    // scene_objects reconcile + plate match-or-mint, so a reprocess is a single atomic replacement.
    let (assigned, plates_assigned) = {
        let _t = observe::StageTimer::start(STAGE, &[("lane", "vision"), ("stage", "write_tx")]);
        let mut tx = pool.begin().await.context("begin vision write tx")?;
        let assigned = if face_writes.is_empty() {
            Vec::new()
        } else {
            face_match::assign_faces(
                &mut tx,
                segment_id,
                &seg.device_id,
                &face_writes,
                &cfg.face_match_cfg(),
            )
            .await?
        };
        if objects_ran {
            insert_scene_objects(&mut tx, segment_id, &seg.device_id, &object_writes).await?;
        }
        let plates_assigned = if plate_writes.is_empty() {
            Vec::new()
        } else {
            plate_match::assign_plates(
                &mut tx,
                segment_id,
                &seg.device_id,
                &plate_writes,
                &cfg.plate_match_cfg(),
            )
            .await?
        };
        tx.commit().await.context("commit vision write tx")?;
        (assigned, plates_assigned)
    };

    // Proactive layer (roadmap A3): materialize person/plate/object events + evaluate alert rules
    // from the just-committed detections. Guarded so an event/alert failure can never fail vision
    // processing (the segment still marks `done`). assigned/plates_assigned align to the *_writes.
    if cfg.events.enabled {
        let _t = observe::StageTimer::start(STAGE, &[("lane", "vision"), ("stage", "derive_events")]);
        if let Err(e) = crate::events_producer::derive_vision_events(
            pool,
            &seg,
            segment_id,
            &face_writes,
            &assigned,
            &object_writes,
            &plate_writes,
            &plates_assigned,
            &cfg.events,
        )
        .await
        {
            tracing::warn!(segment_id = %segment_id, error = %format!("{e:#}"), "vision event production failed; continuing");
        }
    }

    let attributed = assigned.iter().filter(|a| a.is_some()).count();
    let restored = face_writes.iter().filter(|f| f.restored).count();
    let plates_attributed = plates_assigned.iter().filter(|a| a.is_some()).count();
    tracing::info!(
        segment_id = %segment_id,
        faces = face_writes.len(),
        attributed,
        restored,
        objects = object_writes.len(),
        plates = plate_writes.len(),
        plates_attributed,
        "vision: wrote detections"
    );
    record_vision_latency(started, seg.capture_start_unix_nanos);
    Ok(face_writes.len())
}

/// Clean up + embed ONE detected face. Returns `None` only for a hard-reject (below the absolute
/// floors where even restoration can't help). Otherwise:
///   * already-clean (Mint on the raw crop): embed the raw aligned crop — NO restoration, so clean
///     faces stay in the exact legacy ArcFace space.
///   * recoverable: crop-with-margin → (super-resolve if tiny) → blind-face-restore → align on the
///     restored pixels → re-assess → embed. A restored face may match/attach; it mints only when it
///     was genuinely large + frontal.
///   * no restorer / restoration didn't lift above the gate: fall back to the raw crop if it at
///     least `AttachOnly`s, else drop.
fn enhance_and_embed(
    frame: &RgbImage,
    face: &Face,
    models: &VisionModels,
    p: &FaceEnhanceParams,
) -> Result<Option<FaceOutcome>> {
    // Absolute floor: too small or too low-confidence for any hope of recovery.
    if face.score < p.hard_min_det_score || face.min_side() < p.hard_min_px {
        return Ok(None);
    }

    let pose = face_embed::pose_from_landmarks(&face.landmarks);
    let raw_aligned = face_embed::align_crop(frame, &face.landmarks);
    let raw_sharp = face_embed::sharpness(&raw_aligned);
    let raw_quality = face_embed::assess_quality(face, raw_sharp, &p.gates);

    // Should we even attempt restoration? Only for not-already-clean crops (clean ones must keep
    // their legacy embedding), when a small/blurry crop could plausibly be recovered.
    let wants_restore = raw_quality != FaceQuality::Mint
        && (raw_sharp < p.restore_max_sharpness || face.min_side() < p.restore_min_px);

    if let (true, Some(restorer)) = (wants_restore, models.restorer.as_ref()) {
        if let Some(out) = try_restore(
            frame,
            face,
            &pose,
            restorer,
            models.upscaler.as_deref(),
            &models.embedder,
            p,
        )? {
            return Ok(Some(out));
        }
        // else fall through to the raw path
    }

    // Raw path: embed the raw aligned crop if it at least clears the reject gate.
    if raw_quality == FaceQuality::Reject {
        return Ok(None);
    }
    let quality = downgrade_for_pose(raw_quality, &pose, p);
    let embedding = models.embedder.embed_aligned(&raw_aligned)?;
    let crop = enhance::crop_with_margin(frame, &face.bbox, p.margin_frac).0;
    Ok(Some(FaceOutcome {
        write: build_write(face, &pose, raw_sharp, quality, false, embedding),
        cleaned_crop: crop,
    }))
}

/// The restoration branch: margin-crop → optional super-res → restore → align on restored pixels →
/// re-assess. Returns `None` if the restored crop still doesn't clear the (size-relaxed) gate.
fn try_restore(
    frame: &RgbImage,
    face: &Face,
    pose: &face_embed::Pose,
    restorer: &FaceRestorer,
    upscaler: Option<&Upscaler>,
    embedder: &FaceEmbedder,
    p: &FaceEnhanceParams,
) -> Result<Option<FaceOutcome>> {
    let (mut cur, off) = enhance::crop_with_margin(frame, &face.bbox, p.margin_frac);
    let mut lmk = face.landmarks;
    for l in lmk.iter_mut() {
        l[0] -= off[0];
        l[1] -= off[1];
    }
    // Zoom into a tiny crop first so the restorer gets more to work with.
    if let Some(up) = upscaler {
        if face.min_side() < p.upscale_min_px {
            if let Ok(big) = up.upscale(&cur) {
                let sx = big.width() as f32 / cur.width().max(1) as f32;
                let sy = big.height() as f32 / cur.height().max(1) as f32;
                for l in lmk.iter_mut() {
                    l[0] *= sx;
                    l[1] *= sy;
                }
                cur = big;
            }
        }
    }
    let restored = restorer.restore(&cur)?; // 512×512 RGB
    let sx = restored.width() as f32 / cur.width().max(1) as f32;
    let sy = restored.height() as f32 / cur.height().max(1) as f32;
    let mut lmk_r = lmk;
    for l in lmk_r.iter_mut() {
        l[0] *= sx;
        l[1] *= sy;
    }
    let aligned = face_embed::align_crop(&restored, &lmk_r);
    let restored_sharp = face_embed::sharpness(&aligned);

    // Usable if the restored crop is sharp + confident and the ORIGINAL face was at least the hard
    // floor (restoration can't invent true resolution, so size is judged on the original capture).
    let usable = face.score >= p.gates.min_det_score
        && face.min_side() >= p.hard_min_px
        && restored_sharp >= p.gates.min_sharpness;
    if !usable {
        return Ok(None);
    }
    // Mint only when the original face was genuinely large + confident + sharp-after-restore.
    let big = face.min_side() >= p.gates.min_px * 1.5;
    let confident = face.score >= (p.gates.min_det_score + 0.2).min(0.95);
    let sharp = restored_sharp >= p.gates.min_sharpness * 1.5;
    let base = if big && confident && sharp {
        FaceQuality::Mint
    } else {
        FaceQuality::AttachOnly
    };
    let quality = downgrade_for_pose(base, pose, p);
    let embedding = embedder.embed_aligned(&aligned)?;
    Ok(Some(FaceOutcome {
        write: build_write(face, pose, restored_sharp, quality, true, embedding),
        cleaned_crop: restored,
    }))
}

/// Downgrade a `Mint` to `AttachOnly` when the face isn't frontal enough to safely create a new
/// identity (a profile face minting a "new person" is a classic over-split bug).
fn downgrade_for_pose(q: FaceQuality, pose: &face_embed::Pose, p: &FaceEnhanceParams) -> FaceQuality {
    if q == FaceQuality::Mint
        && !face_embed::is_frontal(pose, p.mint_max_yaw_deg, p.mint_max_pitch_deg)
    {
        FaceQuality::AttachOnly
    } else {
        q
    }
}

/// Assemble a `FaceWrite` (crop_uri/is_best_shot/timestamps filled in by the caller).
fn build_write(
    face: &Face,
    pose: &face_embed::Pose,
    sharp: f32,
    quality: FaceQuality,
    restored: bool,
    embedding: Vec<f32>,
) -> FaceWrite {
    let min_side = face.min_side();
    let frontality = (1.0 - (pose.yaw.abs() / 90.0).min(1.0)) * (1.0 - (pose.pitch.abs() / 90.0).min(1.0));
    let size_term = (min_side / (min_side + 40.0)).clamp(0.0, 1.0);
    let sharp_term = (sharp / (sharp + 30.0)).clamp(0.0, 1.0);
    let quality_score = face.score * frontality * size_term * sharp_term;
    FaceWrite {
        embedding,
        bbox: face.bbox,
        det_score: face.score,
        frame_offset_nanos: 0,
        start_unix_nanos: 0,
        end_unix_nanos: 0,
        quality,
        restored,
        yaw: pose.yaw,
        pitch: pose.pitch,
        quality_score,
        is_best_shot: false,
        crop_uri: None,
    }
}

/// Encode + write the cleaned crops to `<blob_dir>/face_crops/<segment>_<i>.jpg` and set each row's
/// `crop_uri`. Best-effort: a write failure just leaves `crop_uri` NULL (sample-face falls back to
/// re-cropping the raw frame). Deterministic filenames keep reprocess idempotent.
async fn persist_face_crops(blob_dir: &str, segment_id: Uuid, outcomes: &mut [FaceOutcome]) {
    let dir = std::path::Path::new(blob_dir).join("face_crops");
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!(error = %e, "could not create face_crops dir; skipping crop persistence");
        return;
    }
    // Encode (CPU-bound) off the async runtime; collect (index, path) for the ones that succeed.
    let jobs: Vec<(usize, std::path::PathBuf, RgbImage)> = outcomes
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let path = dir.join(format!("{segment_id}_{i}.jpg"));
            (i, path, thumbnail(&o.cleaned_crop, 256))
        })
        .collect();
    let written = tokio::task::spawn_blocking(move || {
        let mut ok: Vec<(usize, String)> = Vec::new();
        for (i, path, img) in jobs {
            match img.save(&path) {
                Ok(()) => ok.push((i, path.to_string_lossy().into_owned())),
                Err(e) => tracing::warn!(error = %e, ?path, "writing face crop failed"),
            }
        }
        ok
    })
    .await
    .unwrap_or_default();
    for (i, uri) in written {
        outcomes[i].write.crop_uri = Some(uri);
    }
}

/// Downscale to at most `max` px on the long side (keep aspect); never upscale.
fn thumbnail(img: &RgbImage, max: u32) -> RgbImage {
    let (w, h) = (img.width(), img.height());
    let long = w.max(h);
    if long <= max || long == 0 {
        return img.clone();
    }
    let scale = max as f32 / long as f32;
    enhance::resize_rgb(img, (w as f32 * scale) as u32, (h as f32 * scale) as u32)
}

/// Run the plate lane for ONE frame: for each vehicle ROI (or the whole frame when configured and no
/// vehicle was found), zoom into the ROI, detect plates, rectify + enhance, OCR. Returns per-plate
/// candidates (cross-frame clustering + voting happens later). Non-fatal: per-ROI/per-plate failures
/// are logged and skipped.
#[allow(clippy::too_many_arguments)]
fn process_plate_lane(
    img: &RgbImage,
    dets: &[DetectedObject],
    pd: &PlateDetector,
    po: &PlateOcr,
    upscaler: Option<&Upscaler>,
    p: &PlateLaneParams,
    offset: i64,
    abs: i64,
) -> Vec<PlateCand> {
    let mut out = Vec::new();
    // ROIs: each vehicle's bbox; or the whole frame if configured and no vehicle is present.
    let mut rois: Vec<([f32; 4], Option<String>)> = dets
        .iter()
        .filter(|d| is_vehicle(&d.label))
        .map(|d| (d.bbox, Some(d.label.clone())))
        .collect();
    if rois.is_empty() && p.detect_whole_frame {
        rois.push(([0.0, 0.0, img.width() as f32, img.height() as f32], None));
    }

    for (roi, vlabel) in rois {
        let (vcrop, voff) = enhance::crop_with_margin(img, &roi, p.roi_margin);
        let boxes = match pd.detect(&vcrop) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "plate detect failed for a vehicle ROI; skipping");
                continue;
            }
        };
        for pb in boxes {
            // Map ROI-local coords back to original-frame pixels.
            let fb = [pb.bbox[0] + voff[0], pb.bbox[1] + voff[1], pb.bbox[2], pb.bbox[3]];
            let min_side = pb.bbox[2].min(pb.bbox[3]);
            if min_side < p.gates.min_px {
                continue;
            }
            let fcorners = pb.corners.map(|c| {
                let mut o = c;
                for q in o.iter_mut() {
                    q[0] += voff[0];
                    q[1] += voff[1];
                }
                o
            });
            let plate_frame = PlateBox {
                bbox: fb,
                score: pb.score,
                corners: fcorners,
            };
            let rect = rectify::rectify(img, &plate_frame, rectify::PLATE_W, rectify::PLATE_H);
            let enhanced = rectify::enhance_plate(&rect, upscaler, p.sr_min_side);
            let read = match po.read(&enhanced) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "plate OCR failed; skipping plate");
                    continue;
                }
            };
            if read.text.is_empty() {
                continue;
            }
            out.push(PlateCand {
                read,
                det_score: pb.score,
                vehicle_bbox: vlabel.as_ref().map(|_| roi),
                vehicle_label: vlabel.clone(),
                plate_bbox: fb,
                plate_corners: fcorners,
                frame_offset_nanos: offset,
                start_unix_nanos: abs,
                end_unix_nanos: abs,
                min_side,
                crop: enhanced,
            });
        }
    }
    out
}

/// Cluster per-frame plate candidates by plate-bbox IoU and vote each cluster into one confident
/// read (temporal aggregation across the segment's frames). Returns each plate's `PlateWrite` paired
/// with its best thumbnail crop. Rejected (low-quality) clusters are dropped.
fn cluster_and_vote_plates(
    cands: Vec<PlateCand>,
    gates: &PlateGates,
) -> Vec<(PlateWrite, RgbImage)> {
    let mut clusters: Vec<Vec<PlateCand>> = Vec::new();
    for c in cands {
        match clusters
            .iter_mut()
            .find(|cl| geom::iou(&cl[0].plate_bbox, &c.plate_bbox) > 0.3)
        {
            Some(cl) => cl.push(c),
            None => clusters.push(vec![c]),
        }
    }

    let mut writes = Vec::new();
    for cl in clusters {
        let reads: Vec<PlateRead> = cl.iter().map(|c| c.read.clone()).collect();
        let Some(voted) = plate_norm::vote(&reads) else {
            continue;
        };
        // Representative member (best per-frame read) supplies bboxes/corners/crop/timestamps.
        let rep = cl
            .iter()
            .max_by(|a, b| {
                (a.det_score * a.read.mean_conf)
                    .partial_cmp(&(b.det_score * b.read.mean_conf))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap();
        let det_score = cl.iter().map(|c| c.det_score).fold(0.0f32, f32::max);
        let min_side = cl.iter().map(|c| c.min_side).fold(0.0f32, f32::max);
        let len = voted.text.chars().count();
        let quality = plate_norm::assess_quality(det_score, voted.mean_conf, min_side, len, gates);
        if quality == PlateQuality::Reject {
            continue;
        }
        let norm = plate_norm::fold_confusables(&plate_norm::normalize(&voted.text));
        let pw = PlateWrite {
            ocr_text: voted.text.clone(),
            ocr_text_norm: norm,
            char_confidences: voted.char_confidences.clone(),
            mean_conf: voted.mean_conf,
            det_score,
            vehicle_bbox: rep.vehicle_bbox,
            vehicle_label: rep.vehicle_label.clone(),
            plate_bbox: rep.plate_bbox,
            plate_corners: rep.plate_corners,
            frame_offset_nanos: rep.frame_offset_nanos,
            start_unix_nanos: rep.start_unix_nanos,
            end_unix_nanos: rep.end_unix_nanos,
            quality,
            crop_uri: None,
            is_best_shot: false,
            embedding: None,
        };
        writes.push((pw, rep.crop.clone()));
    }
    writes
}

/// Persist the rectified plate crops to `<blob_dir>/plate_crops/<segment>_<i>.jpg` and set each
/// row's `crop_uri`. Best-effort, deterministic filenames (same contract as `persist_face_crops`).
async fn persist_plate_crops(
    blob_dir: &str,
    segment_id: Uuid,
    pairs: &mut [(PlateWrite, RgbImage)],
) {
    let dir = std::path::Path::new(blob_dir).join("plate_crops");
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!(error = %e, "could not create plate_crops dir; skipping crop persistence");
        return;
    }
    let jobs: Vec<(usize, std::path::PathBuf, RgbImage)> = pairs
        .iter()
        .enumerate()
        .map(|(i, (_, crop))| (i, dir.join(format!("{segment_id}_{i}.jpg")), crop.clone()))
        .collect();
    let written = tokio::task::spawn_blocking(move || {
        let mut ok: Vec<(usize, String)> = Vec::new();
        for (i, path, img) in jobs {
            match img.save(&path) {
                Ok(()) => ok.push((i, path.to_string_lossy().into_owned())),
                Err(e) => tracing::warn!(error = %e, ?path, "writing plate crop failed"),
            }
        }
        ok
    })
    .await
    .unwrap_or_default();
    for (i, uri) in written {
        pairs[i].0.crop_uri = Some(uri);
    }
}

/// Idempotent `scene_objects` reconcile for one segment: delete-by-segment then insert every row
/// (region rows carry a bbox + det_score; the whole-frame open-vocab row has NULL bbox/score and
/// `object_label = '__frame__'`). Runs inside the vision write tx. bbox stored as JSONB `[x,y,w,h]`
/// in original-frame pixels (the viewer overlay scales it against the intrinsic video resolution).
async fn insert_scene_objects(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    segment_id: Uuid,
    device_id: &str,
    objs: &[ObjectWrite],
) -> Result<()> {
    sqlx::query("DELETE FROM scene_objects WHERE segment_id = $1")
        .bind(segment_id)
        .execute(&mut **tx)
        .await
        .context("clearing prior scene_objects")?;
    for o in objs {
        let dim = o.embedding.len() as i32;
        let bbox_json: Option<String> = o
            .bbox
            .map(|b| format!("[{},{},{},{}]", b[0], b[1], b[2], b[3]));
        sqlx::query(
            "INSERT INTO scene_objects \
             (segment_id, device_id, object_label, bbox, det_score, frame_offset_nanos, \
              start_unix_nanos, end_unix_nanos, embedding, embedding_model, embedding_dim) \
             VALUES ($1, $2, $3, $4::jsonb, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(segment_id)
        .bind(device_id)
        .bind(&o.object_label)
        .bind(bbox_json)
        .bind(o.det_score)
        .bind(o.frame_offset_nanos)
        .bind(o.start_unix_nanos)
        .bind(o.end_unix_nanos)
        .bind(pgvector::Vector::from(o.embedding.clone()))
        .bind(CLIP_MODEL_TAG)
        .bind(dim)
        .execute(&mut **tx)
        .await
        .context("inserting scene_object")?;
    }
    Ok(())
}
