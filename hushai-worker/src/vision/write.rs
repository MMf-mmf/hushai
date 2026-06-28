//! The per-segment vision pipeline + its single idempotent write transaction. The visual analogue
//! of `process::process_segment` + `write_transcript`:
//!   load segment -> sample frames -> (per frame, on the blocking pool) detect faces -> quality-gate
//!   -> align + embed -> collect -> in ONE tx, advisory-locked match-or-mint into persons/person_segments.
//!
//! Reject-quality faces are dropped here (no row, no mint). A segment with no decodable video or no
//! usable face writes nothing and is not an error — it just marks vision status `done`.
//! (Phase B will add open-vocab object detection + CLIP rows into `scene_objects` in the same tx.)

use std::sync::Arc;

use anyhow::{Context, Result};

use super::detect::FaceDetector;
use super::face_embed::{self, FaceEmbedder, FaceQuality};
use super::face_match::{self, FaceWrite};
use super::frames;
use super::objects::{self, ClipEmbedder, ObjectDetector};
use crate::config::WorkerConfig;
use crate::media;
use sqlx::PgPool;
use uuid::Uuid;

/// The vision models, loaded once at startup and cloned (cheap `Arc`) into each worker task.
/// Faces are required; the object lane (RF-DETR + CLIP) is OPTIONAL — when its models aren't
/// provisioned it stays `None` and only faces run (the audio/face paths never depend on it).
#[derive(Clone)]
pub struct VisionModels {
    pub detector: Arc<FaceDetector>,
    pub embedder: Arc<FaceEmbedder>,
    pub object_detector: Option<Arc<ObjectDetector>>,
    pub clip: Option<Arc<ClipEmbedder>>,
}

/// CLIP image-embedding model tag stored on every `scene_objects` row (the open-vocab space).
const CLIP_MODEL_TAG: &str = "openclip-vit-b32";

/// One detected object ready to persist into `scene_objects` (region row, or the whole-frame
/// open-vocab row with `object_label = '__frame__'` and NULL bbox/score).
struct ObjectWrite {
    object_label: String,
    bbox: Option<[f32; 4]>,
    det_score: Option<f32>,
    frame_offset_nanos: i64,
    start_unix_nanos: i64,
    end_unix_nanos: i64,
    embedding: Vec<f32>,
}

/// Process one VIDEO/MUXED segment through the face-identity pipeline. Returns the number of face
/// observations written. Idempotent (see `face_match::assign_faces`).
pub async fn process_vision_segment(
    pool: &PgPool,
    models: &VisionModels,
    cfg: &WorkerConfig,
    segment_id: Uuid,
) -> Result<usize> {
    let seg = media::load_segment(pool, segment_id).await?;
    let frames = frames::sample_frames(cfg, &seg, cfg.frames_per_segment).await?;
    if frames.is_empty() {
        return Ok(0); // no decodable video (e.g. audio-only blob) — clean no-op
    }

    let gates = cfg.face_gates();
    let capture_start = seg.capture_start_unix_nanos;
    // The object lane (RF-DETR + CLIP) runs only when both its models are provisioned; otherwise
    // we never touch scene_objects (faces still run). When it DOES run, scene_objects is reconciled
    // by delete-by-segment-then-insert for idempotency, even if no object was found this pass.
    let objects_ran = models.object_detector.is_some() && models.clip.is_some();
    let mut face_writes: Vec<FaceWrite> = Vec::new();
    let mut object_writes: Vec<ObjectWrite> = Vec::new();

    for frame in frames {
        let offset = frame.offset_nanos;
        let abs = capture_start + offset;
        let img = frame.image;
        let detector = models.detector.clone();
        let embedder = models.embedder.clone();
        let obj_det = models.object_detector.clone();
        let clip = models.clip.clone();

        // Detection + alignment + embedding are CPU-bound ONNX work — off the async runtime.
        let (faces_out, objs_out) = tokio::task::spawn_blocking(
            move || -> Result<(Vec<FaceWrite>, Vec<ObjectWrite>)> {
                // ---- faces (required lane) ----
                let faces = detector.detect(&img).context("face detect")?;
                let mut fout = Vec::new();
                for face in faces {
                    let crop = face_embed::align_crop(&img, &face.landmarks);
                    let sharp = face_embed::sharpness(&crop);
                    let quality = face_embed::assess_quality(&face, sharp, &gates);
                    if quality == FaceQuality::Reject {
                        continue; // bad crop: no row, no centroid fold
                    }
                    let embedding = embedder.embed(&img, &face).context("face embed")?;
                    fout.push(FaceWrite {
                        embedding,
                        bbox: face.bbox,
                        det_score: face.score,
                        frame_offset_nanos: offset,
                        start_unix_nanos: abs,
                        end_unix_nanos: abs,
                        quality,
                    });
                }

                // ---- objects (optional lane; failures are NON-FATAL so faces always succeed) ----
                let mut oout = Vec::new();
                if let (Some(od), Some(cl)) = (obj_det.as_ref(), clip.as_ref()) {
                    match od.detect(&img) {
                        Ok(dets) => {
                            for d in dets {
                                let region = objects::crop_region(&img, &d.bbox);
                                match cl.embed(&region) {
                                    Ok(emb) => oout.push(ObjectWrite {
                                        object_label: d.label,
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
                            // Whole-frame open-vocab row so arbitrary (non-COCO) queries still match.
                            if let Ok(emb) = cl.embed(&img) {
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
                        }
                        Err(e) => tracing::warn!(error = %e, "object detect failed; object lane skips this frame"),
                    }
                }
                Ok((fout, oout))
            },
        )
        .await
        .context("vision blocking task panicked")??;
        face_writes.extend(faces_out);
        object_writes.extend(objs_out);
    }

    // Nothing to write and no object reconciliation needed — the common always-on no-op.
    if face_writes.is_empty() && !objects_ran {
        return Ok(0);
    }

    // One transaction: advisory-locked match-or-mint into persons/person_segments + the
    // idempotent scene_objects reconcile, so a reprocess is a single atomic replacement.
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
    tx.commit().await.context("commit vision write tx")?;

    let attributed = assigned.iter().filter(|a| a.is_some()).count();
    tracing::info!(
        segment_id = %segment_id,
        faces = face_writes.len(),
        attributed,
        objects = object_writes.len(),
        "vision: wrote detections"
    );
    Ok(face_writes.len())
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
