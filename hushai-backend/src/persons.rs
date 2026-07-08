//! Authenticated read/admin surface over the derived person (face) catalog — the visual
//! sibling of `speakers.rs`.
//!
//! `GET  /v1/persons`                  — list discovered persons + bounded recent sightings
//! `PATCH /v1/persons/{id}`            — set display_name (name a face)
//! `POST /v1/persons/{id}/merge`       — fold two ids for the same person into one
//! `POST /v1/persons/{id}/archive`     — disregard (display-level; matcher still attributes)
//! `POST /v1/persons/{id}/unarchive`   — restore from the Archived section
//! `POST /v1/persons/{id}/owner`       — mark this face as the device owner ("This is me")
//! `POST /v1/persons/{id}/unowner`     — clear the owner mark
//! `GET  /v1/persons/{id}/sample-face` — a representative cropped face (ID a person by sight)
//!
//! Runtime sqlx (`query`/`.bind`/`try_get`), NOT the `query!` macros, for the same reason as
//! speakers.rs: the vision tables aren't in the committed `.sqlx/` cache. `person_segments`
//! holds one row PER DETECTED FACE (many per segment); `persons.person_id` and
//! `person_segments.person_id` are both `uuid` (the 0009 type contract), so no text casts.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::error::IngestError;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct PersonSummary {
    pub person_id: Uuid,
    pub display_name: Option<String>,
    /// Raw per-face template count (one `person_segments` row PER DETECTED FACE PER SAMPLED FRAME).
    /// Internal weight for centroid math / merge blending — NOT a human "sightings" count, since a
    /// single short clip is sampled across many frames (and many contiguous ~2s segments), so this
    /// over-counts a single appearance. The UI shows `n_sightings` instead.
    pub n_samples: i64,
    /// Human-meaningful number of distinct appearances: detections sessionized by time gap, so the
    /// many frames of one short clip — and a run of contiguous segments of one continuous
    /// appearance — collapse to a single sighting. See [`sighting_gap_nanos`].
    pub n_sightings: i64,
    /// Absolute timestamps of up to 3 recent sightings (a "when did we see this face" hint).
    pub sample_sighting_unix_nanos: Vec<i64>,
    /// Disregarded by the operator (0021). Display-level only: clients tuck archived entries
    /// into a collapsed "Archived" section; matching/RAG/watchlist behavior is unchanged.
    pub archived: bool,
    /// The device owner's face (0023, "This is me"). At most one person carries this;
    /// hushai-rag's owner resolution consults it before the OWNER_PERSON_* env fallback.
    pub is_owner: bool,
}

