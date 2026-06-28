//! Read-only **AI processing-status** coverage over the two per-segment work queues
//! (`segment_transcription_status` for the AUDIO pipeline — ASR + speaker-ID + sentiment;
//! `segment_vision_status` for the VISION pipeline — face + object detection), LEFT-JOINed
//! to `segments` for their wall-clock ranges.
//!
//! This is distinct from `timeline.rs` (which stretches *exist* on disk) and `detections.rs`
//! (what was *detected*): it answers "how far has the AI pipeline gotten over this stretch,
//! and what did it produce." The worker processes oldest-first, so the live edge reads
//! `pending`/`processing` and older footage `done` — exactly the wave the scrub bar shows.
//!
//! Per lane we run an index-friendly status query (`segments` LEFT JOIN the status table,
//! gated by `media_type` so a lane's *absence* means "not applicable here", not "pending")
//! and one or two output-count queries grouped by `segment_id`, merge them in Rust, then
//! coalesce contiguous same-status segments into intervals — mirroring `timeline::build_timeline`.
//! Runtime `sqlx::query_as` (no compile-time macros), like `detections.rs`.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ViewerResult;

// ---------------------------------------------------------------------------
// Response shape
// ---------------------------------------------------------------------------

/// One coalesced run of contiguous AUDIO segments sharing a pipeline status.
#[derive(Debug, Serialize)]
pub struct AudioInterval {
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
    /// `pending` | `processing` | `done` | `error`.
    pub status: String,
    /// Representative worker error; present only when `status == "error"`.
    pub last_error: Option<String>,
    /// Transcript sentences produced over this run (0 unless `done` with speech).
    pub sentences: i64,
    /// Distinct attributed speakers over this run.
    pub speakers: i64,
}

/// One coalesced run of contiguous VISION segments sharing a pipeline status.
#[derive(Debug, Serialize)]
pub struct VisionInterval {
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
    pub status: String,
    pub last_error: Option<String>,
    /// Face detections over this run.
    pub faces: i64,
    /// Object detections over this run (whole-frame `__frame__` rows excluded).
    pub objects: i64,
}

/// Segment-granularity stage tally for a lane (counts *segments*, not intervals).
#[derive(Debug, Default, Serialize)]
pub struct LaneSummary {
    pub pending: i64,
    pub processing: i64,
    pub done: i64,
    pub error: i64,
}

