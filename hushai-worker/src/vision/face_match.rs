//! Online, global, serialized, idempotent FACE match-or-mint into `persons`/`person_segments`.
//! The visual analogue of `speaker_match::assign_speaker`, with the same hard-won machinery:
//!   1. MULTI-VECTOR k-NN vote over `person_segments` raw face templates (centroid = cold-start
//!      fallback), reusing the pure `speaker_match::vote`.
//!   2. MINT-GUARD HYSTERESIS (match_threshold < gray-zone < mint_distance_floor); only a clean
//!      (`FaceQuality::Mint`) face may create a NEW identity — marginal faces far from everyone are
//!      left NULL, never a duplicate.
//!   3. SELF-HEALING CENTROID recomputed from recent `quality='clean'` rows.
//!
//! Differences from the speaker matcher (one voice per segment): a video segment yields MANY faces,
//! so this is delete-by-segment-then-insert (the transcript_sentences idiom). Idempotency: if any
//! `person_segments` row already exists for the segment, the prior assignments are returned verbatim
//! and nothing is touched (byte-identical reprocess). Faces are processed in order and each row is
//! inserted before the next face's k-NN, so the same person appearing in two sampled frames of one
//! segment matches a person minted earlier in the same segment.
//!
//! Uses a DIFFERENT advisory-lock key from the speaker space (cross-device identity → one global
//! person lock). `Reject`-quality faces never reach here (the caller drops them: no row, no mint).

use anyhow::{Context, Result};
use sqlx::{AssertSqlSafe, Postgres, Row, Transaction};
use uuid::Uuid;

use super::face_embed::FaceQuality;
use crate::speaker_match::vote; // reuse the proven pure k-NN vote

/// Global person-space advisory lock key — distinct from SPEAKER_LOCK_KEY ("hsspkr").
const VISION_PERSON_LOCK_KEY: i64 = 0x6873_7670_736e; // "hsvpsn"

#[derive(Debug, Clone, Copy)]
pub struct FaceMatchConfig {
    pub match_threshold: f32,
    pub mint_distance_floor: f32,
    pub knn_k: i64,
    pub knn_neighbor_ceiling: f32,
    pub knn_min_neighbors: i64,
    pub knn_ef_search: i64,
    pub knn_statement_timeout_ms: i64,
    pub centroid_window: i64,
    /// When false, a generatively-RESTORED face may match/attach but may NOT mint a new identity or
    /// fold into a centroid (protects existing centroids from restoration drift during a transition).
    pub restored_may_mint: bool,
}

/// One detected face, ready to assign + persist. `quality` is `Mint` or `AttachOnly` only
/// (`Reject` faces are dropped by the caller and never reach the matcher).
#[derive(Debug, Clone)]
pub struct FaceWrite {
    pub embedding: Vec<f32>, // 512-d L2-normalized ArcFace template
    pub bbox: [f32; 4],
    pub det_score: f32,
    pub frame_offset_nanos: i64,
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
    pub quality: FaceQuality,
    /// True if this embedding came from a generatively-restored crop (recover-then-embed path).
    pub restored: bool,
    /// Approximate head pose (degrees) from the landmark proxy; stored for calibration/auditing.
    pub yaw: f32,
    pub pitch: f32,
    /// Composite crop quality (sharpness×frontality×det_score×size); drives best-shot selection.
    pub quality_score: f32,
    /// Tagged best-shot for the segment (the thumbnail the UI prefers).
    pub is_best_shot: bool,
    /// Filesystem path of the persisted cleaned crop, if `face_persist_crop` wrote one.
    pub crop_uri: Option<String>,
}

enum Action {
    Match(Uuid),
    Attach(Uuid),
    Mint,
    Null,
}

