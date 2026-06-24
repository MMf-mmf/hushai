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

use axum::extract::multipart::Field;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::error::IngestError;

pub const STORAGE_BACKEND: &str = "file";

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
        // Content already durable. Leave guard unpersisted so the temp is removed.
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