impl LaneSummary {
    fn tally(&mut self, status: &str) {
        match status {
            "pending" => self.pending += 1,
            "processing" => self.processing += 1,
            "done" => self.done += 1,
            "error" => self.error += 1,
            _ => {}
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ProcessingResponse {
    pub device_id: String,
    pub from: i64,
    pub to: i64,
    pub audio: Vec<AudioInterval>,
    pub audio_summary: LaneSummary,
    pub vision: Vec<VisionInterval>,
    pub vision_summary: LaneSummary,
    /// True when a per-lane segment cap was hit and results were truncated (never silent).
    pub truncated: bool,
}

/// Query both work queues + their output tables for the window and assemble the response.
/// `max_rows` caps each lane's status query independently (a backstop; the UI fetches the
/// visible window).
pub async fn windowed_processing(
    pool: &PgPool,
    device_id: &str,
    from: i64,
    to: i64,
    max_rows: i64,
) -> ViewerResult<ProcessingResponse> {
    let (audio_rows, audio_trunc) = fetch_audio(pool, device_id, from, to, max_rows).await?;
    let (vision_rows, vision_trunc) = fetch_vision(pool, device_id, from, to, max_rows).await?;

    let (audio_ivals, audio_summary) = coalesce(&audio_rows, |acc: &mut AudioAcc, c| {
        acc.sentences += c.sentences;
        acc.speakers.extend(c.speakers.iter().cloned());
    });
    let (vision_ivals, vision_summary) = coalesce(&vision_rows, |acc: &mut VisionAcc, c| {
        acc.faces += c.faces;
        acc.objects += c.objects;
    });

    let truncated = audio_trunc || vision_trunc;
    if truncated {
        tracing::warn!(device_id, from, to, max_rows, "processing status truncated at per-lane cap");
    }

    Ok(ProcessingResponse {
        device_id: device_id.to_string(),
        from,
        to,
        audio: audio_ivals
            .into_iter()
            .map(|iv| AudioInterval {
                start_unix_nanos: iv.start_ns,
                end_unix_nanos: iv.end_ns,
                status: iv.status,
                last_error: iv.last_error,
                sentences: iv.acc.sentences,
                speakers: iv.acc.speakers.len() as i64,
            })
            .collect(),
        audio_summary,
        vision: vision_ivals
            .into_iter()
            .map(|iv| VisionInterval {
                start_unix_nanos: iv.start_ns,
                end_unix_nanos: iv.end_ns,
                status: iv.status,
                last_error: iv.last_error,
                faces: iv.acc.faces,
                objects: iv.acc.objects,
            })
            .collect(),
        vision_summary,
        truncated,
    })
}

// ---------------------------------------------------------------------------
// Per-segment rows + counts
// ---------------------------------------------------------------------------

/// One segment's status + output counts, in stitch order, before coalescing.
struct LaneRow<C> {
    start_ns: i64,
    end_ns: i64,
    gap_before: bool,
    stream_id: String,
    session_id: Uuid,
    status: String,
    last_error: Option<String>,
    counts: C,
}

#[derive(Default)]
struct AudioCounts {
    sentences: i64,
    speakers: Vec<String>,
}

#[derive(Default)]
struct VisionCounts {
    faces: i64,
    objects: i64,
}

#[derive(Default)]
struct AudioAcc {
    sentences: i64,
    speakers: HashSet<String>,
}

#[derive(Default)]
struct VisionAcc {
    faces: i64,
    objects: i64,
}

/// One status row as read from the DB before counts are merged in.
type StatusRow = (Uuid, i64, i64, bool, String, Uuid, String, Option<String>);

/// AUDIO lane: `segments` LEFT JOIN `segment_transcription_status`, media_type AUDIO(1)/MUXED(3).
async fn fetch_audio(
    pool: &PgPool,
    device_id: &str,
    from: i64,
    to: i64,
    max_rows: i64,
) -> ViewerResult<(Vec<LaneRow<AudioCounts>>, bool)> {
    let status: Vec<StatusRow> = sqlx::query_as(
        r#"
        SELECT
            s.segment_id,
            s.capture_start_unix_nanos                       AS start_ns,
            s.capture_start_unix_nanos + s.duration_nanos    AS end_ns,
            s.gap_before,
            s.stream_id,
            s.session_id,
            COALESCE(t.status, 'pending')                    AS status,
            t.last_error
        FROM segments s
        LEFT JOIN segment_transcription_status t USING (segment_id)
        WHERE s.device_id = $1
          AND s.media_type IN (1, 3)
          AND s.capture_start_unix_nanos < $3
          AND s.capture_start_unix_nanos + s.duration_nanos > $2
        ORDER BY s.stream_id, s.session_id, s.sequence
        LIMIT $4
        "#,
    )
    .bind(device_id)
    .bind(from)
    .bind(to)
    .bind(max_rows + 1)
    .fetch_all(pool)
    .await?;
    let truncated = status.len() as i64 > max_rows;

    // Per-segment output: sentence count + the (tiny) distinct speaker-id set, so a
    // coalesced run's distinct-speaker count is correct (a plain sum would double-count
    // a speaker spanning adjacent segments). `speaker_id` is the denormalized text UUID.
    let counts: Vec<(Uuid, i64, Vec<String>)> = sqlx::query_as(
        r#"
        SELECT
            segment_id,
            count(*)                                                             AS sentences,
            COALESCE(array_agg(DISTINCT speaker_id) FILTER (WHERE speaker_id IS NOT NULL),
                     ARRAY[]::text[])                                            AS speakers
        FROM transcript_sentences
        WHERE device_id = $1
          AND start_unix_nanos < $3
          AND end_unix_nanos   > $2
        GROUP BY segment_id
        "#,
    )
    .bind(device_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    let mut count_by: HashMap<Uuid, AudioCounts> = HashMap::with_capacity(counts.len());
    for (seg, sentences, speakers) in counts {
        count_by.insert(seg, AudioCounts { sentences, speakers });
    }

    let rows = status
        .into_iter()
        .take(max_rows as usize)
        .map(|(seg, start_ns, end_ns, gap_before, stream_id, session_id, st, last_error)| LaneRow {
            start_ns,
            end_ns,
            gap_before,
            stream_id,
            session_id,
            status: st,
            last_error,
            counts: count_by.remove(&seg).unwrap_or_default(),
        })
        .collect();
    Ok((rows, truncated))
}

/// VISION lane: `segments` LEFT JOIN `segment_vision_status`, media_type VIDEO(2)/MUXED(3).
async fn fetch_vision(
    pool: &PgPool,
    device_id: &str,
    from: i64,
    to: i64,
    max_rows: i64,
) -> ViewerResult<(Vec<LaneRow<VisionCounts>>, bool)> {
    let status: Vec<StatusRow> = sqlx::query_as(
        r#"
        SELECT
            s.segment_id,
            s.capture_start_unix_nanos                       AS start_ns,
            s.capture_start_unix_nanos + s.duration_nanos    AS end_ns,
            s.gap_before,
            s.stream_id,
            s.session_id,
            COALESCE(v.status, 'pending')                    AS status,
            v.last_error
        FROM segments s
        LEFT JOIN segment_vision_status v USING (segment_id)
        WHERE s.device_id = $1
          AND s.media_type IN (2, 3)
          AND s.capture_start_unix_nanos < $3
          AND s.capture_start_unix_nanos + s.duration_nanos > $2
        ORDER BY s.stream_id, s.session_id, s.sequence
        LIMIT $4
        "#,
    )
    .bind(device_id)
    .bind(from)
    .bind(to)
    .bind(max_rows + 1)
    .fetch_all(pool)
    .await?;
    let truncated = status.len() as i64 > max_rows;

    let faces: Vec<(Uuid, i64)> = sqlx::query_as(
        r#"
        SELECT segment_id, count(*) AS faces
        FROM person_segments
        WHERE device_id = $1
          AND start_unix_nanos < $3
          AND end_unix_nanos   > $2
        GROUP BY segment_id
        "#,
    )
    .bind(device_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;

    // Exclude whole-frame CLIP rows (`__frame__`, NULL bbox) — same filter as detections.rs.
    let objects: Vec<(Uuid, i64)> = sqlx::query_as(
        r#"
        SELECT segment_id, count(*) AS objects
        FROM scene_objects
        WHERE device_id = $1
          AND start_unix_nanos < $3
          AND end_unix_nanos   > $2
          AND object_label IS DISTINCT FROM '__frame__'
          AND bbox IS NOT NULL
        GROUP BY segment_id
        "#,
    )
    .bind(device_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;

    let mut count_by: HashMap<Uuid, VisionCounts> = HashMap::new();
    for (seg, n) in faces {
        count_by.entry(seg).or_default().faces = n;
    }
    for (seg, n) in objects {
        count_by.entry(seg).or_default().objects = n;
    }

    let rows = status
        .into_iter()
        .take(max_rows as usize)
        .map(|(seg, start_ns, end_ns, gap_before, stream_id, session_id, st, last_error)| LaneRow {
            start_ns,
            end_ns,
            gap_before,
            stream_id,
            session_id,
            status: st,
            last_error,
            counts: count_by.remove(&seg).unwrap_or_default(),
        })
        .collect();
    Ok((rows, truncated))
}

// ---------------------------------------------------------------------------
// Coalescing -> status intervals
// ---------------------------------------------------------------------------

/// A coalesced run carrying its folded output accumulator `A`.
struct RawInterval<A> {
    start_ns: i64,
    end_ns: i64,
    status: String,
    last_error: Option<String>,
    acc: A,
}

/// Fold ordered per-segment rows into status intervals, summing output via `fold`, and tally
/// the segment-granularity `LaneSummary`. A new interval begins on a status/stream/session
/// change, a `gap_before` flag, or a non-adjacent wall-clock step — the `build_timeline`
/// rule (timeline.rs) plus the extra `status` term so a recovered region shows green-after-red.
fn coalesce<C, A: Default>(
    rows: &[LaneRow<C>],
    fold: impl Fn(&mut A, &C),
) -> (Vec<RawInterval<A>>, LaneSummary) {
    let mut out: Vec<RawInterval<A>> = Vec::new();
    let mut summary = LaneSummary::default();
    // (stream_id, session_id) context of the currently-open interval.
    let mut cur: Option<(String, Uuid, RawInterval<A>)> = None;

    for row in rows {
        summary.tally(&row.status);
        let dur = row.end_ns.saturating_sub(row.start_ns);
        let continues = match &cur {
            Some((stream, session, iv)) => {
                iv.status == row.status
                    && *stream == row.stream_id
                    && *session == row.session_id
                    && !row.gap_before
                    // monotonic + roughly adjacent (not a silent jump forward)
                    && row.start_ns >= iv.end_ns.saturating_sub(dur)
                    && row.start_ns <= iv.end_ns.saturating_add(dur)
            }
            None => false,
        };

        if continues {
            let (_, _, iv) = cur.as_mut().unwrap();
            iv.end_ns = iv.end_ns.max(row.end_ns);
            fold(&mut iv.acc, &row.counts);
            if row.status == "error" && row.last_error.is_some() {
                iv.last_error = row.last_error.clone();
            }
        } else {
            if let Some((_, _, iv)) = cur.take() {
                out.push(iv);
            }
            let mut acc = A::default();
            fold(&mut acc, &row.counts);
            cur = Some((
                row.stream_id.clone(),
                row.session_id,
                RawInterval {
                    start_ns: row.start_ns,
                    end_ns: row.end_ns,
                    status: row.status.clone(),
                    last_error: if row.status == "error" {
                        row.last_error.clone()
                    } else {
                        None
                    },
                    acc,
                },
            ));
        }
    }
    if let Some((_, _, iv)) = cur.take() {
        out.push(iv);
    }
    (out, summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arow(seq_start: i64, dur: i64, gap: bool, status: &str, sentences: i64, speakers: &[&str]) -> LaneRow<AudioCounts> {
        LaneRow {
            start_ns: seq_start,
            end_ns: seq_start + dur,
            gap_before: gap,
            stream_id: "cam0-audio".into(),
            session_id: Uuid::nil(),
            status: status.into(),
            last_error: None,
            counts: AudioCounts {
                sentences,
                speakers: speakers.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    fn fold_audio(acc: &mut AudioAcc, c: &AudioCounts) {
        acc.sentences += c.sentences;
        acc.speakers.extend(c.speakers.iter().cloned());
    }

    #[test]
    fn coalesces_same_status_and_sums_output() {
        let d = 2_000_000_000;
        let rows = vec![
            arow(0, d, false, "done", 3, &["alice"]),
            arow(d, d, false, "done", 2, &["alice", "bob"]),
        ];
        let (ivals, summary) = coalesce(&rows, fold_audio);
        assert_eq!(ivals.len(), 1);
        assert_eq!(ivals[0].start_ns, 0);
        assert_eq!(ivals[0].end_ns, 2 * d);
        assert_eq!(ivals[0].acc.sentences, 5);
        // distinct union, NOT 3 (alice counted once across the two segments)
        assert_eq!(ivals[0].acc.speakers.len(), 2);
        assert_eq!(summary.done, 2);
    }

    #[test]
    fn splits_on_status_boundary() {
        let d = 2_000_000_000;
        let rows = vec![
            arow(0, d, false, "done", 1, &[]),
            arow(d, d, false, "processing", 0, &[]),
            arow(2 * d, d, false, "pending", 0, &[]),
        ];
        let (ivals, summary) = coalesce(&rows, fold_audio);
        assert_eq!(ivals.len(), 3);
        assert_eq!(ivals[0].status, "done");
        assert_eq!(ivals[1].status, "processing");
        assert_eq!(ivals[2].status, "pending");
        assert_eq!(summary.done, 1);
        assert_eq!(summary.processing, 1);
        assert_eq!(summary.pending, 1);
    }

    #[test]
    fn splits_on_gap_before_even_when_status_matches() {
        let d = 2_000_000_000;
        let rows = vec![
            arow(0, d, false, "done", 1, &[]),
            arow(10 * d, d, true, "done", 1, &[]), // gap_before -> new interval
        ];
        let (ivals, _) = coalesce(&rows, fold_audio);
        assert_eq!(ivals.len(), 2);
    }

    #[test]
    fn error_interval_carries_last_error() {
        let d = 2_000_000_000;
        let mut r = arow(0, d, false, "error", 0, &[]);
        r.last_error = Some("whisper OOM".into());
        let (ivals, summary) = coalesce(&[r], fold_audio);
        assert_eq!(ivals.len(), 1);
        assert_eq!(ivals[0].status, "error");
        assert_eq!(ivals[0].last_error.as_deref(), Some("whisper OOM"));
        assert_eq!(summary.error, 1);
    }
}
