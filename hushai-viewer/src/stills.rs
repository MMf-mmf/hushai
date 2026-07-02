//! Lazy, content-addressed still-frame extraction from stored segments: one JPEG per
//! (segment, width). First frame only — segments are ~2s and start on a keyframe, so a
//! single-frame decode is cheap and deterministic. Backs the timeline hover preview
//! (`thumb.jpg`, 320w) and the camera-grid tiles (`poster.jpg`, 480w).
//!
//! Mirrors remux.rs: cached forever under `cache/still/<w>/<ab>/<cd>/<sha>.jpg` (blobs
//! are immutable, so a cached still never needs invalidation), single-flight per key,
//! bounded by the shared ffmpeg semaphore, and evicted by the same LRU cache reaper.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{ViewerError, ViewerResult};
use crate::remux::{TempPath, blob_path, is_sha_hex, lookup_blob, stage_input};
use crate::state::ViewerState;

/// Timeline hover-preview width (px).
pub const THUMB_W: u32 = 320;
/// Camera-grid tile width (px).
pub const POSTER_W: u32 = 480;

fn cache_path(cache_dir: &Path, sha_hex: &str, width: u32) -> PathBuf {
    cache_dir
        .join("still")
        .join(width.to_string())
        .join(&sha_hex[0..2])
        .join(&sha_hex[2..4])
        .join(format!("{sha_hex}.jpg"))
}

/// Get the cached still for `(sha, width)`, extracting on demand. Single-flight per
/// key + the global ffmpeg semaphore keep a fast hover-scrub from forking many ffmpegs.
pub async fn ensure_still(state: &ViewerState, sha_hex: &str, width: u32) -> ViewerResult<PathBuf> {
    if !is_sha_hex(sha_hex) {
        return Err(ViewerError::BadRequest("invalid segment hash".into()));
    }
    let final_path = cache_path(&state.cfg.cache_dir, sha_hex, width);
    if tokio::fs::try_exists(&final_path)
        .await
        .map_err(anyhow::Error::from)?
    {
        return Ok(final_path);
    }

    let key = format!("{sha_hex}.{width}.jpg");
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
            let r = extract_to(state, sha_hex, width, &final_path).await;
            drop(permit);
            r
        }
    };
    state.inflight_release(&key);
    result.map(|_| final_path)
}

async fn extract_to(
    state: &ViewerState,
    sha_hex: &str,
    width: u32,
    final_path: &Path,
) -> ViewerResult<()> {
    let (blob_uri, container, init, _capture_start_ns) = lookup_blob(&state.pool, sha_hex)
        .await?
        .ok_or_else(|| ViewerError::NotFound(format!("no segment with hash {sha_hex}")))?;
    let blob = blob_path(&blob_uri)?;

    let parent = final_path.parent().expect("cache path has a parent");
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(anyhow::Error::from)?;

    let (input_path, staged_input) =
        stage_input(parent, sha_hex, &blob, &container, init.as_deref()).await?;
    let tmp_out = parent.join(format!(".out-{sha_hex}-{}.jpg", Uuid::now_v7()));
    let mut out_guard = TempPath(Some(tmp_out.clone()));

    let scale = format!("scale={width}:-2");
    let output = tokio::process::Command::new(&state.cfg.ffmpeg_bin)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(&input_path)
        .args(["-frames:v", "1", "-vf", &scale, "-q:v", "6", "-f", "image2"])
        .arg(&tmp_out)
        // Kill the child if this future is dropped (a cancelled hover) — see remux.rs.
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("running {}", state.cfg.ffmpeg_bin))?;
    if !output.status.success() {
        return Err(anyhow!(
            "ffmpeg still of {sha_hex} ({width}px) exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }

    tokio::fs::rename(&tmp_out, final_path)
        .await
        .context("promoting still into cache")?;
    out_guard.disarm(); // renamed away; nothing to clean
    drop(staged_input); // remove the fmp4 staging temp now
    Ok(())
}

/// The video-bearing segment covering wall-clock `t_ns` (muxed preferred over video-only),
/// else the nearest within ±5s — hover previews shouldn't 404 in the sub-second cracks
/// between segments.
pub async fn video_sha_at(
    pool: &PgPool,
    device_id: &str,
    t_ns: i64,
) -> ViewerResult<Option<String>> {
    let exact: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT encode(content_sha256, 'hex')
        FROM segments
        WHERE device_id = $1 AND media_type IN (2, 3)
          AND capture_start_unix_nanos <= $2
          AND capture_start_unix_nanos + duration_nanos > $2
        ORDER BY media_type DESC
        LIMIT 1
        "#,
    )
    .bind(device_id)
    .bind(t_ns)
    .fetch_optional(pool)
    .await?;
    if let Some((sha,)) = exact {
        return Ok(Some(sha));
    }
    let near: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT encode(content_sha256, 'hex')
        FROM segments
        WHERE device_id = $1 AND media_type IN (2, 3)
          AND capture_start_unix_nanos BETWEEN $2 - 5000000000 AND $2 + 5000000000
        ORDER BY abs(capture_start_unix_nanos - $2), media_type DESC
        LIMIT 1
        "#,
    )
    .bind(device_id)
    .bind(t_ns)
    .fetch_optional(pool)
    .await?;
    Ok(near.map(|r| r.0))
}

/// The device's newest video-bearing segment (the camera-grid "poster" frame).
pub async fn latest_video_sha(pool: &PgPool, device_id: &str) -> ViewerResult<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT encode(content_sha256, 'hex')
        FROM segments
        WHERE device_id = $1 AND media_type IN (2, 3)
        ORDER BY capture_start_unix_nanos DESC, media_type DESC
        LIMIT 1
        "#,
    )
    .bind(device_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_path_shards_by_width_then_sha_prefix() {
        let sha = "ab".repeat(32);
        let p = cache_path(Path::new("/cache"), &sha, THUMB_W);
        assert_eq!(
            p,
            Path::new("/cache/still/320/ab/ab").join(format!("{sha}.jpg"))
        );
    }
}
