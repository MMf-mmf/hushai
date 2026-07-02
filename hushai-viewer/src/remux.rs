//! Lazy, content-addressed remux of a stored blob into an MPEG-TS segment that
//! hls.js can stitch. Stream-copy only (`-c copy`) — no re-encode.
//!
//! Two source conventions (keyed off `container`, never the source — contract §7):
//!  - `mp4` (Android): self-contained MP4; remux directly. Video needs the AVCC→Annex-B
//!    bitstream filter; audio needs the mux start-offset zeroed.
//!  - `fmp4` (feed_segments.py): bare `moof`+`mdat`; prepend `codec_init_data`
//!    (`ftyp`+`moov`) to a temp file first, then remux (one muxed TS).
//!
//! The result is cached forever under `cache/ts/<ab>/<cd>/<sha>.<variant>.ts` — blobs
//! are immutable, so a cached TS never needs invalidation.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{ViewerError, ViewerResult};
use crate::state::ViewerState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Video,
    Audio,
    Muxed,
}

impl Variant {
    pub fn parse(s: &str) -> Option<Variant> {
        match s {
            "video" => Some(Variant::Video),
            "audio" => Some(Variant::Audio),
            "muxed" => Some(Variant::Muxed),
            _ => None,
        }
    }
    pub fn suffix(self) -> &'static str {
        match self {
            Variant::Video => "video",
            Variant::Audio => "audio",
            Variant::Muxed => "muxed",
        }
    }
}

/// A 64-char lowercase sha256 hex — guards path construction against panics/traversal.
pub fn is_sha_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn cache_path(cache_dir: &Path, sha_hex: &str, variant: Variant) -> PathBuf {
    cache_dir
        .join("ts")
        .join(&sha_hex[0..2])
        .join(&sha_hex[2..4])
        .join(format!("{sha_hex}.{}.ts", variant.suffix()))
}

/// LRU-evict the TS remux cache down to `max_bytes` (oldest mtime first). The cache is
/// content-addressed derived data — safe to delete, regenerated on demand — and never self-evicts,
/// so without this it grows unbounded on the same volume the disk watermark guards. Blocking (std::fs);
/// call via spawn_blocking. Returns (total_bytes_seen, bytes_freed). Best-effort: unreadable/undeletable
/// entries are skipped.
pub fn reap_cache(cache_dir: &Path, max_bytes: u64) -> std::io::Result<(u64, u64)> {
    let ts_dir = cache_dir.join("ts");
    if !ts_dir.exists() {
        return Ok((0, 0));
    }
    fn walk(dir: &Path, files: &mut Vec<(PathBuf, u64, std::time::SystemTime)>, total: &mut u64) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                walk(&e.path(), files, total);
            } else if md.is_file() {
                *total += md.len();
                files.push((e.path(), md.len(), md.modified().unwrap_or(std::time::UNIX_EPOCH)));
            }
        }
    }
    let mut files = Vec::new();
    let mut total = 0u64;
    walk(&ts_dir, &mut files, &mut total);
    if total <= max_bytes {
        return Ok((total, 0));
    }
    files.sort_by_key(|(_, _, mt)| *mt); // oldest first
    let (mut cur, mut freed) = (total, 0u64);
    for (p, sz, _) in files {
        if cur <= max_bytes {
            break;
        }
        if std::fs::remove_file(&p).is_ok() {
            cur -= sz.min(cur);
            freed += sz;
        }
    }
    Ok((total, freed))
}

/// `file:///abs/path` -> `/abs/path` (the backend always writes absolute `file://` URIs).
fn blob_path(blob_uri: &str) -> anyhow::Result<PathBuf> {
    let p = blob_uri
        .strip_prefix("file://")
        .ok_or_else(|| anyhow!("unsupported blob_uri scheme (expected file://): {blob_uri}"))?;
    Ok(PathBuf::from(p))
}

