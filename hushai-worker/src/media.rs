//! Resolve a segment's stored media and decode it to PCM for ASR.
//!
//! A segment's media bytes live in a content-addressed `file://` blob. Two
//! client conventions exist and are distinguished by the manifest `container`:
//!  - `fmp4` (e.g. `feed_segments.py`): the blob is a bare `moof`+`mdat` fragment
//!    and `codec_init_data` holds the fMP4 init (`ftyp`+`moov`); they must be
//!    concatenated to decode.
//!  - `mp4` (e.g. the native Android client): the blob is already a self-contained
//!    MP4 (own `ftyp`+`moov`+`mdat`); `codec_init_data` is raw SPS/PPS / ASC and
//!    must NOT be prepended (doing so corrupts the file → "moov atom not found").
//! We key off `container`, never the source (contract §7).

use std::path::PathBuf;

use anyhow::{Context, anyhow};
use sqlx::{AssertSqlSafe, PgPool};
use uuid::Uuid;

use crate::config::WorkerConfig;

/// The subset of a `segments` row the worker needs.
#[derive(Debug, Clone)]
pub struct SegmentRow {
    pub segment_id: Uuid,
    pub device_id: String,
    pub blob_uri: String,
    /// Manifest `container` ("fmp4" | "mp4" | …). Decides whether the blob is a
    /// bare fragment needing the init prepended, or already self-contained.
    pub container: String,
    pub codec_init_data: Option<Vec<u8>>,
    /// Device wall-clock at segment start (UTC ns) — the anchor for absolute timestamps.
    pub capture_start_unix_nanos: i64,
    /// Stream/session/order identity, used to build the speaker window from contiguous
    /// neighbors. `gap_before` marks a capture discontinuity right before this segment.
    pub session_id: Uuid,
    pub stream_id: String,
    pub sequence: i64,
    pub duration_nanos: i64,
    pub gap_before: bool,
}

const SEGMENT_COLS: &str = "segment_id, device_id, blob_uri, container, codec_init_data, \
     capture_start_unix_nanos, session_id, stream_id, sequence, duration_nanos, gap_before";

type SegmentTuple = (
    Uuid,
    String,
    String,
    String,
    Option<Vec<u8>>,
    i64,
    Uuid,
    String,
    i64,
    i64,
    bool,
);

fn row_from_tuple(t: SegmentTuple) -> SegmentRow {
    SegmentRow {
        segment_id: t.0,
        device_id: t.1,
        blob_uri: t.2,
        container: t.3,
        codec_init_data: t.4,
        capture_start_unix_nanos: t.5,
        session_id: t.6,
        stream_id: t.7,
        sequence: t.8,
        duration_nanos: t.9,
        gap_before: t.10,
    }
}

pub async fn load_segment(pool: &PgPool, segment_id: Uuid) -> anyhow::Result<SegmentRow> {
    let row: SegmentTuple = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT {SEGMENT_COLS} FROM segments WHERE segment_id = $1"
    )))
    .bind(segment_id)
    .fetch_one(pool)
    .await
    .with_context(|| format!("loading segment {segment_id}"))?;
    Ok(row_from_tuple(row))
}

/// Load the candidate look-back neighbors for `seg`'s speaker window: same session+stream,
/// AUDIO/MUXED, with `sequence <= seg.sequence`, newest first, capped at `max_segments`. The
/// pure [`select_window`] then decides how many to actually include (contiguity + budget).
pub async fn load_window_candidates(
    pool: &PgPool,
    seg: &SegmentRow,
    max_segments: usize,
) -> anyhow::Result<Vec<SegmentRow>> {
    let rows: Vec<SegmentTuple> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT {SEGMENT_COLS} FROM segments \
         WHERE session_id = $1 AND stream_id = $2 AND media_type IN (1, 3) AND sequence <= $3 \
         ORDER BY sequence DESC LIMIT $4"
    )))
    .bind(seg.session_id)
    .bind(&seg.stream_id)
    .bind(seg.sequence)
    .bind(max_segments.max(1) as i64)
    .fetch_all(pool)
    .await
    .with_context(|| format!("loading speaker-window candidates for {}", seg.segment_id))?;
    Ok(rows.into_iter().map(row_from_tuple).collect())
}

