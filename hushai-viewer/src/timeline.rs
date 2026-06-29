//! Read-only timeline queries over the `segments` table, plus the coalescing that
//! turns raw ~2s segment rows into the spans/coverage the scrub bar draws.
//!
//! Ordering for stitching is by `(stream_id, session_id, sequence)` — never wall
//! clock, which is skew-prone (same rule `export_capture.sh` uses). Wall clock
//! (`capture_start_unix_nanos`) is used only for absolute placement (PDT) and for
//! the window filter.

use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ViewerResult;

/// Map the proto `MediaType` int to a stream "kind" the UI understands.
pub fn kind_of(media_type: i32) -> &'static str {
    match media_type {
        1 => "audio",
        2 => "video",
        3 => "muxed",
        _ => "other",
    }
}

// ---------------------------------------------------------------------------
// Device list
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct DeviceSummary {
    pub device_id: String,
    /// Operator-assigned friendly name (set via the management surface); `null` until renamed.
    pub display_name: Option<String>,
    pub source_kind: String,
    pub first_capture_unix_nanos: Option<i64>,
    pub last_capture_unix_nanos: Option<i64>,
    pub segment_count: i64,
    pub session_count: i64,
    pub has_video: bool,
    pub has_audio: bool,
    pub has_muxed: bool,
}

