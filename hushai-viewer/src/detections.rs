//! Read-only detection queries over the vision tables (`person_segments`,
//! `scene_objects`), grouped by sampled-frame timestamp so the UI can overlay
//! bounding boxes synced to playback.
//!
//! Two tables, two coordinate-agnostic facts the overlay relies on:
//!   * `bbox` is stored as JSONB `[x, y, w, h]` in **original-frame pixels** — the same
//!     pixel grid the browser decodes (the viewer remux is `-c copy`, no rescale), so the
//!     client scales these against `video.videoWidth/videoHeight`. No frame dims are stored.
//!   * a detection's absolute instant = `start_unix_nanos + frame_offset_nanos` (the worker
//!     samples ~3 frames per ~2s segment). We group detections by that instant into "frames".
//!
//! Faces and objects live in separate tables with different indexes, so we run two
//! index-friendly window queries and merge in Rust (mirrors `timeline::windowed_segments`
//! for the query shape; runtime `sqlx::query_as`, no compile-time macros).
//!
//! NB: `bbox` is decoded via a `::text` cast + `serde_json` rather than `sqlx::types::Json`
//! because the viewer's `sqlx` does not enable the `json` feature; `serde_json` is already a dep.

use std::collections::BTreeMap;

use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ViewerResult;

/// All detections for a device overlapping `[from, to)`, grouped by sampled frame.
#[derive(Debug, Serialize)]
pub struct DetectionsResponse {
    pub device_id: String,
    pub from: i64,
    pub to: i64,
    /// Sampled frames, ascending by `t_unix_nanos`. Each holds every detection at that instant.
    pub frames: Vec<DetectionFrame>,
    /// True when a per-table row cap was hit and results were truncated (never silent).
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct DetectionFrame {
    /// Absolute timestamp of the sampled frame = `start_unix_nanos + frame_offset_nanos`.
    pub t_unix_nanos: i64,
    pub detections: Vec<Detection>,
}

#[derive(Debug, Serialize)]
pub struct Detection {
    /// "person" | "object".
    pub kind: &'static str,
    /// Object class for objects; the person's `display_name`, or `null` when unidentified/unnamed
    /// (the client renders `null` as "Unidentified").
    pub label: Option<String>,
    /// Stable identity for per-person coloring; `null` for objects and unidentified faces.
    pub person_id: Option<Uuid>,
    /// `[x, y, w, h]` in original-frame pixels (top-left + size).
    pub bbox: [f32; 4],
    pub det_score: Option<f32>,
}

/// Query both vision tables for the window and assemble the grouped response.
/// `max_rows` caps each table independently (a backstop; the UI fetches a small rolling window).
pub async fn windowed_detections(
    pool: &PgPool,
    device_id: &str,
    from: i64,
    to: i64,
    max_rows: i64,
) -> ViewerResult<DetectionsResponse> {
    let (mut rows, obj_trunc) = fetch_objects(pool, device_id, from, to, max_rows).await?;
    let (persons, ppl_trunc) = fetch_persons(pool, device_id, from, to, max_rows).await?;
    rows.extend(persons);

    let truncated = obj_trunc || ppl_trunc;
    if truncated {
        tracing::warn!(
            device_id,
            from,
            to,
            max_rows,
            "detections truncated at per-table cap"
        );
    }
    Ok(assemble(device_id, from, to, rows, truncated))
}

/// Objects: `scene_objects` via `_device_time_idx`. Excludes whole-frame CLIP rows
/// (`object_label = '__frame__'`, NULL bbox) which have no box to draw.
async fn fetch_objects(
    pool: &PgPool,
    device_id: &str,
    from: i64,
    to: i64,
    max_rows: i64,
) -> ViewerResult<(Vec<(i64, Detection)>, bool)> {
    // (object_label, bbox_json, det_score, t_ns)
    let raw: Vec<(Option<String>, String, Option<f32>, i64)> = sqlx::query_as(
        r#"
        SELECT
            object_label,
            bbox::text AS bbox_json,
            det_score,
            start_unix_nanos + COALESCE(frame_offset_nanos, 0) AS t_ns
        FROM scene_objects
        WHERE device_id = $1
          AND start_unix_nanos < $3
          AND end_unix_nanos   > $2
          AND object_label IS DISTINCT FROM '__frame__'
          AND bbox IS NOT NULL
        ORDER BY t_ns
        LIMIT $4
        "#,
    )
    .bind(device_id)
    .bind(from)
    .bind(to)
    .bind(max_rows + 1)
    .fetch_all(pool)
    .await?;

    let truncated = raw.len() as i64 > max_rows;
    let out = raw
        .into_iter()
        .take(max_rows as usize)
        .filter_map(|(label, bbox_json, det_score, t)| {
            let bbox = parse_bbox(&bbox_json)?;
            Some((
                t,
                Detection {
                    kind: "object",
                    label,
                    person_id: None,
                    bbox,
                    det_score,
                },
            ))
        })
        .collect();
    Ok((out, truncated))
}

/// Persons: `person_segments` LEFT JOIN `persons`. LEFT JOIN is mandatory — `person_id`
/// is nullable (unidentified faces); a matched-but-unnamed person also yields NULL name.
/// Both collapse to `label = None`, disambiguated client-side by `person_id` presence.
async fn fetch_persons(
    pool: &PgPool,
    device_id: &str,
    from: i64,
    to: i64,
    max_rows: i64,
) -> ViewerResult<(Vec<(i64, Detection)>, bool)> {
    // (display_name, person_id, bbox_json, det_score, t_ns)
    let raw: Vec<(Option<String>, Option<Uuid>, String, Option<f32>, i64)> = sqlx::query_as(
        r#"
        SELECT
            p.display_name,
            ps.person_id,
            ps.bbox::text AS bbox_json,
            ps.det_score,
            ps.start_unix_nanos + COALESCE(ps.frame_offset_nanos, 0) AS t_ns
        FROM person_segments ps
        LEFT JOIN persons p ON p.person_id = ps.person_id
        WHERE ps.device_id = $1
          AND ps.start_unix_nanos < $3
          AND ps.end_unix_nanos   > $2
          AND ps.bbox IS NOT NULL
        ORDER BY t_ns
        LIMIT $4
        "#,
    )
    .bind(device_id)
    .bind(from)
    .bind(to)
    .bind(max_rows + 1)
    .fetch_all(pool)
    .await?;

    let truncated = raw.len() as i64 > max_rows;
    let out = raw
        .into_iter()
        .take(max_rows as usize)
        .filter_map(|(display_name, person_id, bbox_json, det_score, t)| {
            let bbox = parse_bbox(&bbox_json)?;
            Some((
                t,
                Detection {
                    kind: "person",
                    label: display_name,
                    person_id,
                    bbox,
                    det_score,
                },
            ))
        })
        .collect();
    Ok((out, truncated))
}

/// Parse a JSONB-as-text bbox `[x,y,w,h]`. A malformed bbox is skipped (logged) rather than
/// failing the whole window. Accepts integer or float JSON numbers.
fn parse_bbox(s: &str) -> Option<[f32; 4]> {
    match serde_json::from_str::<[f32; 4]>(s) {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::warn!(bbox = %s, error = %e, "skipping detection with unparseable bbox");
            None
        }
    }
}

