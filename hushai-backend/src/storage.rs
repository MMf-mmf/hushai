//! Content-addressed blob storage and the durable write path.
//!
//! Layout under the canonical blob root:
//! ```text
//! {root}/tmp/seg-<uuid>.tmp        # in-progress uploads (same filesystem as blobs)
//! {root}/blobs/ab/cd/<sha256hex>   # promoted, content-addressed media
//! ```
//!
//! Durability (the only path that may yield 200):
//! 1. stream body → temp file, hashing as we go;
//! 2. caller compares digest/length to the manifest (mismatch → never promote);
//! 3. fsync temp → atomic rename into the content path → fsync the destination
//!    directory (POSIX rename durability).
//! Because the blob is durable BEFORE the DB row commits, a crash can only orphan
//! a (GC-able) blob — never commit a row pointing at missing bytes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::multipart::Field;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::error::IngestError;

pub const STORAGE_BACKEND: &str = "file";

/// Grace window for [`reclaim_blobs`]: never unlink a blob whose file was (re)created within this
/// span. Comfortably longer than any ingest promote→row-commit gap, so a blob just written for an
/// in-flight segment (whose row may not have committed yet) is never mistaken for an orphan.
pub const RECLAIM_GRACE: Duration = Duration::from_secs(600);

/// A temp blob on disk, removed on drop unless explicitly persisted (RAII guard
/// ensures a failed/aborted upload never leaves a stray temp file).
#[derive(Debug)]
pub struct TempBlob {
    path: PathBuf,
    persisted: bool,
}

impl TempBlob {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            persisted: false,
        }
    }
}

impl Drop for TempBlob {
    fn drop(&mut self) {
        if !self.persisted {
            // Best-effort cleanup; a leftover temp is harmless and GC-able.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A streamed-and-hashed upload awaiting promotion.
#[derive(Debug)]
pub struct HashedTemp {
    guard: TempBlob,
    pub sha256: [u8; 32],
    pub byte_len: i64,
}

fn blobs_dir(root: &Path) -> PathBuf {
    root.join("blobs")
}

fn tmp_dir(root: &Path) -> PathBuf {
    root.join("tmp")
}

/// Create the `blobs/` and `tmp/` subtrees if missing, and fsync the root so those
/// directory entries are themselves durable (the boot-durable ancestor that
/// `promote`'s fsync chain stops at).
pub async fn ensure_layout(root: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(blobs_dir(root)).await?;
    tokio::fs::create_dir_all(tmp_dir(root)).await?;
    fsync_dir(root).await?;
    Ok(())
}

/// `{root}/blobs/<ab>/<cd>/<full-hex>` for a 64-char lowercase sha256 hex.
pub fn shard_path(root: &Path, sha_hex: &str) -> PathBuf {
    blobs_dir(root)
        .join(&sha_hex[0..2])
        .join(&sha_hex[2..4])
        .join(sha_hex)
}

fn blob_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Stream a multipart `body` field to a temp file while computing SHA-256 and
/// byte length. The body is never buffered in memory. Works regardless of
/// whether the manifest part has been seen yet.
pub async fn stream_to_temp_and_hash(
    mut field: Field<'_>,
    root: &Path,
) -> Result<HashedTemp, IngestError> {
    let temp_path = tmp_dir(root).join(format!("seg-{}.tmp", Uuid::now_v7()));
    let file = tokio::fs::File::create(&temp_path).await.map_err(io_err)?;
    let guard = TempBlob::new(temp_path);

    let mut writer = tokio::io::BufWriter::new(file);
    let mut hasher = Sha256::new();
    let mut byte_len: u64 = 0;

    while let Some(chunk) = field.chunk().await? {
        hasher.update(&chunk);
        byte_len += chunk.len() as u64;
        writer.write_all(&chunk).await.map_err(io_err)?;
    }

    writer.flush().await.map_err(io_err)?;
    // fsync the temp file's contents before it becomes promotable.
    writer.into_inner().sync_all().await.map_err(io_err)?;

    let sha256: [u8; 32] = hasher.finalize().into();
    let byte_len = i64::try_from(byte_len).map_err(|_| IngestError::ValueOutOfRange("byte_len"))?;

    Ok(HashedTemp {
        guard,
        sha256,
        byte_len,
    })
}

/// Promote a hashed temp blob into its content-addressed home and return its
/// `file://` URI. Idempotent: if the content already exists on disk, the temp is
/// dropped and no second blob is written.
pub async fn promote(root: &Path, hashed: &mut HashedTemp) -> Result<String, IngestError> {
    let sha_hex = hex::encode(hashed.sha256);
    let final_path = shard_path(root, &sha_hex);
    let parent = final_path
        .parent()
        .expect("shard path always has a parent")
        .to_path_buf();

    tokio::fs::create_dir_all(&parent).await.map_err(io_err)?;

    if tokio::fs::try_exists(&final_path).await.map_err(io_err)? {
        // Content already durable. Refresh its mtime so a re-ingest of byte-identical content — a distinct
        // segment_id sharing one blob, reachable via re-import/loadtest since content_sha256 has no unique
        // constraint — resets the reclaim grace clock (see reclaim_blobs's too_new check). Otherwise a
        // concurrent background reclaim can observe the blob as unreferenced (the re-ingesting row not yet
        // committed) with an already-expired grace and unlink it, leaving a committed segment row pointing
        // at missing bytes. Best-effort: on failure the pre-existing grace window still applies.
        let refresh = final_path.clone();
        let _ = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&refresh)?
                .set_modified(std::time::SystemTime::now())
        })
        .await;
        // Leave guard unpersisted so the temp is removed.
        return Ok(blob_uri(&final_path));
    }