/// Pure window selection over candidates ordered NEWEST-first (index 0 = the current segment).
/// Walks back from the current segment, including a predecessor only while the run stays
/// contiguous (sequence exactly one lower) and uninterrupted (the younger segment has no
/// `gap_before`), until the accumulated RAW duration reaches `target_secs` or `max_segments`
/// is hit. Returns the chosen rows OLDEST-first (ready to concatenate), always including the
/// current segment. Separated from I/O so the contiguity rules are unit-testable.
pub fn select_window(
    candidates_newest_first: &[SegmentRow],
    target_secs: f64,
    max_segments: usize,
) -> Vec<SegmentRow> {
    let mut chosen: Vec<SegmentRow> = Vec::new();
    let Some(current) = candidates_newest_first.first() else {
        return chosen;
    };
    let mut acc_nanos: i64 = current.duration_nanos.max(0);
    chosen.push(current.clone());
    let target_nanos = (target_secs.max(0.0) * 1e9) as i64;

    for cand in candidates_newest_first.iter().skip(1) {
        if chosen.len() >= max_segments.max(1) || acc_nanos >= target_nanos {
            break;
        }
        let younger = chosen.last().unwrap();
        // Stop if the run breaks: a capture gap right before the younger segment, or a
        // non-consecutive sequence (a missing/foreign segment between them).
        if younger.gap_before || cand.sequence != younger.sequence - 1 {
            break;
        }
        acc_nanos += cand.duration_nanos.max(0);
        chosen.push(cand.clone());
    }

    chosen.reverse(); // oldest-first for concatenation
    chosen
}

/// `file:///abs/path` -> `/abs/path`. The backend always writes absolute `file://` URIs.
pub fn blob_path(blob_uri: &str) -> anyhow::Result<PathBuf> {
    let path = blob_uri
        .strip_prefix("file://")
        .ok_or_else(|| anyhow!("unsupported blob_uri scheme (expected file://): {blob_uri}"))?;
    Ok(PathBuf::from(path))
}

