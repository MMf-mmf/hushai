//! One-time pre-split of the source video into an fMP4 init segment + N media segment bodies,
//! each hashed ONCE. Every virtual camera replays these exact bytes (identity-only fan-out), so the
//! only per-emit work is an `Arc` clone + a tiny manifest encode + the POST — never a decode or
//! re-hash. The ffmpeg invocation matches `local_dev/feed_segments.py` so bodies are byte-identical
//! to the reference client.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One pre-split media segment, shared by `Arc` across all cameras.
pub struct SegmentBody {
    pub body: Arc<Vec<u8>>,
    pub content_sha256: [u8; 32],
    pub byte_len: i64,
}

/// The immutable, shared corpus: the init segment + all media bodies.
pub struct Corpus {
    pub init_bytes: Arc<Vec<u8>>,
    pub segments: Arc<Vec<SegmentBody>>,
    pub seg_seconds: u64,
}

impl Corpus {
    pub fn build(video: &Path, work_dir: &Path, seg_seconds: u64) -> Result<Self> {
        std::fs::create_dir_all(work_dir)
            .with_context(|| format!("create work dir {}", work_dir.display()))?;
        let init = work_dir.join("init.mp4");
        let mut segs = collect_segments(work_dir)?;
        if !(init.exists() && !segs.is_empty()) {
            run_ffmpeg(video, work_dir, seg_seconds)?;
            segs = collect_segments(work_dir)?;
        }
        if !init.exists() || segs.is_empty() {
            anyhow::bail!("ffmpeg produced no init/segments in {}", work_dir.display());
        }

        let init_bytes = Arc::new(std::fs::read(&init).context("read init.mp4")?);
        let mut bodies = Vec::with_capacity(segs.len());
        for p in &segs {
            let body = std::fs::read(p).with_context(|| format!("read {}", p.display()))?;
            let mut h = Sha256::new();
            h.update(&body);
            let sha: [u8; 32] = h.finalize().into();
            let byte_len = body.len() as i64;
            bodies.push(SegmentBody { body: Arc::new(body), content_sha256: sha, byte_len });
        }
        tracing::info!(
            segments = bodies.len(),
            init_bytes = init_bytes.len(),
            "corpus ready (bodies hashed once; reused by every camera)"
        );
        Ok(Self { init_bytes, segments: Arc::new(bodies), seg_seconds })
    }

    pub fn duration_nanos(&self) -> i64 {
        (self.seg_seconds as i64) * 1_000_000_000
    }
}

fn collect_segments(work_dir: &Path) -> Result<Vec<PathBuf>> {
    if !work_dir.exists() {
        return Ok(Vec::new());
    }
    let mut v: Vec<PathBuf> = std::fs::read_dir(work_dir)
        .with_context(|| format!("read dir {}", work_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("seg_") && n.ends_with(".m4s"))
                .unwrap_or(false)
        })
        .collect();
    v.sort();
    Ok(v)
}

fn run_ffmpeg(video: &Path, work_dir: &Path, seg_seconds: u64) -> Result<()> {
    if !video.exists() {
        anyhow::bail!("source video not found: {}", video.display());
    }
    // Clear stale outputs so segment numbering is deterministic.
    for p in collect_segments(work_dir)? {
        let _ = std::fs::remove_file(p);
    }
    let _ = std::fs::remove_file(work_dir.join("init.mp4"));

    tracing::info!(video = %video.display(), "splitting into ~{seg_seconds}s MUXED fMP4 segments via ffmpeg");
    let status = std::process::Command::new("ffmpeg")
        .current_dir(work_dir)
        .args(["-y", "-i"])
        .arg(video)
        .args([
            "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p", "-c:a", "aac",
            "-force_key_frames", &format!("expr:gte(t,n_forced*{seg_seconds})"),
            "-hls_time", &seg_seconds.to_string(),
            "-hls_segment_type", "fmp4",
            "-hls_fmp4_init_filename", "init.mp4",
            "-hls_segment_filename", "seg_%05d.m4s",
            "-hls_list_size", "0",
            "-hls_playlist_type", "vod",
            "index.m3u8",
        ])
        .status()
        .context("spawn ffmpeg (is it installed and on PATH?)")?;
    if !status.success() {
        anyhow::bail!("ffmpeg failed: {status}");
    }
    Ok(())
}