/// A new "sighting" begins when consecutive detections of the same face are more than this many
/// nanoseconds apart; closer detections collapse into one. Tunable via `PERSON_SIGHTING_GAP_SECONDS`
/// (default 60s). This turns the raw per-frame `person_segments` rows into the count of distinct
/// appearances the UI labels "N sightings" — so a single few-second clip reads as 1, not ~20.
fn sighting_gap_nanos() -> i64 {
    let secs: i64 = std::env::var("PERSON_SIGHTING_GAP_SECONDS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(60);
    secs.max(0).saturating_mul(1_000_000_000)
}

/// `GET /v1/persons` — the global (cross-device) catalog with a time-clustered sighting count and
/// up to 3 recent sighting times per person (LATERAL correlated subqueries, cheap on the
/// partitioned `person_segments`).
pub async fn list_persons(
    State(st): State<AppState>,
) -> Result<Json<Vec<PersonSummary>>, IngestError> {
    let rows = sqlx::query(
        r#"
        SELECT p.person_id,
               p.display_name,
               p.n_samples,
               p.archived_at IS NOT NULL AS archived,
               p.is_owner,
               COALESCE(sight.n, 0) AS n_sightings,
               COALESCE(samp.ts, ARRAY[]::bigint[]) AS sample_sightings
        FROM persons p
        LEFT JOIN LATERAL (
            -- Sessionize this face's detections by time gap: a sighting starts at the first
            -- detection (gap IS NULL) and whenever the gap to the previous one exceeds $1 ns.
            SELECT count(*) AS n
            FROM (
                SELECT start_unix_nanos
                         - lag(start_unix_nanos) OVER (ORDER BY start_unix_nanos) AS gap
                FROM person_segments
                WHERE person_id = p.person_id
                  AND start_unix_nanos IS NOT NULL
            ) g
            WHERE g.gap IS NULL OR g.gap > $1
        ) sight ON true
        LEFT JOIN LATERAL (
            SELECT array_agg(q.t ORDER BY q.t DESC) AS ts
            FROM (
                SELECT start_unix_nanos AS t
                FROM person_segments
                WHERE person_id = p.person_id
                  AND start_unix_nanos IS NOT NULL
                ORDER BY start_unix_nanos DESC
                LIMIT 3
            ) q
        ) samp ON true
        ORDER BY n_sightings DESC, p.n_samples DESC
        "#,
    )
    .bind(sighting_gap_nanos())
    .fetch_all(&st.pool)
    .await?;

    let out = rows
        .into_iter()
        .map(|r| PersonSummary {
            person_id: r.get("person_id"),
            display_name: r
                .try_get::<Option<String>, _>("display_name")
                .unwrap_or(None),
            n_samples: r.get("n_samples"),
            n_sightings: r.get("n_sightings"),
            sample_sighting_unix_nanos: r
                .try_get::<Vec<i64>, _>("sample_sightings")
                .unwrap_or_default(),
            archived: r.try_get("archived").unwrap_or(false),
            is_owner: r.try_get("is_owner").unwrap_or(false),
        })
        .collect();
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct RenameReq {
    pub display_name: String,
}

#[derive(Debug, Serialize)]
pub struct PersonRow {
    pub person_id: Uuid,
    pub display_name: Option<String>,
    pub n_samples: i64,
    pub archived: bool,
    pub is_owner: bool,
}

/// `PATCH /v1/persons/{id}` — name a face (idempotent). 404 if the id is unknown.
pub async fn rename_person(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<RenameReq>,
) -> Result<Json<PersonRow>, IngestError> {
    let name = req.display_name.trim();
    if name.is_empty() {
        return Err(IngestError::BadRequest(
            "display_name must not be empty".into(),
        ));
    }
    let mut tx = st.pool.begin().await?;
    let row = sqlx::query(
        "UPDATE persons SET display_name = $1, updated_at = now() \
         WHERE person_id = $2 \
         RETURNING person_id, display_name, n_samples, archived_at IS NOT NULL AS archived, is_owner",
    )
    .bind(name)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(IngestError::NotFound("person"))?;
    // Running memory: record the identification moment — the accumulated anonymous history is
    // now attached to this name (profiles are keyed by id, so nothing moves).
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    crate::profiles::note_identified_in_tx(&mut tx, "person", id, name, now_ns, 0).await?;
    tx.commit().await?;

    Ok(Json(person_row(&row)))
}

fn person_row(row: &sqlx::postgres::PgRow) -> PersonRow {
    PersonRow {
        person_id: row.get("person_id"),
        display_name: row
            .try_get::<Option<String>, _>("display_name")
            .unwrap_or(None),
        n_samples: row.get("n_samples"),
        archived: row.try_get("archived").unwrap_or(false),
        is_owner: row.try_get("is_owner").unwrap_or(false),
    }
}

/// `POST /v1/persons/{id}/archive` — disregard a face (idempotent). Display-level only: the
/// face matcher still attributes new detections to it (else the next sighting would re-mint a
/// duplicate that reappears under "Unidentified"). 404 if the id is unknown.
pub async fn archive_person(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<PersonRow>, IngestError> {
    set_person_archived(&st, id, true).await
}

/// `POST /v1/persons/{id}/unarchive` — restore a disregarded face (idempotent).
pub async fn unarchive_person(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<PersonRow>, IngestError> {
    set_person_archived(&st, id, false).await
}

async fn set_person_archived(
    st: &AppState,
    id: Uuid,
    archived: bool,
) -> Result<Json<PersonRow>, IngestError> {
    let row = sqlx::query(
        "UPDATE persons \
         SET archived_at = CASE WHEN $1 THEN now() ELSE NULL END, updated_at = now() \
         WHERE person_id = $2 \
         RETURNING person_id, display_name, n_samples, archived_at IS NOT NULL AS archived, is_owner",
    )
    .bind(archived)
    .bind(id)
    .fetch_optional(&st.pool)
    .await?
    .ok_or(IngestError::NotFound("person"))?;

    Ok(Json(person_row(&row)))
}

/// `POST /v1/persons/{id}/owner` — mark this face as the device owner ("This is me").
/// One transaction: clear any previous owner, set the new one (404 on unknown/archived).
/// Idempotent; the 0023 partial unique index makes a racing double-set fail loudly.
pub async fn set_person_owner(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<PersonRow>, IngestError> {
    let mut tx = st.pool.begin().await?;
    sqlx::query("UPDATE persons SET is_owner = false, updated_at = now() WHERE is_owner AND person_id <> $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query(
        "UPDATE persons SET is_owner = true, updated_at = now() \
         WHERE person_id = $1 AND archived_at IS NULL \
         RETURNING person_id, display_name, n_samples, archived_at IS NOT NULL AS archived, is_owner",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(IngestError::NotFound("person (unknown or archived)"))?;
    tx.commit().await?;
    Ok(Json(person_row(&row)))
}

/// `POST /v1/persons/{id}/unowner` — clear the owner mark (idempotent). 404 if unknown.
pub async fn clear_person_owner(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<PersonRow>, IngestError> {
    let row = sqlx::query(
        "UPDATE persons SET is_owner = false, updated_at = now() \
         WHERE person_id = $1 \
         RETURNING person_id, display_name, n_samples, archived_at IS NOT NULL AS archived, is_owner",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await?
    .ok_or(IngestError::NotFound("person"))?;
    Ok(Json(person_row(&row)))
}

#[derive(Debug, Deserialize)]
pub struct MergeReq {
    pub into: Uuid,
}

/// `POST /v1/persons/{id}/merge` — fold the path id (loser) into `into` (survivor): repoint
/// `person_segments.person_id`, combine centroids weighted by n_samples, preserve the
/// survivor's display_name, delete the loser. (The conservative matcher over-splits, so a
/// human occasionally needs to combine two ids of the same face.)
pub async fn merge_person(
    State(st): State<AppState>,
    Path(loser): Path<Uuid>,
    Json(req): Json<MergeReq>,
) -> Result<StatusCode, IngestError> {
    let into = req.into;
    if loser == into {
        return Err(IngestError::BadRequest(
            "cannot merge a person into itself".into(),
        ));
    }

    let mut tx = st.pool.begin().await?;

    // Lock both rows in a stable id order (deadlock-safe); read centroids + counts.
    let (lo, hi) = if loser < into {
        (loser, into)
    } else {
        (into, loser)
    };
    let rows = sqlx::query(
        "SELECT person_id, centroid, n_samples FROM persons \
         WHERE person_id IN ($1, $2) FOR UPDATE",
    )
    .bind(lo)
    .bind(hi)
    .fetch_all(&mut *tx)
    .await?;
    if rows.len() != 2 {
        return Err(IngestError::NotFound("person (loser or survivor)"));
    }

    let mut loser_c: Option<pgvector::Vector> = None;
    let mut loser_n: i64 = 0;
    let mut into_c: Option<pgvector::Vector> = None;
    let mut into_n: i64 = 0;
    for r in &rows {
        let id: Uuid = r.get("person_id");
        let c = r
            .try_get::<Option<pgvector::Vector>, _>("centroid")
            .unwrap_or(None);
        let n: i64 = r.get("n_samples");
        if id == loser {
            loser_c = c;
            loser_n = n;
        } else {
            into_c = c;
            into_n = n;
        }
    }

    // Repoint raw face templates. person_segments.person_id is uuid (no text cast).
    sqlx::query("UPDATE person_segments SET person_id = $1 WHERE person_id = $2")
        .bind(into)
        .bind(loser)
        .execute(&mut *tx)
        .await?;

    let merged = weighted_renorm(into_c.as_ref(), into_n, loser_c.as_ref(), loser_n);
    sqlx::query(
        "UPDATE persons SET centroid = $1, n_samples = $2, updated_at = now() WHERE person_id = $3",
    )
    .bind(merged)
    .bind(into_n + loser_n)
    .bind(into)
    .execute(&mut *tx)
    .await?;

    // Keep any "of interest" watch alive across the merge: repoint (or drop) the loser's watch +
    // managed rule to the survivor before the loser id disappears (else the watch silently dies).
    crate::watchlist::reconcile_merge(&mut tx, "person", loser, into).await?;
    // Fold the loser's accumulated running-memory profile into the survivor's.
    crate::profiles::merge_in_tx(&mut tx, "person", loser, into).await?;
    // Repoint the loser's Gotham graph edges onto the survivor (fold duplicates, drop self-edges).
    crate::graph_pass::merge_in_tx(&mut tx, "person", loser, into).await?;

    sqlx::query("DELETE FROM persons WHERE person_id = $1")
        .bind(loser)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(StatusCode::OK)
}

/// Weighted mean of two (optional) centroids, L2-renormalized. Falls back to whichever is
/// present; `None` if both are absent. (Mirror of speakers::weighted_renorm.)
fn weighted_renorm(
    a: Option<&pgvector::Vector>,
    na: i64,
    b: Option<&pgvector::Vector>,
    nb: i64,
) -> Option<pgvector::Vector> {
    match (a, b) {
        (Some(a), Some(b)) if a.as_slice().len() == b.as_slice().len() => {
            let (wa, wb) = (na.max(0) as f32, nb.max(0) as f32);
            let denom = (wa + wb).max(1.0);
            let mut v: Vec<f32> = a
                .as_slice()
                .iter()
                .zip(b.as_slice())
                .map(|(x, y)| (x * wa + y * wb) / denom)
                .collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in &mut v {
                    *x /= norm;
                }
            }
            Some(pgvector::Vector::from(v))
        }
        (Some(a), _) => Some(a.clone()),
        (_, Some(b)) => Some(b.clone()),
        (None, None) => None,
    }
}

/// `GET /v1/persons/{id}/sample-face` — return a cropped JPEG of the best stored sighting so a
/// human can name the face by sight (a label/vector alone isn't human-identifiable). Picks the
/// highest-confidence `person_segment`, reconstructs a decodable file (prepend `codec_init_data`
/// only for an `fmp4` fragment — key off `container`, same rule as `sample_audio`/the viewer
/// remux), then ffmpeg seeks to the sampled frame and crops to the stored bbox.
pub async fn sample_face(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Response, IngestError> {
    // Prefer the persisted CLEANED (restored) best-shot crop: the image-cleanup stage already did
    // the zoom/restore, so this shows the operator the clearest face instead of a raw re-crop. Order
    // best-shot → quality_score → det_score so we surface the best available regardless of which
    // columns are populated (older rows have neither crop_uri nor quality_score).
    let row = sqlx::query(
        "SELECT seg.blob_uri, seg.container, seg.codec_init_data, \
                ps.bbox::text AS bbox_json, ps.frame_offset_nanos, ps.crop_uri \
         FROM person_segments ps JOIN segments seg ON seg.segment_id = ps.segment_id \
         WHERE ps.person_id = $1 \
         ORDER BY ps.is_best_shot DESC, ps.quality_score DESC NULLS LAST, \
                  ps.det_score DESC NULLS LAST, ps.created_at \
         LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await?
    .ok_or(IngestError::NotFound("sample face for person"))?;

    // Fast path: a stored cleaned crop. Canonicalize + require under blob_root (same guard as raw
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

/// Decode a single frame at `offset_secs`, optionally cropped to `[x,y,w,h]` (pixels), as JPEG.
/// `pub(crate)` so the ALPR sample-crop path (`plates.rs`) reuses the exact same ffmpeg invocation.
pub(crate) async fn extract_jpeg(
    path: &std::path::Path,
    offset_secs: f64,
    bbox: Option<[f32; 4]>,
) -> anyhow::Result<Vec<u8>> {
    let ffmpeg = std::env::var("FFMPEG_BIN").unwrap_or_else(|_| "ffmpeg".to_string());
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-nostdin")
        .args(["-v", "error"])
        .args(["-ss", &format!("{offset_secs:.3}")])
        .arg("-i")
        .arg(path)
        .args(["-frames:v", "1"]);
    if let Some([x, y, w, h]) = bbox {
        // Round to even-ish ints; clamp origin >= 0. ffmpeg errors if the rect exceeds the
        // frame — the caller falls back to a full-frame extract on error.
        let (x, y, w, h) = (
            x.max(0.0) as i32,
            y.max(0.0) as i32,
            w.max(1.0) as i32,
            h.max(1.0) as i32,
        );
        cmd.args(["-vf", &format!("crop={w}:{h}:{x}:{y}")]);
    }
    cmd.args(["-f", "image2", "-c:v", "mjpeg", "-"]);
    let out = cmd.output().await?;
    if !out.status.success() {
        anyhow::bail!(
            "ffmpeg frame extract failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(out.stdout)
}

/// Parse a JSONB-as-text bbox `[x,y,w,h]`; `None` if malformed. `pub(crate)` so `plates.rs`
/// parses `plate_bbox` with the identical contract.
pub(crate) fn parse_bbox(s: &str) -> Option<[f32; 4]> {
    serde_json::from_str::<[f32; 4]>(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_renorm_blends_and_normalizes() {
        let a = pgvector::Vector::from(vec![1.0_f32, 0.0]);
        let b = pgvector::Vector::from(vec![0.0_f32, 1.0]);
        let m = weighted_renorm(Some(&a), 1, Some(&b), 1).unwrap();
        let s = m.as_slice();
        let norm = (s[0] * s[0] + s[1] * s[1]).sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        assert!((s[0] - s[1]).abs() < 1e-5); // equal weights -> symmetric
    }

    #[test]
    fn weighted_renorm_falls_back_to_present_side() {
        let a = pgvector::Vector::from(vec![3.0_f32, 4.0]);
        assert!(weighted_renorm(Some(&a), 1, None, 0).is_some());
        assert!(weighted_renorm(None, 0, None, 0).is_none());
    }

    #[test]
    fn bbox_parse() {
        assert_eq!(parse_bbox("[1,2,3,4]"), Some([1.0, 2.0, 3.0, 4.0]));
        assert_eq!(parse_bbox("nope"), None);
    }
}