/// Assign (and persist) every detected face for one segment inside an open transaction. Returns
/// the resolved `person_id` per input face (in order; `None` = left unattributed).
pub async fn assign_faces(
    tx: &mut Transaction<'_, Postgres>,
    segment_id: Uuid,
    device_id: &str,
    faces: &[FaceWrite],
    cfg: &FaceMatchConfig,
) -> Result<Vec<Option<Uuid>>> {
    // 1. Global serialization for the whole match/mint step (cross-device → one person space).
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(VISION_PERSON_LOCK_KEY)
        .execute(&mut **tx)
        .await
        .context("taking vision person advisory lock")?;

    // 2. Idempotency: if this segment was already processed, return its exact outcomes and touch
    //    nothing (byte-identical reprocess). person_segments rows are ordered by id = insert order.
    let prior =
        sqlx::query("SELECT person_id FROM person_segments WHERE segment_id = $1 ORDER BY id")
            .bind(segment_id)
            .fetch_all(&mut **tx)
            .await
            .context("reading prior face assignments")?;
    if !prior.is_empty() {
        return prior
            .iter()
            .map(|r| {
                r.try_get::<Option<Uuid>, _>("person_id")
                    .map_err(Into::into)
            })
            .collect();
    }

    // 3. Per-tx HNSW/timeout GUCs (after the early-return so reprocess stays untouched).
    sqlx::query("SET LOCAL hnsw.iterative_scan = 'strict_order'")
        .execute(&mut **tx)
        .await
        .context("set iterative_scan")?;
    let ef = cfg.knn_ef_search.max(cfg.knn_k).max(1);
    sqlx::query(AssertSqlSafe(format!("SET LOCAL hnsw.ef_search = {ef}")))
        .execute(&mut **tx)
        .await
        .context("set ef_search")?;
    let timeout = cfg.knn_statement_timeout_ms.max(0);
    sqlx::query(AssertSqlSafe(format!(
        "SET LOCAL statement_timeout = {timeout}"
    )))
    .execute(&mut **tx)
    .await
    .context("set statement_timeout")?;

    // 4. Clear any partial rows from a failed prior attempt, then match-or-mint each face.
    sqlx::query("DELETE FROM person_segments WHERE segment_id = $1")
        .bind(segment_id)
        .execute(&mut **tx)
        .await
        .context("clearing prior person_segments")?;

    let mut assigned: Vec<Option<Uuid>> = Vec::with_capacity(faces.len());
    let mut to_recompute: Vec<Uuid> = Vec::new();

    for f in faces {
        let qvec = pgvector::Vector::from(f.embedding.clone());

        // 4a. k-NN over raw per-face templates. $1 referenced thrice (one bind).
        let rows = sqlx::query(
            "SELECT person_id, (embedding <=> $1) AS dist \
             FROM person_segments \
             WHERE person_id IS NOT NULL AND (embedding <=> $1) <= $2 \
             ORDER BY embedding <=> $1 LIMIT $3",
        )
        .bind(qvec.clone())
        .bind(cfg.knn_neighbor_ceiling as f64)
        .bind(cfg.knn_k)
        .fetch_all(&mut **tx)
        .await
        .context("k-NN over person_segments")?;

        let mut neighbors: Vec<(Uuid, f32)> = Vec::with_capacity(rows.len());
        for r in &rows {
            let id: Uuid = r.get("person_id");
            let d: f64 = r.try_get("dist")?;
            neighbors.push((id, d as f32));
        }

        // 4b. Vote (enough neighbors) else centroid catalog fallback.
        let decided: Option<(Uuid, f32)> = match vote(&neighbors, cfg.knn_k, cfg.match_threshold) {
            Some(w) if neighbors.len() as i64 >= cfg.knn_min_neighbors => {
                Some((w.speaker_id, w.min_dist))
            }
            _ => nearest_centroid(tx, &f.embedding).await?,
        };

        // 4c. Mint-guard hysteresis (only clean faces may mint; gray zone always attaches). A
        //     restored face may match/attach but may not mint or fold into a centroid unless the
        //     operator has cleared `restored_may_mint` (protects centroids from generative drift).
        let may_fold = !f.restored || cfg.restored_may_mint;
        let action = match decided {
            Some((id, d)) if d <= cfg.match_threshold => {
                if f.quality == FaceQuality::Mint && may_fold {
                    Action::Match(id)
                } else {
                    Action::Attach(id)
                }
            }
            Some((id, d)) if d <= cfg.mint_distance_floor => Action::Attach(id),
            _ => {
                if f.quality == FaceQuality::Mint && may_fold {
                    Action::Mint
                } else {
                    Action::Null
                }
            }
        };

        let (person_id, quality_tag): (Option<Uuid>, &str) = match action {
            Action::Match(id) => {
                if !to_recompute.contains(&id) {
                    to_recompute.push(id);
                }
                (Some(id), "clean")
            }
            Action::Attach(id) => (Some(id), "marginal"),
            Action::Mint => {
                let id = Uuid::now_v7();
                sqlx::query(
                    "INSERT INTO persons (person_id, centroid, n_samples, first_seen_device_id) \
                     VALUES ($1, $2, 1, $3)",
                )
                .bind(id)
                .bind(qvec.clone())
                .bind(device_id)
                .execute(&mut **tx)
                .await
                .context("minting person")?;
                tracing::info!(person_id = %id, "minted new person (clean face, far from all known faces)");
                (Some(id), "clean")
            }
            Action::Null => (None, "marginal"),
        };

        // Insert the raw per-face template (visible to subsequent faces' k-NN within this tx).
        let bbox_json = format!("[{},{},{},{}]", f.bbox[0], f.bbox[1], f.bbox[2], f.bbox[3]);
        sqlx::query(
            "INSERT INTO person_segments \
             (segment_id, device_id, person_id, start_unix_nanos, end_unix_nanos, \
              frame_offset_nanos, embedding, bbox, det_score, quality, \
              crop_uri, is_best_shot, restored, yaw, pitch, quality_score) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb, $9, $10, $11, $12, $13, $14, $15, $16)",
        )
        .bind(segment_id)
        .bind(device_id)
        .bind(person_id)
        .bind(f.start_unix_nanos)
        .bind(f.end_unix_nanos)
        .bind(f.frame_offset_nanos)
        .bind(qvec)
        .bind(bbox_json)
        .bind(f.det_score)
        .bind(quality_tag)
        .bind(f.crop_uri.as_deref())
        .bind(f.is_best_shot)
        .bind(f.restored)
        .bind(f.yaw)
        .bind(f.pitch)
        .bind(f.quality_score)
        .execute(&mut **tx)
        .await
        .context("inserting person_segment")?;

        assigned.push(person_id);
    }

    // 5. Self-healing centroid recompute for each matched person (now incl. the just-written rows).
    for id in to_recompute {
        recompute_centroid(tx, id, cfg.centroid_window).await?;
    }

    Ok(assigned)
}

