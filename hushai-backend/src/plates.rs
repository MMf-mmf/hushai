//! Authenticated read/admin surface over the derived license-plate (ALPR) catalog — the vehicle
//! sibling of `persons.rs` (faces).
//!
//! `GET   /v1/plates`                   — list discovered plates + bounded recent sightings
//! `GET   /v1/plates/search?q=...`      — exact + fuzzy (trigram) lookup by plate text
//! `PATCH /v1/plates/{id}`              — set display_name (name a plate, e.g. "Mom's car")
//! `POST  /v1/plates/{id}/merge`        — fold two ids for the same plate into one
//! `GET   /v1/plates/{id}/sample-crop`  — a representative cropped plate (ID a plate by sight)
//!
//! KEY DIVERGENCE from persons/speakers: a plate's identity IS its NORMALIZED TEXT, not an
//! embedding, so there's no centroid to blend on merge and search is string-based (exact +
//! pg_trgm fuzzy for OCR noise). Otherwise this clones persons.rs: runtime sqlx
//! (`query`/`.bind`/`try_get`), gap-sessionized sightings, and the crop fast-path → ffmpeg
//! fallback. `plate_detections` holds one row PER OCR READ (many per segment); `license_plates`
//! and `plate_detections.plate_id` are both `uuid`, so no text casts.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::error::IngestError;
use crate::persons::{extract_jpeg, parse_bbox};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct PlateSummary {
    pub plate_id: Uuid,
    /// Canonical, voted human-facing plate string (the display text).
    pub plate_text: String,
    pub display_name: Option<String>,
    /// Raw per-read template count (one `plate_detections` row PER OCR READ PER SAMPLED FRAME).
    /// Internal weight for voting / merge folding — NOT a human "sightings" count, since a single
    /// pass of one vehicle is read across many frames (and many contiguous ~2s segments), so this
    /// over-counts a single appearance. The UI shows `n_sightings` instead.
    pub n_samples: i64,
    /// Human-meaningful number of distinct appearances: detections sessionized by time gap, so the
    /// many reads of one pass — and a run of contiguous segments of one continuous appearance —
    /// collapse to a single sighting. See [`sighting_gap_nanos`].
    pub n_sightings: i64,
    /// Absolute timestamps of up to 3 recent sightings (a "when did we see this plate" hint).
    pub sample_sighting_unix_nanos: Vec<i64>,
}

/// A new "sighting" begins when consecutive detections of the same plate are more than this many
/// nanoseconds apart; closer detections collapse into one. Tunable via `PLATE_SIGHTING_GAP_SECONDS`
/// (default 60s). This turns the raw per-read `plate_detections` rows into the count of distinct
/// appearances the UI labels "N sightings" — so a single pass reads as 1, not ~20.
fn sighting_gap_nanos() -> i64 {
    let secs: i64 = std::env::var("PLATE_SIGHTING_GAP_SECONDS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(60);
    secs.max(0).saturating_mul(1_000_000_000)
}

/// `GET /v1/plates` — the global (cross-device) catalog with a time-clustered sighting count and
/// up to 3 recent sighting times per plate (LATERAL correlated subqueries, cheap on the
/// partitioned `plate_detections`).
pub async fn list_plates(State(st): State<AppState>) -> Result<Json<Vec<PlateSummary>>, IngestError> {
    let rows = sqlx::query(
        r#"
        SELECT lp.plate_id,
               lp.plate_text,
               lp.display_name,
               lp.n_samples,
               COALESCE(sight.n, 0) AS n_sightings,
               COALESCE(samp.ts, ARRAY[]::bigint[]) AS sample_sightings
        FROM license_plates lp
        LEFT JOIN LATERAL (
            -- Sessionize this plate's detections by time gap: a sighting starts at the first
            -- detection (gap IS NULL) and whenever the gap to the previous one exceeds $1 ns.
            SELECT count(*) AS n
            FROM (
                SELECT start_unix_nanos
                         - lag(start_unix_nanos) OVER (ORDER BY start_unix_nanos) AS gap
                FROM plate_detections
                WHERE plate_id = lp.plate_id
                  AND start_unix_nanos IS NOT NULL
            ) g
            WHERE g.gap IS NULL OR g.gap > $1
        ) sight ON true
        LEFT JOIN LATERAL (
            SELECT array_agg(q.t ORDER BY q.t DESC) AS ts
            FROM (
                SELECT start_unix_nanos AS t
                FROM plate_detections
                WHERE plate_id = lp.plate_id
                  AND start_unix_nanos IS NOT NULL
                ORDER BY start_unix_nanos DESC
                LIMIT 3
            ) q
        ) samp ON true
        ORDER BY n_sightings DESC, lp.n_samples DESC
        "#,
    )
    .bind(sighting_gap_nanos())
    .fetch_all(&st.pool)
    .await?;

    let out = rows
        .into_iter()
        .map(|r| PlateSummary {
            plate_id: r.get("plate_id"),
            plate_text: r.get("plate_text"),
            display_name: r
                .try_get::<Option<String>, _>("display_name")
                .unwrap_or(None),
            n_samples: r.get("n_samples"),
            n_sightings: r.get("n_sightings"),
            sample_sighting_unix_nanos: r
                .try_get::<Vec<i64>, _>("sample_sightings")
                .unwrap_or_default(),
        })
        .collect();
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
}

/// Fold a free-text query into the catalog's `plate_text_norm` matching key: uppercase + strip
/// every non-alphanumeric char (spaces, dashes, dots). Matches the worker's normalization so a
/// user typing "abc-123" or "abc 123" finds plate "ABC123".
fn normalize_query(q: &str) -> String {
    q.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_uppercase())
        .collect()
}

