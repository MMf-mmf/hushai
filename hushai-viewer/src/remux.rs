//! Lazy, content-addressed remux of a stored blob into an MPEG-TS segment that
//! hls.js can stitch. Stream-copy (`-c copy`) by default — EXCEPT a segment whose video
//! carries a rotation matrix is re-encoded upright (see `probe_rotation` / `needs_upright`),
//! because MPEG-TS/hls.js/MSE drop the matrix and `-c copy` alone would play sideways.
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

/// LRU-evict the derived-data caches down to `max_bytes` (oldest mtime first): the TS remux
/// cache plus the still-frame cache (stills.rs). Both are content-addressed derived data —
/// safe to delete, regenerated on demand — and never self-evict, so without this they grow
/// unbounded on the same volume the disk watermark guards. Blocking (std::fs);
/// call via spawn_blocking. Returns (total_bytes_seen, bytes_freed). Best-effort: unreadable/undeletable
/// entries are skipped.
pub fn reap_cache(cache_dir: &Path, max_bytes: u64) -> std::io::Result<(u64, u64)> {
    let roots = [cache_dir.join("ts"), cache_dir.join("still")];
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
    for root in &roots {
        if root.exists() {
            walk(root, &mut files, &mut total);
        }
    }
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
pub(crate) fn blob_path(blob_uri: &str) -> anyhow::Result<PathBuf> {
    let p = blob_uri
        .strip_prefix("file://")
        .ok_or_else(|| anyhow!("unsupported blob_uri scheme (expected file://): {blob_uri}"))?;
    Ok(PathBuf::from(p))
}

/// Removes a path on drop unless disarmed (RAII for temp inputs/outputs).
pub(crate) struct TempPath(pub(crate) Option<PathBuf>);
impl TempPath {
    pub(crate) fn disarm(&mut self) {
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

pub(crate) async fn lookup_blob(
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
    let (input_path, staged_input) =
        stage_input(parent, sha_hex, &blob, &container, init.as_deref()).await?;

    let tmp_out = parent.join(format!(".out-{sha_hex}-{}.ts", Uuid::now_v7()));
    let mut out_guard = TempPath(Some(tmp_out.clone()));

    // Does this segment's video carry a rotation matrix? Android stamps one
    // (setOrientationHint) so portrait/rotated capture displays upright — but a `-c copy`
    // remux drops it (MPEG-TS can't carry it; hls.js/MSE ignore it either way), so the
    // browser would play sideways. When rotated, re-encode the video so ffmpeg autorotate
    // BAKES the rotation into the pixels; 0°/matrix-less segments keep the fast copy path.
    // Audio never rotates. Fail open to copy on any probe error (never break playback).
    let needs_upright = state.cfg.upright_reencode
        && matches!(variant, Variant::Video | Variant::Muxed)
        && probe_rotation(&state.cfg.ffprobe_bin, &input_path).await != 0;

    // libx264 re-encode args for the upright path (autorotate is default-on, so do NOT add
    // `-vf transpose` or `-noautorotate`, and drop `h264_mp4toannexb` — that bsf is only for
    // copying AVCC into TS; the encoder's Annex-B output goes straight into the mpegts muxer).
    let crf = state.cfg.reencode_crf.to_string();
    let preset = state.cfg.reencode_preset.as_str();

    let mut cmd = tokio::process::Command::new(&state.cfg.ffmpeg_bin);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(&input_path);
    match variant {
        Variant::Video => {
            cmd.args(["-map", "0:v:0"]);
            if needs_upright {
                cmd.args(["-c:v", "libx264", "-preset", preset, "-crf", &crf, "-pix_fmt", "yuv420p"]);
            } else {
                cmd.args(["-c:v", "copy", "-bsf:v", "h264_mp4toannexb"]);
            }
        }
        Variant::Audio => {
            cmd.args(["-map", "0:a:0", "-c:a", "copy"]);
        }
        Variant::Muxed => {
            cmd.args(["-map", "0"]);
            if needs_upright {
                // Re-encode video upright, keep audio a stream-copy in the one ffmpeg.
                cmd.args(["-c:v", "libx264", "-preset", preset, "-crf", &crf, "-pix_fmt", "yuv420p"])
                    .args(["-c:a", "copy"]);
            } else {
                cmd.args(["-c", "copy", "-bsf:v", "h264_mp4toannexb"]);
            }
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

/// The video stream's display-matrix rotation in degrees (normalized to (-180, 180]), or 0
/// when there is none / on any error. ffmpeg 7.x exposes it under stream *side data*
/// (`"Display Matrix"` → `"rotation"`), NOT the legacy `stream_tags=rotate` (empty on 7.x).
/// We only need "is it non-zero" — ffmpeg autorotate reads the matrix itself for direction,
/// so the sign here doesn't matter. Fails open (returns 0 ⇒ fast copy path) on any error.
async fn probe_rotation(ffprobe_bin: &str, input: &Path) -> i32 {
    let output = tokio::process::Command::new(ffprobe_bin)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream_side_data_list",
            "-of",
            "json",
        ])
        .arg(input)
        .kill_on_drop(true)
        .output()
        .await;
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return 0,
    };
    rotation_from_ffprobe_json(&output.stdout)
}

/// Pure parse of `ffprobe -show_entries stream_side_data_list -of json` output → the video
/// display-matrix rotation normalized to (-180, 180], or 0 when absent/malformed. Split out
/// so it can be unit-tested without spawning ffprobe.
fn rotation_from_ffprobe_json(bytes: &[u8]) -> i32 {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return 0;
    };
    // { "streams": [ { "side_data_list": [ { "side_data_type": "Display Matrix",
    //   "rotation": -90 }, ... ] } ] }  — rotation may be a number or a string.
    let rot = json["streams"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["side_data_list"].as_array())
        .flatten()
        .find_map(|sd| match &sd["rotation"] {
            serde_json::Value::Number(n) => n.as_i64(),
            serde_json::Value::String(s) => s.trim().parse::<f64>().ok().map(|f| f as i64),
            _ => None,
        })
        .unwrap_or(0);
    ((rot.rem_euclid(360) + 180).rem_euclid(360) - 180) as i32
}

/// Stage a blob as a seekable ffmpeg input (shared with stills.rs). `fmp4` blobs are bare
/// `moof`+`mdat` fragments — prepend `codec_init_data` (`ftyp`+`moov`) into a temp mp4
/// beside `parent`; `mp4` blobs pass through untouched. The returned guard removes any
/// staged temp on drop.
pub(crate) async fn stage_input(
    parent: &Path,
    sha_hex: &str,
    blob: &Path,
    container: &str,
    init: Option<&[u8]>,
) -> ViewerResult<(PathBuf, TempPath)> {
    if !container.eq_ignore_ascii_case("fmp4") {
        return Ok((blob.to_path_buf(), TempPath(None)));
    }
    let media = tokio::fs::read(blob)
        .await
        .with_context(|| format!("reading blob {}", blob.display()))?;
    let init_bytes = init.unwrap_or_default();
    let mut buf = Vec::with_capacity(init_bytes.len() + media.len());
    buf.extend_from_slice(init_bytes);
    buf.extend_from_slice(&media);
    let tmp = parent.join(format!(".in-{sha_hex}-{}.mp4", Uuid::now_v7()));
    tokio::fs::write(&tmp, &buf)
        .await
        .context("staging fmp4 input for ffmpeg")?;
    Ok((tmp.clone(), TempPath(Some(tmp))))
}

#[cfg(test)]
mod tests {
    use super::{probe_rotation, rotation_from_ffprobe_json};

    fn side_data(rotation: &str) -> String {
        format!(
            r#"{{"streams":[{{"side_data_list":[{{"side_data_type":"Display Matrix","rotation":{rotation}}}]}}]}}"#
        )
    }

    #[test]
    fn parses_numeric_rotation() {
        // The exact shape ffprobe 7.1 emits for an Android-rotated segment.
        assert_eq!(rotation_from_ffprobe_json(side_data("90").as_bytes()), 90);
        assert_eq!(rotation_from_ffprobe_json(side_data("-90").as_bytes()), -90);
        assert_eq!(rotation_from_ffprobe_json(side_data("270").as_bytes()), -90);
    }

    #[test]
    fn parses_string_rotation() {
        // Some ffmpeg builds serialize rotation as a quoted string.
        assert_eq!(rotation_from_ffprobe_json(side_data(r#""90""#).as_bytes()), 90);
        assert_eq!(
            rotation_from_ffprobe_json(side_data(r#""-180""#).as_bytes()),
            -180
        );
    }

    #[test]
    fn one_eighty_is_nonzero() {
        // 180 must still trigger the upright re-encode (upside-down), normalized to -180.
        assert_eq!(rotation_from_ffprobe_json(side_data("180").as_bytes()), -180);
    }

    #[test]
    fn zero_and_multiples_of_360_are_zero() {
        assert_eq!(rotation_from_ffprobe_json(side_data("0").as_bytes()), 0);
        assert_eq!(rotation_from_ffprobe_json(side_data("360").as_bytes()), 0);
    }

    #[test]
    fn no_side_data_is_zero() {
        // No matrix (the common landscape/0° case) → fast copy path.
        assert_eq!(
            rotation_from_ffprobe_json(br#"{"streams":[{}]}"#),
            0
        );
        assert_eq!(rotation_from_ffprobe_json(br#"{"streams":[]}"#), 0);
    }

    #[test]
    fn malformed_json_fails_open_to_zero() {
        assert_eq!(rotation_from_ffprobe_json(b"not json"), 0);
        assert_eq!(rotation_from_ffprobe_json(b""), 0);
    }

    // Real ffmpeg/ffprobe round-trip: a landscape clip re-stamped with a 90° display matrix
    // (exactly what Android's setOrientationHint produces) must probe non-zero, while the
    // untouched landscape clip probes zero (fast copy path). Gated `#[ignore]` because it
    // spawns ffmpeg/ffprobe — run with `cargo test -p hushai-viewer -- --ignored probe_rotation`.
    #[tokio::test]
    #[ignore = "spawns ffmpeg/ffprobe; run with --ignored"]
    async fn probe_rotation_on_real_fixtures() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("hushai-rot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let land = dir.join("land.mp4");
        let rot = dir.join("rot.mp4");

        let ok = Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=1280x720:rate=30:duration=1")
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&land)
            .status()
            .expect("run ffmpeg");
        assert!(ok.success(), "ffmpeg landscape fixture failed");
        // Stamp a 90° matrix without touching pixels (mimics MediaMuxer.setOrientationHint).
        let ok = Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error", "-display_rotation", "90", "-i"])
            .arg(&land)
            .args(["-c", "copy"])
            .arg(&rot)
            .status()
            .expect("run ffmpeg");
        assert!(ok.success(), "ffmpeg rotate-stamp failed");

        assert_ne!(probe_rotation("ffprobe", &rot).await, 0, "rotated clip must probe non-zero");
        assert_eq!(probe_rotation("ffprobe", &land).await, 0, "landscape clip must probe zero");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
