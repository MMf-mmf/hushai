//! Read-only **sentiment coverage** for the timeline's mood ribbon: which stretches of a
//! device's audio were positive / neutral / negative, coalesced into contiguous runs.
//!
//! `sentiment` is SEGMENT-level, denormalized onto every sentence of the segment (see the
//! header of `hushai-rag/src/analytics.rs`) — so we collapse `transcript_sentences` to
//! segment grain FIRST (`GROUP BY segment_id`), then join `segments` for wall-clock
//! ranges. Mirrors `processing.rs`: windowed, runtime `sqlx::query_as`, capped with an
//! explicit `truncated` flag, contiguous same-sentiment segments coalesced in Rust.

use serde::Serialize;
use sqlx::PgPool;

use crate::error::ViewerResult;

/// Two segments this close (ns) count as one contiguous run (matches the ~2s cadence
/// with a little ingest jitter).
const JOIN_GAP_NS: i64 = 1_500_000_000;

/// One coalesced run of contiguous segments sharing a sentiment.
#[derive(Debug, Serialize)]
pub struct SentimentInterval {
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
    /// `positive` | `neutral` | `negative` (whatever the worker wrote).
    pub sentiment: String,
    /// Segments contributing to this run.
    pub segments: i64,
}

#[derive(Debug, Serialize)]
pub struct SentimentResponse {
    pub device_id: String,
    pub from: i64,
    pub to: i64,
    pub intervals: Vec<SentimentInterval>,
    /// True when the row cap was hit and the window is incomplete (never silent).
    pub truncated: bool,
}

pub async fn windowed_sentiment(
    pool: &PgPool,
    device_id: &str,
    from: i64,
    to: i64,
    max_rows: i64,
) -> ViewerResult<SentimentResponse> {
    // Collapse to segment grain first; MIN(sentiment) is arbitrary-but-stable across the
    // identical denormalized copies (the same trick analytics.rs uses).
    let mut rows: Vec<(i64, i64, String)> = sqlx::query_as(
        r#"
        SELECT seg.capture_start_unix_nanos,
               seg.capture_start_unix_nanos + seg.duration_nanos,
               t.sentiment
        FROM (
            SELECT segment_id, MIN(sentiment) AS sentiment
            FROM transcript_sentences
            WHERE sentiment IS NOT NULL
              AND start_unix_nanos < $3
              AND end_unix_nanos > $2
            GROUP BY segment_id
        ) t
        JOIN segments seg ON seg.segment_id = t.segment_id
        WHERE seg.device_id = $1
        ORDER BY seg.capture_start_unix_nanos
        LIMIT $4
        "#,
    )
    .bind(device_id)
    .bind(from)
    .bind(to)
    .bind(max_rows + 1)
    .fetch_all(pool)
    .await?;

    let truncated = rows.len() as i64 > max_rows;
    if truncated {
        rows.truncate(max_rows as usize);
        tracing::warn!(device_id, from, to, max_rows, "sentiment window truncated at cap");
    }

    Ok(SentimentResponse {
        device_id: device_id.to_string(),
        from,
        to,
        intervals: coalesce(&rows),
        truncated,
    })
}

fn coalesce(rows: &[(i64, i64, String)]) -> Vec<SentimentInterval> {
    let mut out: Vec<SentimentInterval> = Vec::new();
    for (start, end, sentiment) in rows {
        match out.last_mut() {
            Some(prev)
                if prev.sentiment == *sentiment && start - prev.end_unix_nanos <= JOIN_GAP_NS =>
            {
                prev.end_unix_nanos = (*end).max(prev.end_unix_nanos);
                prev.segments += 1;
            }
            _ => out.push(SentimentInterval {
                start_unix_nanos: *start,
                end_unix_nanos: *end,
                sentiment: sentiment.clone(),
                segments: 1,
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000; // 1s in ns

    #[test]
    fn coalesces_contiguous_same_sentiment_and_splits_on_change_or_gap() {
        let rows = vec![
            (0, 2 * S, "neutral".to_string()),
            (2 * S, 4 * S, "neutral".to_string()), // contiguous, same -> merge
            (4 * S, 6 * S, "positive".to_string()), // sentiment change -> split
            (20 * S, 22 * S, "positive".to_string()), // >1.5s gap -> split
        ];
        let iv = coalesce(&rows);
        assert_eq!(iv.len(), 3);
        assert_eq!((iv[0].start_unix_nanos, iv[0].end_unix_nanos), (0, 4 * S));
        assert_eq!(iv[0].segments, 2);
        assert_eq!(iv[1].sentiment, "positive");
        assert_eq!(iv[2].start_unix_nanos, 20 * S);
    }
}