/// `GET /v1/plates/search?q=...` — find catalog plates by text: an exact normalized-key hit OR
/// trigram-similar candidates (OCR noise / partial reads), best-first. Empty/blank `q` yields no
/// rows. The `pg_trgm` `%` operator + `gin` trigram index back the fuzzy path.
pub async fn search_plates(
    State(st): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Vec<PlateSummary>>, IngestError> {
    let norm = normalize_query(&q.q);
    if norm.is_empty() {
        return Ok(Json(Vec::new()));
    }

    // Exact OR trigram-similar, ordered by similarity (exact == 1.0 sorts first). The sighting
    // columns mirror `list_plates` so the UI can render search hits with the same shape.
    let rows = sqlx::query(
        r#"
        SELECT lp.plate_id,
               lp.plate_text,
               lp.display_name,
               lp.n_samples,
               COALESCE(sight.n, 0) AS n_sightings,
               COALESCE(samp.ts, ARRAY[]::bigint[]) AS sample_sightings
        FROM license_plates lp
        LEFT JOIN LATERAL (
            SELECT count(*) AS n
            FROM (
                SELECT start_unix_nanos
                         - lag(start_unix_nanos) OVER (ORDER BY start_unix_nanos) AS gap
                FROM plate_detections
                WHERE plate_id = lp.plate_id
                  AND start_unix_nanos IS NOT NULL
            ) g
            WHERE g.gap IS NULL OR g.gap > $2
        ) sight ON true
        LEFT JOIN LATERAL (
            SELECT array_agg(q.t ORDER BY q.t DESC) AS ts
            FROM (
                SELECT start_unix_nanos AS t
                FROM plate_detections
                WHERE plate_id = lp.plate_id
                  AND start_unix_nanos IS NOT NULL
                ORDER BY start_unix_nanos DESC
                LIMIT 3
            ) q
        ) samp ON true
        WHERE lp.plate_text_norm = $1 OR lp.plate_text_norm % $1
        ORDER BY similarity(lp.plate_text_norm, $1) DESC
        LIMIT 20
        "#,
    )
    .bind(&norm)
    .bind(sighting_gap_nanos())
    .fetch_all(&st.pool)
    .await?;

    let out = rows
        .into_iter()
        .map(|r| PlateSummary {
            plate_id: r.get("plate_id"),
            plate_text: r.get("plate_text"),
            display_name: r
                .try_get::<Option<String>, _>("display_name")
                .unwrap_or(None),
            n_samples: r.get("n_samples"),
            n_sightings: r.get("n_sightings"),
            sample_sighting_unix_nanos: r
                .try_get::<Vec<i64>, _>("sample_sightings")
                .unwrap_or_default(),
        })
        .collect();
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct RenameReq {
    pub display_name: String,
}

#[derive(Debug, Serialize)]
pub struct PlateRow {
    pub plate_id: Uuid,
    pub plate_text: String,
    pub display_name: Option<String>,
    pub n_samples: i64,
}

/// `PATCH /v1/plates/{id}` — name a plate (idempotent). 404 if the id is unknown.
pub async fn rename_plate(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<RenameReq>,
) -> Result<Json<PlateRow>, IngestError> {
    let name = req.display_name.trim();
    if name.is_empty() {
        return Err(IngestError::BadRequest(
            "display_name must not be empty".into(),
        ));
    }
    let row = sqlx::query(
        "UPDATE license_plates SET display_name = $1, updated_at = now() \
         WHERE plate_id = $2 RETURNING plate_id, plate_text, display_name, n_samples",
    )
    .bind(name)
    .bind(id)
    .fetch_optional(&st.pool)
    .await?
    .ok_or(IngestError::NotFound("plate"))?;

    Ok(Json(PlateRow {
        plate_id: row.get("plate_id"),
        plate_text: row.get("plate_text"),
        display_name: row
            .try_get::<Option<String>, _>("display_name")
            .unwrap_or(None),
        n_samples: row.get("n_samples"),
    }))
}

#[derive(Debug, Deserialize)]
pub struct MergeReq {
    pub into: Uuid,
}

/// `POST /v1/plates/{id}/merge` — fold the path id (loser) into `into` (survivor): repoint
/// `plate_detections.plate_id`, fold the loser's `n_samples` into the survivor, preserve the
/// survivor's display_name, delete the loser. (OCR noise occasionally mints two ids — e.g.
/// "ABC123"/"ABCl23" — for the same plate; a human combines them. There's no centroid to blend,
/// since a plate's identity is its text, not a vector.)
pub async fn merge_plate(
    State(st): State<AppState>,
    Path(loser): Path<Uuid>,
    Json(req): Json<MergeReq>,
) -> Result<StatusCode, IngestError> {
    let into = req.into;
    if loser == into {
        return Err(IngestError::BadRequest(
            "cannot merge a plate into itself".into(),
        ));
    }

    let mut tx = st.pool.begin().await?;

    // Lock both rows in a stable id order (deadlock-safe); read counts.
    let (lo, hi) = if loser < into {
        (loser, into)
    } else {
        (into, loser)
    };
    let rows = sqlx::query(
        "SELECT plate_id, n_samples FROM license_plates \
         WHERE plate_id IN ($1, $2) FOR UPDATE",
    )
    .bind(lo)
    .bind(hi)
    .fetch_all(&mut *tx)
    .await?;
    if rows.len() != 2 {
        return Err(IngestError::NotFound("plate (loser or survivor)"));
    }

    let mut loser_n: i64 = 0;
    for r in &rows {
        let id: Uuid = r.get("plate_id");
        let n: i64 = r.get("n_samples");
        if id == loser {
            loser_n = n;
        }
    }

    // Repoint raw OCR reads. plate_detections.plate_id is uuid (no text cast).
    sqlx::query("UPDATE plate_detections SET plate_id = $1 WHERE plate_id = $2")
        .bind(into)
        .bind(loser)
        .execute(&mut *tx)
        .await?;

    // No centroid to blend: just fold the loser's raw read count into the survivor.
    sqlx::query(
        "UPDATE license_plates SET n_samples = n_samples + $1, updated_at = now() \
         WHERE plate_id = $2",
    )
    .bind(loser_n)
    .bind(into)
    .execute(&mut *tx)
    .await?;

    // Keep any "of interest" watch alive across the merge (repoint/drop) before the loser disappears.
    crate::watchlist::reconcile_merge(&mut tx, "plate", loser, into).await?;

    sqlx::query("DELETE FROM license_plates WHERE plate_id = $1")
        .bind(loser)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(StatusCode::OK)
}

/// `GET /v1/plates/{id}/sample-crop` — return a cropped JPEG of the best stored read so a human
/// can identify the plate/vehicle by sight (text alone can be a wrong OCR). Picks the best
/// `plate_detection` (best-shot → OCR confidence → detector score), reconstructs a decodable file
/// (prepend `codec_init_data` only for an `fmp4` fragment — key off `container`, same rule as
/// `sample_face`), then ffmpeg seeks to the sampled frame and crops to the stored `plate_bbox`.
pub async fn sample_plate_crop(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Response, IngestError> {
    // Prefer the persisted rectified plate crop: the worker already did the perspective rectify, so
    // this shows the operator the clearest plate instead of a raw re-crop. Order best-shot →
    // ocr_confidence → det_score so we surface the best available regardless of which columns are
    // populated (older rows have neither crop_uri nor confidences).
    let row = sqlx::query(
        "SELECT seg.blob_uri, seg.container, seg.codec_init_data, \
                pd.plate_bbox::text AS bbox_json, pd.frame_offset_nanos, pd.crop_uri \
         FROM plate_detections pd JOIN segments seg ON seg.segment_id = pd.segment_id \
         WHERE pd.plate_id = $1 \
         ORDER BY pd.is_best_shot DESC, pd.ocr_confidence DESC NULLS LAST, \
                  pd.det_score DESC NULLS LAST, pd.created_at \
         LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await?
    .ok_or(IngestError::NotFound("sample crop for plate"))?;

    // Fast path: a stored rectified crop. Canonicalize + require under blob_root (same guard as raw
    // blobs); on any miss, fall through to re-cropping the source frame so the endpoint never fails.
    let crop_uri: Option<String> = row.try_get("crop_uri").unwrap_or(None);
    if let Some(crop) = crop_uri.as_deref().filter(|s| !s.is_empty()) {
        if let Ok(path) = tokio::fs::canonicalize(crop).await {
            if path.starts_with(&*st.blob_root) {
                if let Ok(bytes) = tokio::fs::read(&path).await {
                    if !bytes.is_empty() {
                        let len = bytes.len();
                        return Response::builder()
                            .header(CONTENT_TYPE, "image/jpeg")
                            .header(CONTENT_LENGTH, len)
                            .body(Body::from(bytes))
                            .map_err(|e| {
                                IngestError::Internal(anyhow::anyhow!("building response: {e}"))
                            });
                    }
                }
            }
        }
    }

    let blob_uri: String = row.get("blob_uri");
    let container: String = row.get("container");
    let codec_init_data: Option<Vec<u8>> = row.try_get("codec_init_data").unwrap_or(None);
    let bbox_json: Option<String> = row.try_get("bbox_json").unwrap_or(None);
    let frame_offset_nanos: Option<i64> = row.try_get("frame_offset_nanos").unwrap_or(None);
    let bbox = bbox_json.as_deref().and_then(parse_bbox);
    let offset_secs = frame_offset_nanos.unwrap_or(0).max(0) as f64 / 1e9;

    let raw = blob_uri
        .strip_prefix("file://")
        .ok_or(IngestError::BadRequest("unsupported blob scheme".into()))?;
    // Path-traversal guard: canonicalize + require under blob_root (404 off-root, no leak).
    let path = tokio::fs::canonicalize(raw)
        .await
        .map_err(|_| IngestError::NotFound("blob"))?;
    if !path.starts_with(&*st.blob_root) {
        return Err(IngestError::NotFound("blob"));
    }
    let media = tokio::fs::read(&path)
        .await
        .map_err(|e| IngestError::Internal(e.into()))?;

    // Reconstruct a decodable file: only a bare fMP4 fragment needs the init prepended.
    let mut bytes = Vec::new();
    if container.eq_ignore_ascii_case("fmp4") {
        if let Some(init) = &codec_init_data {
            bytes.extend_from_slice(init);
        }
    }
    bytes.extend_from_slice(&media);

    let tmp = tempfile::NamedTempFile::new().map_err(|e| IngestError::Internal(e.into()))?;
    tokio::fs::write(tmp.path(), &bytes)
        .await
        .map_err(|e| IngestError::Internal(e.into()))?;

    // Try the cropped frame; if the crop is out of bounds / fails, fall back to the full frame
    // so the endpoint always yields a usable image.
    let jpeg = match extract_jpeg(tmp.path(), offset_secs, bbox).await {
        Ok(b) if !b.is_empty() => b,
        _ => extract_jpeg(tmp.path(), offset_secs, None)
            .await
            .map_err(IngestError::Internal)?,
    };
    if jpeg.is_empty() {
        return Err(IngestError::Internal(anyhow::anyhow!(
            "ffmpeg produced no frame"
        )));
    }
    let len = jpeg.len();
    Response::builder()
        .header(CONTENT_TYPE, "image/jpeg")
        .header(CONTENT_LENGTH, len)
        .body(Body::from(jpeg))
        .map_err(|e| IngestError::Internal(anyhow::anyhow!("building response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_query_uppercases_and_strips() {
        assert_eq!(normalize_query("abc-123"), "ABC123");
        assert_eq!(normalize_query(" ab c 1.2.3 "), "ABC123");
        assert_eq!(normalize_query("!@#$"), "");
        assert_eq!(normalize_query(""), "");
    }
}
