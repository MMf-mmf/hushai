//! Sample a few RGB frames from a stored VIDEO/MUXED segment via ffmpeg.
//!
//! Mirrors `media::extract_pcm`'s blob fetch + container-keyed fragment reconstruction (prepend
//! `codec_init_data` only for bare `fmp4` fragments; self-contained `mp4` is used as-is), but
//! decodes frames instead of PCM. A single sharp frame is enough for a face embedding, so we
//! sample a small number of frames per ~2s segment rather than every frame.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use image::RgbImage;

use crate::config::WorkerConfig;
use crate::media::{SegmentRow, blob_path};

/// One decoded frame plus its approximate offset from the segment start (derived from the sample
/// rate — good enough to seek + crop the same frame later for the sample-face endpoint).
pub struct DecodedFrame {
    pub offset_nanos: i64,
    pub image: RgbImage,
}

/// Decode up to `max_frames` evenly-spaced RGB frames from the segment's video track. Returns an
/// empty vec when the segment has no decodable video (expected for audio-only blobs — NOT an
/// error, so a vision claim on such a segment simply writes no rows). A genuine ffmpeg failure is
/// an error.
pub async fn sample_frames(
    cfg: &WorkerConfig,
    seg: &SegmentRow,
    max_frames: usize,
) -> Result<Vec<DecodedFrame>> {
    let max_frames = max_frames.max(1);
    let path = blob_path(&seg.blob_uri)?;
    let media = tokio::fs::read(&path)
        .await
        .with_context(|| format!("reading blob {}", path.display()))?;

    // Key off `container`, never the source (contract §7), exactly like media::extract_pcm.
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

    // Per-call UUID so two concurrent decodes of the SAME segment never share staging
    // paths. A long segment can be re-claimed (lease timeout) while still in-flight; keying
    // temp paths by segment_id alone let loop B's remove_dir_all/TempPath::drop wipe loop A's
    // frames mid-read → ffmpeg "No such file" + spurious vision errors. Mirrors media.rs:185.
    let run = uuid::Uuid::now_v7();

    // ffmpeg needs a seekable input for mp4, so stage the fragment on disk.
    let tmp_in = std::env::temp_dir().join(format!("hushai-vis-in-{}-{}.mp4", seg.segment_id, run));
    tokio::fs::write(&tmp_in, &fragment)
        .await
        .context("staging temp media for ffmpeg")?;
    let _c1 = TempPath(tmp_in.clone());

    // Emit frames as PNG into a temp dir; the `image` crate then yields dims + RGB directly.
    let out_dir = std::env::temp_dir().join(format!("hushai-vis-frames-{}-{}", seg.segment_id, run));
    let _ = tokio::fs::remove_dir_all(&out_dir).await; // clear any stale run
    tokio::fs::create_dir_all(&out_dir)
        .await
        .context("creating frame output dir")?;
    let _c2 = TempPath(out_dir.clone());
    let pattern = out_dir.join("f_%03d.png");

    // Spread `max_frames` across the segment duration; fall back to 1 fps if duration is unknown.
    let dur_secs = (seg.duration_nanos as f64 / 1e9).max(0.001);
    let fps = (max_frames as f64 / dur_secs).clamp(0.5, 30.0);

    let output = tokio::process::Command::new(&cfg.ffmpeg_bin)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(&tmp_in)
        // -an: drop audio. fps filter subsamples; -frames:v caps the count.
        .args([
            "-an",
            "-vf",
            &format!("fps={fps}"),
            "-frames:v",
            &max_frames.to_string(),
            "-f",
            "image2",
        ])
        .arg(&pattern)
        .output()
        .await
        .with_context(|| {
            format!(
                "running {} for frames on {}",
                cfg.ffmpeg_bin,
                tmp_in.display()
            )
        })?;

    if !output.status.success() {
        // A segment with no video stream (audio-only) or otherwise undecodable: treat as "no
        // frames" rather than failing the vision claim — the same tolerance media.rs lacks for
        // the audio path on video-only blobs.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("does not contain any stream")
            || stderr.contains("Output file is empty")
            || stderr.contains("Invalid data found")
        {
            tracing::debug!(segment_id = %seg.segment_id, "no decodable video frames: {}", stderr.trim());
            return Ok(Vec::new());
        }
        return Err(anyhow!(
            "ffmpeg frames exited {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    // Load the produced PNGs in deterministic order; approximate each frame's offset.
    let mut entries: Vec<PathBuf> = Vec::new();
    let mut rd = tokio::fs::read_dir(&out_dir)
        .await
        .context("reading frame dir")?;
    while let Some(e) = rd.next_entry().await? {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) == Some("png") {
            entries.push(p);
        }
    }
    entries.sort();

    let mut frames = Vec::with_capacity(entries.len());
    for (i, p) in entries.into_iter().enumerate() {
        let img = image::open(&p)
            .with_context(|| format!("decoding frame {}", p.display()))?
            .to_rgb8();
        let offset_nanos = ((i as f64 + 0.5) / fps * 1e9) as i64;
        frames.push(DecodedFrame {
            offset_nanos,
            image: img,
        });
    }
    Ok(frames)
}

/// Removes a staged temp file/dir when dropped (even on early return / panic).
struct TempPath(PathBuf);
impl Drop for TempPath {
    fn drop(&mut self) {
        if self.0.is_dir() {
            let _ = std::fs::remove_dir_all(&self.0);
        } else {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}
