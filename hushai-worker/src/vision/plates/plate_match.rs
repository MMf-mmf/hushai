//! Online, global, serialized, idempotent PLATE match-or-mint into `license_plates` /
//! `plate_detections`. The structural twin of `face_match::assign_faces` (advisory lock, idempotent
//! delete-by-segment, intra-tx visibility), but a plate's identity is its TEXT, so matching is by
//! the confusable-folded normalized string — exact first, then trigram-candidate + edit-distance
//! fuzzy for OCR noise — NOT a k-NN over an embedding. Only a `Mint`-quality read may create a new
//! catalog plate; everything else matches/attaches or is left NULL. The canonical `plate_text`
//! self-heals to the best clean read (the string analogue of the self-healing centroid).

use anyhow::{Context, Result};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use super::normalize::{edit_distance, PlateQuality};

/// Plate-space advisory lock key — distinct from the person ("hsvpsn") + speaker ("hsspkr") keys.
const VISION_PLATE_LOCK_KEY: i64 = 0x6873_706c_6174; // "hsplat"

#[derive(Debug, Clone, Copy)]
pub struct PlateMatchConfig {
    pub max_edit_distance: usize,
    pub fuzzy_min_similarity: f32,
    pub min_len: usize,
}

/// One plate read for a segment, ready to assign + persist.
#[derive(Debug, Clone)]
pub struct PlateWrite {
    pub ocr_text: String,
    pub ocr_text_norm: String,
    pub char_confidences: Vec<f32>,
    pub mean_conf: f32,
    pub det_score: f32,
    pub vehicle_bbox: Option<[f32; 4]>,
    pub vehicle_label: Option<String>,
    pub plate_bbox: [f32; 4],
    pub plate_corners: Option<[[f32; 2]; 4]>,
    pub frame_offset_nanos: i64,
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
    pub quality: PlateQuality,
    pub crop_uri: Option<String>,
    pub is_best_shot: bool,
    pub embedding: Option<Vec<f32>>,
}

/// Assign + persist every plate read for one segment inside an open transaction. Returns the
/// resolved `plate_id` per input read (in order; `None` = left unattributed).
pub async fn assign_plates(
    tx: &mut Transaction<'_, Postgres>,
    segment_id: Uuid,
    device_id: &str,
    plates: &[PlateWrite],
    cfg: &PlateMatchConfig,
) -> Result<Vec<Option<Uuid>>> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(VISION_PLATE_LOCK_KEY)
        .execute(&mut **tx)
        .await
        .context("taking vision plate advisory lock")?;

    // Idempotency: a reprocessed segment returns its prior outcomes untouched.
    let prior =
        sqlx::query("SELECT plate_id FROM plate_detections WHERE segment_id = $1 ORDER BY id")
            .bind(segment_id)
            .fetch_all(&mut **tx)
            .await
            .context("reading prior plate assignments")?;
    if !prior.is_empty() {
        return prior
            .iter()
            .map(|r| r.try_get::<Option<Uuid>, _>("plate_id").map_err(Into::into))
            .collect();
    }

    sqlx::query("DELETE FROM plate_detections WHERE segment_id = $1")
        .bind(segment_id)
        .execute(&mut **tx)
        .await
        .context("clearing prior plate_detections")?;

    let mut assigned: Vec<Option<Uuid>> = Vec::with_capacity(plates.len());
    let mut touched: Vec<Uuid> = Vec::new();

    for p in plates {
        let norm = p.ocr_text_norm.as_str();
        let plate_id = if norm.chars().count() < cfg.min_len {
            None
        } else {
            match find_match(tx, norm, cfg).await? {
                Some(id) => {
                    sqlx::query(
                        "UPDATE license_plates SET n_samples = n_samples + 1, \
                         last_seen_unix_nanos = GREATEST(coalesce(last_seen_unix_nanos, 0), $2), \
                         updated_at = now() WHERE plate_id = $1",
                    )
                    .bind(id)
                    .bind(p.end_unix_nanos)
                    .execute(&mut **tx)
                    .await
                    .context("bumping matched plate")?;
                    Some(id)
                }
                None if p.quality == PlateQuality::Mint => {
                    Some(mint_plate(tx, p, device_id).await?)
                }
                None => None,
            }
        };
        if let Some(id) = plate_id {
            if !touched.contains(&id) {
                touched.push(id);
            }
        }

        let quality_tag = if p.quality == PlateQuality::Mint {
            "clean"
        } else {
            "marginal"
        };
        insert_detection(tx, segment_id, device_id, p, plate_id, quality_tag).await?;
        assigned.push(plate_id);
    }

    // Self-heal each touched plate's canonical text to its best clean read.
    for id in touched {
        recanonicalize(tx, id).await?;
    }
    Ok(assigned)
}