/// Reconstruct the decodable fragment and extract **16 kHz mono f32 PCM** via ffmpeg.
/// Returns an empty vec only if the audio decoded to nothing; ffmpeg failure is an error.
pub async fn extract_pcm(cfg: &WorkerConfig, seg: &SegmentRow) -> anyhow::Result<Vec<f32>> {
    let path = blob_path(&seg.blob_uri)?;
    let media = tokio::fs::read(&path)
        .await
        .with_context(|| format!("reading blob {}", path.display()))?;

    // Only bare fMP4 fragments need the init prepended. A self-contained container
    // (e.g. "mp4" from the Android client) already has its own ftyp+moov — prepending
    // raw SPS/PPS would corrupt it. Key off `container`, never the source (§7).
    let needs_init = seg.container.eq_ignore_ascii_case("fmp4");
    let init_len = if needs_init {
        seg.codec_init_data.as_ref().map_or(0, Vec::len)
    } else {
        0
    };
    let mut fragment = Vec::with_capacity(init_len + media.len());
    if needs_init {
        if let Some(init) = &seg.codec_init_data {
            fragment.extend_from_slice(init);
        }
    }
    fragment.extend_from_slice(&media);

    // ffmpeg needs a seekable input for mp4, so stage the fragment on disk. The temp name
    // must be unique PER CALL, not per segment: with speaker windowing, one worker decodes a
    // segment as a look-back neighbor while another decodes the SAME segment as its current
    // one — a segment_id-keyed temp would collide and one's cleanup would delete it under the
    // other ("No such file or directory"). A fresh uuid suffix isolates every decode.
    let tmp = std::env::temp_dir().join(format!(
        "hushai-asr-{}-{}.mp4",
        seg.segment_id,
        Uuid::now_v7()
    ));
    tokio::fs::write(&tmp, &fragment)
        .await
        .context("staging temp media for ffmpeg")?;
    let _cleanup = TempFile(tmp.clone());

    let output = tokio::process::Command::new(&cfg.ffmpeg_bin)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(&tmp)
        // -vn: drop video; decode audio to raw little-endian f32, mono, 16 kHz, to stdout.
        .args(["-vn", "-ac", "1", "-ar", "16000", "-f", "f32le", "-"])
        .output()
        .await
        .with_context(|| format!("running {} on {}", cfg.ffmpeg_bin, tmp.display()))?;

    if !output.status.success() {
        return Err(anyhow!(
            "ffmpeg exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(bytes_to_f32(&output.stdout))
}

fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Removes a staged temp file when dropped (even on early return/panic).
struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_file_scheme() {
        assert_eq!(
            blob_path("file:///data/blobs/ab/cd/ef").unwrap(),
            PathBuf::from("/data/blobs/ab/cd/ef")
        );
        assert!(blob_path("s3://bucket/key").is_err());
    }

    #[test]
    fn decodes_le_f32_pairs() {
        let bytes = [0u8, 0, 0, 0, 0, 0, 128, 63]; // 0.0, 1.0
        assert_eq!(bytes_to_f32(&bytes), vec![0.0, 1.0]);
    }

    fn seg(sequence: i64, dur_secs: f64, gap_before: bool) -> SegmentRow {
        SegmentRow {
            segment_id: Uuid::from_u128(sequence as u128 + 1),
            device_id: "d".into(),
            blob_uri: "file:///x".into(),
            container: "mp4".into(),
            codec_init_data: None,
            capture_start_unix_nanos: sequence * 2_000_000_000,
            session_id: Uuid::nil(),
            stream_id: "s".into(),
            sequence,
            duration_nanos: (dur_secs * 1e9) as i64,
            gap_before,
        }
    }

    fn seqs(rows: &[SegmentRow]) -> Vec<i64> {
        rows.iter().map(|r| r.sequence).collect()
    }

    #[test]
    fn window_accumulates_to_target_oldest_first() {
        // 2s clips, target 3s -> current (seq5) + one predecessor (seq4) reaches 4s >= 3s.
        let cands = vec![seg(5, 2.0, false), seg(4, 2.0, false), seg(3, 2.0, false)];
        assert_eq!(seqs(&select_window(&cands, 3.0, 5)), vec![4, 5]);
    }

    #[test]
    fn window_stops_at_gap_before() {
        // current seg5 has gap_before -> it begins a fresh run; never reach the predecessor.
        let cands = vec![seg(5, 0.5, true), seg(4, 0.5, false), seg(3, 0.5, false)];
        assert_eq!(seqs(&select_window(&cands, 3.0, 5)), vec![5]);
        // gap before seg4 -> include seg5+seg4 then stop (don't cross into seg3).
        let cands2 = vec![seg(5, 0.5, false), seg(4, 0.5, true), seg(3, 0.5, false)];
        assert_eq!(seqs(&select_window(&cands2, 3.0, 5)), vec![4, 5]);
    }

    #[test]
    fn window_stops_at_sequence_gap() {
        // seq jumps 5 -> 3 (missing 4): don't bridge.
        let cands = vec![seg(5, 0.5, false), seg(3, 0.5, false), seg(2, 0.5, false)];
        assert_eq!(seqs(&select_window(&cands, 3.0, 5)), vec![5]);
    }

    #[test]
    fn window_respects_max_segments() {
        let cands = vec![
            seg(5, 0.2, false),
            seg(4, 0.2, false),
            seg(3, 0.2, false),
            seg(2, 0.2, false),
        ];
        assert_eq!(seqs(&select_window(&cands, 100.0, 2)), vec![4, 5]);
    }

    #[test]
    fn window_single_candidate_is_just_current() {
        assert_eq!(seqs(&select_window(&[seg(5, 0.3, false)], 3.0, 5)), vec![5]);
        assert!(select_window(&[], 3.0, 5).is_empty());
    }
}