    tokio::fs::rename(&hashed.guard.path, &final_path)
        .await
        .map_err(io_err)?;
    hashed.guard.persisted = true; // file now lives at final_path

    // POSIX rename durability: fsync the destination dir AND every shard level we
    // may have just created (blobs/<ab>/<cd>, blobs/<ab>) up to the boot-durable
    // `blobs/` root. fsync'ing only the leaf would leave a new shard's dirent
    // unflushed in its parent, so a crash could lose the blob despite a 200.
    let blobs_root = blobs_dir(root);
    let mut dir = final_path.parent();
    while let Some(d) = dir {
        fsync_dir(d).await.map_err(io_err)?;
        if d == blobs_root {
            break;
        }
        dir = d.parent();
    }
    Ok(blob_uri(&final_path))
}

/// fsync a directory entry so renames/creations within it survive a crash (POSIX).
async fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        std::fs::File::open(&dir)?.sync_all()
    })
    .await
    .map_err(|join_err| std::io::Error::new(std::io::ErrorKind::Other, join_err))?
}

/// Free bytes available on the volume backing `path`.
pub fn free_space_bytes(path: &Path) -> std::io::Result<u64> {
    fs2::available_space(path)
}

/// Pre-write gate: require at least `watermark` free bytes, else 507. A failure
/// to even stat the volume is treated as storage pressure (fail safe).
pub fn ensure_capacity(root: &Path, watermark: u64) -> Result<(), IngestError> {
    match free_space_bytes(root) {
        Ok(free) if free >= watermark => Ok(()),
        _ => Err(IngestError::StoragePressure),
    }
}

fn io_err(e: std::io::Error) -> IngestError {
    IngestError::Internal(e.into())
}

/// Reclaim the content-addressed blob files for `shas` whose content is no longer referenced by
/// ANY segment row. Returns the total bytes unlinked. Safe to call after a footage/device delete
/// has COMMITTED: it re-checks each candidate against the live DB (so a blob still referenced by a
/// kept segment — possible because storage is content-addressed and byte-identical segments share
/// one file — is never removed), and skips files newer than `grace`.
///
/// Ordering contract (mirrors the write path's durability rule): callers DELETE the rows and
/// COMMIT first, THEN call this. A crash in between only orphans a blob (reclaimed by a later
/// pass), never dangles a row pointing at missing bytes. Best-effort: a failed unlink is logged
/// and skipped, never surfaced as a request error.
///
/// NOTE on the residual race: a *different* `segment_id` re-promoting byte-identical content
/// inside the grace window cannot occur with the current capture clients (distinct captures are
/// never byte-identical; retransmits reuse the same `segment_id`, deduped by
/// `ON CONFLICT (segment_id) DO NOTHING`, so a blob's ref-count only ever goes 1→0). Fully closing
/// it would require `promote()` to hold a per-sha advisory lock across its row commit — a deferred
/// hardening, unnecessary today.
pub async fn reclaim_blobs(pool: &PgPool, root: &Path, shas: &[[u8; 32]], grace: Duration) -> u64 {
    let mut freed: u64 = 0;
    // Chunk so the re-check array + round-trips stay bounded on a whole-day delete (~43k segments).
    for chunk in shas.chunks(1000) {
        let candidates: Vec<Vec<u8>> = chunk.iter().map(|s| s.to_vec()).collect();
        let unreferenced: Vec<Vec<u8>> = match sqlx::query_scalar(
            "SELECT cand FROM unnest($1::bytea[]) AS cand \
             WHERE NOT EXISTS (SELECT 1 FROM segments WHERE content_sha256 = cand)",
        )
        .bind(&candidates)
        .fetch_all(pool)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                // Fail safe: never unlink a blob we couldn't confirm is unreferenced.
                tracing::warn!(error = %e, "reclaim_blobs: reference re-check failed; skipping chunk");
                continue;
            }
        };

        for sha in unreferenced {
            let sha_hex = hex::encode(&sha);
            let path = shard_path(root, &sha_hex);
            let meta = match tokio::fs::metadata(&path).await {
                Ok(m) => m,
                Err(_) => continue, // already gone
            };
            // Grace: protect a just-(re)created blob whose owning row may not have committed yet.
            let too_new = meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map(|age| age < grace)
                .unwrap_or(false);
            if too_new {
                continue;
            }
            let len = meta.len();
            match tokio::fs::remove_file(&path).await {
                Ok(()) => freed += len,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(error = %e, blob = %sha_hex, "reclaim_blobs: unlink failed")
                }
            }
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_path_two_level_shard() {
        let root = Path::new("/data");
        let hex = "ab".to_string() + &"cd".to_string() + &"ef".repeat(30);
        let p = shard_path(root, &hex);
        assert_eq!(p, Path::new("/data/blobs/ab/cd").join(&hex));
    }
}