/// Exact (unique norm key) then trigram-candidate + edit-distance fuzzy match.
async fn find_match(
    tx: &mut Transaction<'_, Postgres>,
    norm: &str,
    cfg: &PlateMatchConfig,
) -> Result<Option<Uuid>> {
    if let Some(r) = sqlx::query("SELECT plate_id FROM license_plates WHERE plate_text_norm = $1")
        .bind(norm)
        .fetch_optional(&mut **tx)
        .await
        .context("exact plate match")?
    {
        return Ok(Some(r.get("plate_id")));
    }
    // Trigram candidates, ranked by similarity; accept the closest within the edit-distance budget.
    let rows = sqlx::query(
        "SELECT plate_id, plate_text_norm, similarity(plate_text_norm, $1) AS sim \
         FROM license_plates WHERE plate_text_norm % $1 \
         ORDER BY sim DESC LIMIT 5",
    )
    .bind(norm)
    .fetch_all(&mut **tx)
    .await
    .context("fuzzy plate candidates")?;
    for r in &rows {
        let sim: f32 = r.try_get::<f32, _>("sim").unwrap_or(0.0);
        let cand: String = r.get("plate_text_norm");
        if sim >= cfg.fuzzy_min_similarity && edit_distance(norm, &cand) <= cfg.max_edit_distance {
            return Ok(Some(r.get("plate_id")));
        }
    }
    Ok(None)
}

/// Mint a new catalog plate (race-safe on the unique norm key).
async fn mint_plate(
    tx: &mut Transaction<'_, Postgres>,
    p: &PlateWrite,
    device_id: &str,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    let row = sqlx::query(
        "INSERT INTO license_plates \
         (plate_id, plate_text, plate_text_norm, n_samples, \
          first_seen_unix_nanos, last_seen_unix_nanos, first_seen_device_id) \
         VALUES ($1, $2, $3, 1, $4, $4, $5) \
         ON CONFLICT (plate_text_norm) DO UPDATE SET \
            n_samples = license_plates.n_samples + 1, \
            last_seen_unix_nanos = EXCLUDED.last_seen_unix_nanos, updated_at = now() \
         RETURNING plate_id",
    )
    .bind(id)
    .bind(&p.ocr_text)
    .bind(&p.ocr_text_norm)
    .bind(p.end_unix_nanos)
    .bind(device_id)
    .fetch_one(&mut **tx)
    .await
    .context("minting plate")?;
    let resolved: Uuid = row.get("plate_id");
    if resolved == id {
        tracing::info!(plate_id = %id, text = %p.ocr_text, "minted new license plate");
    }
    Ok(resolved)
}

async fn insert_detection(
    tx: &mut Transaction<'_, Postgres>,
    segment_id: Uuid,
    device_id: &str,
    p: &PlateWrite,
    plate_id: Option<Uuid>,
    quality_tag: &str,
) -> Result<()> {
    let vbox = p
        .vehicle_bbox
        .map(|b| format!("[{},{},{},{}]", b[0], b[1], b[2], b[3]));
    let pbox = format!(
        "[{},{},{},{}]",
        p.plate_bbox[0], p.plate_bbox[1], p.plate_bbox[2], p.plate_bbox[3]
    );
    let corners = p.plate_corners.map(|c| {
        format!(
            "[[{},{}],[{},{}],[{},{}],[{},{}]]",
            c[0][0], c[0][1], c[1][0], c[1][1], c[2][0], c[2][1], c[3][0], c[3][1]
        )
    });
    let confs = serde_json::to_string(&p.char_confidences).unwrap_or_else(|_| "[]".into());
    let embedding = p.embedding.clone().map(pgvector::Vector::from);
    sqlx::query(
        "INSERT INTO plate_detections \
         (segment_id, device_id, plate_id, start_unix_nanos, end_unix_nanos, frame_offset_nanos, \
          vehicle_bbox, vehicle_label, plate_bbox, plate_corners, ocr_text, ocr_text_norm, \
          ocr_confidence, char_confidences, det_score, quality, embedding, crop_uri, is_best_shot) \
         VALUES ($1,$2,$3,$4,$5,$6,$7::jsonb,$8,$9::jsonb,$10::jsonb,$11,$12,$13,$14::jsonb,$15,$16,$17,$18,$19)",
    )
    .bind(segment_id)
    .bind(device_id)
    .bind(plate_id)
    .bind(p.start_unix_nanos)
    .bind(p.end_unix_nanos)
    .bind(p.frame_offset_nanos)
    .bind(vbox)
    .bind(p.vehicle_label.as_deref())
    .bind(pbox)
    .bind(corners)
    .bind(&p.ocr_text)
    .bind(&p.ocr_text_norm)
    .bind(p.mean_conf)
    .bind(confs)
    .bind(p.det_score)
    .bind(quality_tag)
    .bind(embedding)
    .bind(p.crop_uri.as_deref())
    .bind(p.is_best_shot)
    .execute(&mut **tx)
    .await
    .context("inserting plate_detection")?;
    Ok(())
}

/// Self-healing canonical text: set `plate_text` to the highest-confidence clean read for the plate.
async fn recanonicalize(tx: &mut Transaction<'_, Postgres>, plate_id: Uuid) -> Result<()> {
    let best = sqlx::query(
        "SELECT ocr_text FROM plate_detections \
         WHERE plate_id = $1 AND quality = 'clean' \
         ORDER BY ocr_confidence DESC NULLS LAST, created_at DESC LIMIT 1",
    )
    .bind(plate_id)
    .fetch_optional(&mut **tx)
    .await
    .context("reading best clean plate read")?;
    if let Some(r) = best {
        let text: String = r.get("ocr_text");
        sqlx::query("UPDATE license_plates SET plate_text = $1, updated_at = now() WHERE plate_id = $2")
            .bind(text)
            .bind(plate_id)
            .execute(&mut **tx)
            .await
            .context("updating canonical plate text")?;
    }
    Ok(())
}