/// Group `(t_ns, Detection)` pairs into frames sorted ascending by timestamp.
fn assemble(
    device_id: &str,
    from: i64,
    to: i64,
    rows: Vec<(i64, Detection)>,
    truncated: bool,
) -> DetectionsResponse {
    let mut by_t: BTreeMap<i64, Vec<Detection>> = BTreeMap::new();
    for (t, det) in rows {
        by_t.entry(t).or_default().push(det);
    }
    let frames = by_t
        .into_iter()
        .map(|(t_unix_nanos, detections)| DetectionFrame {
            t_unix_nanos,
            detections,
        })
        .collect();
    DetectionsResponse {
        device_id: device_id.to_string(),
        from,
        to,
        frames,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(kind: &'static str, label: Option<&str>) -> Detection {
        Detection {
            kind,
            label: label.map(|s| s.to_string()),
            person_id: None,
            bbox: [1.0, 2.0, 3.0, 4.0],
            det_score: Some(0.9),
        }
    }

    #[test]
    fn groups_by_frame_timestamp_sorted() {
        let rows = vec![
            (200, det("object", Some("cup"))),
            (100, det("person", None)),
            (200, det("person", Some("Bob"))),
            (100, det("object", Some("chair"))),
        ];
        let r = assemble("dev", 0, 1000, rows, false);
        assert_eq!(r.frames.len(), 2);
        // ascending by timestamp
        assert_eq!(r.frames[0].t_unix_nanos, 100);
        assert_eq!(r.frames[1].t_unix_nanos, 200);
        // both detections land in their frame group
        assert_eq!(r.frames[0].detections.len(), 2);
        assert_eq!(r.frames[1].detections.len(), 2);
        assert!(!r.truncated);
    }

    #[test]
    fn unidentified_person_keeps_null_label() {
        let rows = vec![(50, det("person", None))];
        let r = assemble("dev", 0, 100, rows, false);
        assert_eq!(r.frames[0].detections[0].kind, "person");
        assert!(r.frames[0].detections[0].label.is_none());
    }

    #[test]
    fn bbox_parses_int_and_float_and_rejects_garbage() {
        assert_eq!(parse_bbox("[1, 2, 3, 4]"), Some([1.0, 2.0, 3.0, 4.0]));
        assert_eq!(parse_bbox("[1.5, 2.0, 3.0, 4.0]"), Some([1.5, 2.0, 3.0, 4.0]));
        assert_eq!(parse_bbox("not json"), None);
        // wrong arity is rejected (serde fixed-size array)
        assert_eq!(parse_bbox("[1, 2, 3]"), None);
    }
}
