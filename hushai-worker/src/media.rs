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
use sqlx::PgPool;
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
}

pub async fn load_segment(pool: &PgPool, segment_id: Uuid) -> anyhow::Result<SegmentRow> {
    let row: (String, String, String, Option<Vec<u8>>, i64) = sqlx::query_as(
        r#"
        SELECT device_id, blob_uri, container, codec_init_data, capture_start_unix_nanos
        FROM segments WHERE segment_id = $1
        "#,
    )
    .bind(segment_id)
    .fetch_one(pool)
    .await
    .with_context(|| format!("loading segment {segment_id}"))?;

    Ok(SegmentRow {
        segment_id,
        device_id: row.0,
        blob_uri: row.1,
        container: row.2,
        codec_init_data: row.3,
        capture_start_unix_nanos: row.4,
    })
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

    // ffmpeg needs a seekable input for mp4, so stage the fragment on disk.
    let tmp = std::env::temp_dir().join(format!("hushai-asr-{}.mp4", seg.segment_id));
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
}