/// Removes a path on drop unless disarmed (RAII for temp inputs/outputs).
struct TempPath(Option<PathBuf>);
impl TempPath {
    fn disarm(&mut self) {
        self.0 = None;
    }
}
impl Drop for TempPath {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

async fn lookup_blob(
    pool: &PgPool,
    sha_hex: &str,
) -> ViewerResult<Option<(String, String, Option<Vec<u8>>, i64)>> {
    // Any row with these exact bytes works (content-addressed; identical bytes decode
    // identically). codec_init_data only matters for the fmp4 prepend.
    // capture_start anchors the output PTS so the separate video + audio renditions
    // share one absolute clock (see remux_to).
    let row: Option<(String, String, Option<Vec<u8>>, i64)> = sqlx::query_as(
        r#"
        SELECT blob_uri, container, codec_init_data, capture_start_unix_nanos
        FROM segments
        WHERE content_sha256 = decode($1, 'hex')
        LIMIT 1
        "#,
    )
    .bind(sha_hex)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Get the cached TS path for `(sha, variant)`, remuxing on demand. Single-flight per
/// key + a global ffmpeg semaphore keep a fast scrub from forking many ffmpegs.
pub async fn ensure_ts(
    state: &ViewerState,
    sha_hex: &str,
    variant: Variant,
) -> ViewerResult<PathBuf> {
    if !is_sha_hex(sha_hex) {
        return Err(ViewerError::BadRequest("invalid segment hash".into()));
    }
    let final_path = cache_path(&state.cfg.cache_dir, sha_hex, variant);
    if tokio::fs::try_exists(&final_path)
        .await
        .map_err(anyhow::Error::from)?
    {
        return Ok(final_path);
    }

    let key = format!("{sha_hex}.{}", variant.suffix());
    let lock = state.inflight_lock(&key);
    let result = {
        let _guard = lock.lock().await;
        // Re-check: another request may have produced it while we waited.
        if tokio::fs::try_exists(&final_path)
            .await
            .map_err(anyhow::Error::from)?
        {
            Ok(())
        } else {
            let permit = state
                .ffmpeg_sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ViewerError::Busy)?;
            let r = remux_to(state, sha_hex, variant, &final_path).await;
            drop(permit);
            r
        }
    };
    state.inflight_release(&key);
    result.map(|_| final_path)
}

async fn remux_to(
    state: &ViewerState,
    sha_hex: &str,
    variant: Variant,
    final_path: &Path,
) -> ViewerResult<()> {
    let (blob_uri, container, init, capture_start_ns) = lookup_blob(&state.pool, sha_hex)
        .await?
        .ok_or_else(|| ViewerError::NotFound(format!("no segment with hash {sha_hex}")))?;
    let blob = blob_path(&blob_uri)?;

    // Absolute output-PTS anchor (seconds.nanos). Stamping every segment — video AND
    // audio — with its wall-clock start puts both renditions on one shared clock, so
    // hls.js can interleave the separate audio rendition with video (otherwise each
    // independent TS restarts at PTS 0 and only the first audio segment ever aligns →
    // bufferStalledError). The mpegts 33-bit PTS wraps ~26.5h; hls.js corrects rollover.
    let ts_offset = format!(
        "{}.{:09}",
        capture_start_ns.div_euclid(1_000_000_000),
        capture_start_ns.rem_euclid(1_000_000_000)
    );

    let parent = final_path.parent().expect("cache path has a parent");
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(anyhow::Error::from)?;

    // fmp4 blobs are bare fragments — stage init+blob to a seekable temp mp4 first.
    let needs_init = container.eq_ignore_ascii_case("fmp4");
    let mut staged_input = TempPath(None);
    let input_path: PathBuf = if needs_init {
        let media = tokio::fs::read(&blob)
            .await
            .with_context(|| format!("reading blob {}", blob.display()))?;
        let init_bytes = init.as_deref().unwrap_or_default();
        let mut buf = Vec::with_capacity(init_bytes.len() + media.len());
        buf.extend_from_slice(init_bytes);
        buf.extend_from_slice(&media);
        let tmp = parent.join(format!(".in-{sha_hex}-{}.mp4", Uuid::now_v7()));
        tokio::fs::write(&tmp, &buf)
            .await
            .context("staging fmp4 input for ffmpeg")?;
        staged_input = TempPath(Some(tmp.clone()));
        tmp
    } else {
        blob.clone()
    };

    let tmp_out = parent.join(format!(".out-{sha_hex}-{}.ts", Uuid::now_v7()));
    let mut out_guard = TempPath(Some(tmp_out.clone()));

    let mut cmd = tokio::process::Command::new(&state.cfg.ffmpeg_bin);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(&input_path);
    match variant {
        Variant::Video => {
            cmd.args(["-map", "0:v:0", "-c:v", "copy", "-bsf:v", "h264_mp4toannexb"]);
        }
        Variant::Audio => {
            cmd.args(["-map", "0:a:0", "-c:a", "copy"]);
        }
        Variant::Muxed => {
            cmd.args(["-map", "0", "-c", "copy", "-bsf:v", "h264_mp4toannexb"]);
        }
    }
    // Zero the TS mux start offset (mp4->TS otherwise injects a spurious ~1.4s start
    // offset), anchor the output PTS to the segment's absolute capture time so video +
    // audio share a clock, then write MPEG-TS.
    cmd.args(["-muxdelay", "0", "-muxpreload", "0"])
        .args(["-output_ts_offset", &ts_offset])
        .args(["-f", "mpegts"])
        .arg(&tmp_out);

    let output = cmd
        // Kill the child if this future is dropped (a cancelled /hls/seg scrub) — otherwise the
        // orphaned ffmpeg outlives its semaphore permit and transiently exceeds ffmpeg_concurrency.
        // (export.rs already sets this on its long-lived ffmpeg.)
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("running {}", state.cfg.ffmpeg_bin))?;
    if !output.status.success() {
        return Err(anyhow!(
            "ffmpeg remux of {sha_hex} ({}) exited {}: {}",
            variant.suffix(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }

    tokio::fs::rename(&tmp_out, final_path)
        .await
        .context("promoting remuxed TS into cache")?;
    out_guard.disarm(); // renamed away; nothing to clean
    drop(staged_input); // remove the fmp4 staging temp now
    Ok(())
}