pub async fn list_devices(pool: &PgPool) -> ViewerResult<Vec<DeviceSummary>> {
    // LEFT JOIN so a device with no segments still appears (counts 0, bounds NULL).
    let rows: Vec<(
        String,
        Option<String>,
        String,
        Option<i64>,
        Option<i64>,
        i64,
        i64,
        Option<bool>,
        Option<bool>,
        Option<bool>,
    )> = sqlx::query_as(
        r#"
        SELECT
            d.device_id,
            d.display_name,
            d.source_kind,
            min(s.capture_start_unix_nanos)                       AS first_ns,
            max(s.capture_start_unix_nanos + s.duration_nanos)    AS last_ns,
            count(s.segment_id)                                   AS segment_count,
            count(DISTINCT s.session_id)                          AS session_count,
            bool_or(s.media_type = 2)                             AS has_video,
            bool_or(s.media_type = 1)                             AS has_audio,
            bool_or(s.media_type = 3)                             AS has_muxed
        FROM devices d
        LEFT JOIN segments s USING (device_id)
        GROUP BY d.device_id, d.display_name, d.source_kind
        ORDER BY last_ns DESC NULLS LAST
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| DeviceSummary {
            device_id: r.0,
            display_name: r.1,
            source_kind: r.2,
            first_capture_unix_nanos: r.3,
            last_capture_unix_nanos: r.4,
            segment_count: r.5,
            session_count: r.6,
            has_video: r.7.unwrap_or(false),
            has_audio: r.8.unwrap_or(false),
            has_muxed: r.9.unwrap_or(false),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Windowed segment query (shared by /api/timeline and the playlist builders)
// ---------------------------------------------------------------------------

/// One segment row, in stitch order. Bytes are referenced by content hash.
#[derive(Debug, Clone)]
pub struct SegmentRow {
    pub stream_id: String,
    pub session_id: Uuid,
    pub sequence: i64,
    pub media_type: i32,
    pub container: String,
    pub sha_hex: String,
    pub capture_start_unix_nanos: i64,
    pub duration_nanos: i64,
    pub gap_before: bool,
}

/// All segments for a device overlapping `[from, to)`, ordered for emission.
/// Overlap test: `start < to AND start + dur > from` (a segment straddling either
/// edge is included).
pub async fn windowed_segments(
    pool: &PgPool,
    device_id: &str,
    from: i64,
    to: i64,
) -> ViewerResult<Vec<SegmentRow>> {
    let rows: Vec<(String, Uuid, i64, i32, String, String, i64, i64, bool)> = sqlx::query_as(
        r#"
        SELECT
            stream_id,
            session_id,
            sequence,
            media_type,
            container,
            encode(content_sha256, 'hex') AS sha_hex,
            capture_start_unix_nanos,
            duration_nanos,
            gap_before
        FROM segments
        WHERE device_id = $1
          AND capture_start_unix_nanos < $3
          AND capture_start_unix_nanos + duration_nanos > $2
        ORDER BY stream_id, session_id, sequence
        "#,
    )
    .bind(device_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| SegmentRow {
            stream_id: r.0,
            session_id: r.1,
            sequence: r.2,
            media_type: r.3,
            container: r.4,
            sha_hex: r.5,
            capture_start_unix_nanos: r.6,
            duration_nanos: r.7,
            gap_before: r.8,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Coalescing -> timeline structure for the scrub bar
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Span {
    pub session_id: String,
    pub stream_id: String,
    pub kind: String,
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
    pub segment_count: i64,
}

#[derive(Debug, Serialize)]
pub struct Interval {
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
}

#[derive(Debug, Serialize)]
pub struct TimelineResponse {
    pub device_id: String,
    pub from: i64,
    pub to: i64,
    pub spans: Vec<Span>,
    /// Union of all spans across every kind — what the bar fills. Gaps are the complement.
    pub coverage: Vec<Interval>,
    /// Wall-clock start of each distinct session in the window (hard discontinuities).
    pub session_boundaries: Vec<i64>,
}

/// Coalesce ordered rows into spans. A new span begins on a stream/session change,
/// a `gap_before` flag, or a non-monotonic wall-clock step (skew/overlap).
pub fn build_timeline(device_id: &str, from: i64, to: i64, rows: &[SegmentRow]) -> TimelineResponse {
    let mut spans: Vec<Span> = Vec::new();
    let mut session_first: std::collections::BTreeMap<Uuid, i64> = std::collections::BTreeMap::new();

    for row in rows {
        let end = row.capture_start_unix_nanos.saturating_add(row.duration_nanos);
        session_first
            .entry(row.session_id)
            .and_modify(|v| *v = (*v).min(row.capture_start_unix_nanos))
            .or_insert(row.capture_start_unix_nanos);

        let continues = match spans.last() {
            Some(prev) => {
                prev.stream_id == row.stream_id
                    && prev.session_id == row.session_id.to_string()
                    && !row.gap_before
                    // monotonic, and roughly adjacent (not a silent jump forward)
                    && row.capture_start_unix_nanos >= prev.end_unix_nanos.saturating_sub(row.duration_nanos)
                    && row.capture_start_unix_nanos <= prev.end_unix_nanos.saturating_add(row.duration_nanos)
            }
            None => false,
        };

        if continues {
            let last = spans.last_mut().unwrap();
            last.end_unix_nanos = last.end_unix_nanos.max(end);
            last.segment_count += 1;
        } else {
            spans.push(Span {
                session_id: row.session_id.to_string(),
                stream_id: row.stream_id.clone(),
                kind: kind_of(row.media_type).to_string(),
                start_unix_nanos: row.capture_start_unix_nanos,
                end_unix_nanos: end,
                segment_count: 1,
            });
        }
    }

    let coverage = merge_intervals(spans.iter().map(|s| (s.start_unix_nanos, s.end_unix_nanos)));
    let session_boundaries = session_first.values().copied().collect();

    TimelineResponse {
        device_id: device_id.to_string(),
        from,
        to,
        spans,
        coverage,
        session_boundaries,
    }
}

/// Merge a set of (start,end) intervals into a sorted, non-overlapping union.
fn merge_intervals<I: Iterator<Item = (i64, i64)>>(it: I) -> Vec<Interval> {
    let mut ivals: Vec<(i64, i64)> = it.filter(|(s, e)| e > s).collect();
    ivals.sort_unstable();
    let mut out: Vec<Interval> = Vec::new();
    for (s, e) in ivals {
        match out.last_mut() {
            Some(last) if s <= last.end_unix_nanos => {
                last.end_unix_nanos = last.end_unix_nanos.max(e);
            }
            _ => out.push(Interval {
                start_unix_nanos: s,
                end_unix_nanos: e,
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(stream: &str, session: Uuid, seq: i64, mt: i32, start: i64, dur: i64, gap: bool) -> SegmentRow {
        SegmentRow {
            stream_id: stream.into(),
            session_id: session,
            sequence: seq,
            media_type: mt,
            container: "mp4".into(),
            sha_hex: "00".repeat(32),
            capture_start_unix_nanos: start,
            duration_nanos: dur,
            gap_before: gap,
        }
    }

    #[test]
    fn coalesces_contiguous_and_splits_on_gap() {
        let s = Uuid::now_v7();
        let d = 2_000_000_000;
        let rows = vec![
            row("cam0-video", s, 0, 2, 0, d, false),
            row("cam0-video", s, 1, 2, d, d, false),
            // gap_before -> new span
            row("cam0-video", s, 2, 2, 10 * d, d, true),
        ];
        let t = build_timeline("dev", 0, 100 * d, &rows);
        assert_eq!(t.spans.len(), 2);
        assert_eq!(t.spans[0].segment_count, 2);
        assert_eq!(t.spans[0].end_unix_nanos, 2 * d);
        assert_eq!(t.coverage.len(), 2);
        assert_eq!(t.session_boundaries, vec![0]);
    }

    #[test]
    fn merges_overlapping_coverage_across_kinds() {
        let s = Uuid::now_v7();
        let d = 2_000_000_000;
        // audio and video overlap in wall-clock -> single coverage interval
        let rows = vec![
            row("cam0-video", s, 0, 2, 0, 3 * d, false),
            row("cam0-audio", s, 0, 1, d, d, false),
        ];
        let t = build_timeline("dev", 0, 100 * d, &rows);
        assert_eq!(t.spans.len(), 2);
        assert_eq!(t.coverage.len(), 1);
        assert_eq!(t.coverage[0].start_unix_nanos, 0);
        assert_eq!(t.coverage[0].end_unix_nanos, 3 * d);
    }
}