/// Nearest person centroid by cosine distance over the (small) catalog — the cold-start fallback.
async fn nearest_centroid(
    tx: &mut Transaction<'_, Postgres>,
    embedding: &[f32],
) -> Result<Option<(Uuid, f32)>> {
    let rows = sqlx::query("SELECT person_id, centroid FROM persons")
        .fetch_all(&mut **tx)
        .await
        .context("loading person catalog")?;
    let mut best: Option<(Uuid, f32)> = None;
    for r in &rows {
        let Ok(Some(centroid)) = r.try_get::<Option<pgvector::Vector>, _>("centroid") else {
            continue;
        };
        let d = crate::vad::cosine_distance(embedding, centroid.as_slice());
        if best.as_ref().is_none_or(|(_, bd)| d < *bd) {
            let id: Uuid = r.get("person_id");
            best = Some((id, d));
        }
    }
    Ok(best)
}

/// Recompute a person's centroid as the L2-normalized mean of its most recent `window` CLEAN
/// templates; set n_samples to its total clean-row count. Self-healing (a bad fold ages out).
async fn recompute_centroid(
    tx: &mut Transaction<'_, Postgres>,
    person_id: Uuid,
    window: i64,
) -> Result<()> {
    let mean_row = sqlx::query(
        "SELECT avg(embedding) AS mean FROM ( \
            SELECT embedding FROM person_segments \
            WHERE person_id = $1 AND quality = 'clean' \
            ORDER BY created_at DESC LIMIT $2 \
         ) r",
    )
    .bind(person_id)
    .bind(window.max(1))
    .fetch_one(&mut **tx)
    .await
    .context("recomputing person centroid mean")?;

    let Ok(Some(mean)) = mean_row.try_get::<Option<pgvector::Vector>, _>("mean") else {
        return Ok(());
    };
    let mut centroid = mean.to_vec();
    crate::vad::l2_normalize(&mut centroid);

    let count: i64 = sqlx::query(
        "SELECT count(*) AS n FROM person_segments WHERE person_id = $1 AND quality = 'clean'",
    )
    .bind(person_id)
    .fetch_one(&mut **tx)
    .await
    .context("counting clean person templates")?
    .try_get("n")?;

    sqlx::query(
        "UPDATE persons SET centroid = $1, n_samples = $2, updated_at = now() WHERE person_id = $3",
    )
    .bind(pgvector::Vector::from(centroid))
    .bind(count)
    .bind(person_id)
    .execute(&mut **tx)
    .await
    .context("updating person centroid")?;
    Ok(())
}
