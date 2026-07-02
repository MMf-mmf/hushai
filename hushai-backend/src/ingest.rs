//! The `POST /v1/segments` handler: multipart state machine → integrity gate →
//! durable blob write → idempotent DB commit. The only path that returns 200,
//! and only after the body is fsync'd AND the row is committed.

use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use tracing::Instrument;

use crate::db::{self, Persisted};
use crate::error::IngestError;
use crate::proto::DecodedManifest;
use crate::state::AppState;
use crate::storage::{self, STORAGE_BACKEND};

/// The `manifest` part is small (a `SegmentManifest` is well under a KB); cap it
/// tightly so a hostile client can't make us buffer up to the full body limit in
/// memory. The opaque `body` part streams to disk and is bounded separately.
const MANIFEST_MAX_BYTES: usize = 1024 * 1024; // 1 MiB

pub async fn post_segment(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<StatusCode, IngestError> {
    // Global backpressure: shed (429) rather than queue unboundedly. The permit
    // is held for the lifetime of this request.
    let _permit = state
        .limiter
        .clone()
        .try_acquire_owned()
        .map_err(|_| IngestError::Overloaded)?;

    // Honest storage backpressure (507) before we write a single byte.
    storage::ensure_capacity(&state.blob_root, state.config.disk_watermark_bytes)?;

    // --- Multipart state machine: read parts by name, order-independent. ---
    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut hashed: Option<storage::HashedTemp> = None;

    while let Some(mut field) = multipart.next_field().await? {
        match field.name() {
            Some("manifest") => {
                if manifest_bytes.is_some() {
                    return Err(IngestError::MalformedMultipart(
                        "duplicate manifest part".into(),
                    ));
                }
                // Bounded read so the manifest can't balloon to the full body limit.
                let mut buf = Vec::new();
                while let Some(chunk) = field.chunk().await? {
                    if buf.len() + chunk.len() > MANIFEST_MAX_BYTES {
                        return Err(IngestError::MalformedMultipart(
                            "manifest part too large".into(),
                        ));
                    }
                    buf.extend_from_slice(&chunk);
                }
                manifest_bytes = Some(buf);
            }
            Some("body") => {
                if hashed.is_some() {
                    return Err(IngestError::MalformedMultipart(
                        "duplicate body part".into(),
                    ));
                }
                // Stream to disk + hash regardless of manifest arrival order.
                hashed = Some(storage::stream_to_temp_and_hash(field, &state.blob_root).await?);
            }
            _ => {
                // Drain and ignore unknown parts (forward-compatible).
                let _ = field.bytes().await?;
            }
        }
    }

    let manifest_bytes = manifest_bytes.ok_or(IngestError::MissingPart("manifest"))?;
    // If `body` is missing, any partial temp is cleaned up by HashedTemp's Drop.
    let mut hashed = hashed.ok_or(IngestError::MissingPart("body"))?;

    let manifest = DecodedManifest::decode(&manifest_bytes)?;

    // --- Integrity gate (422): never promote a blob that disagrees with its manifest. ---
    if hashed.byte_len != manifest.byte_len {
        return Err(IngestError::IntegrityMismatch("byte_len"));
    }
    if hashed.sha256 != manifest.content_sha256 {
        return Err(IngestError::IntegrityMismatch("content_sha256"));
    }

    let span = tracing::info_span!(
        "segment",
        segment_id = %manifest.segment_id,
        device_id = %manifest.device_id,
        stream_id = %manifest.stream_id,
        sequence = manifest.sequence,
    );

    async move {
        // Durable blob first, then the committed row (durability ordering §6).
        let blob_uri = storage::promote(&state.blob_root, &mut hashed).await?;
        let outcome =
            db::persist_segment(&state.pool, &manifest, &blob_uri, STORAGE_BACKEND).await?;

        let media = match manifest.media_type {
            1 => "audio",
            2 => "video",
            3 => "muxed",
            _ => "unknown",
        };
        // `source_kind` is UNVALIDATED client free text (contract §7); never use it raw as a metric
        // label or a rogue/compromised device could mint unbounded series in the (never-evicting)
        // registry. Bound it to a small known set; everything else collapses to "other".
        let source = source_label(&manifest.source_kind);
        match outcome {
            Persisted::Inserted => {
                crate::observe::counter(
                    "hushai_segments_ingested_total",
                    &[("source", source), ("media", media), ("result", "new")],
                );
                crate::observe::counter_by(
                    "hushai_ingest_bytes_total",
                    &[("source", source), ("media", media)],
                    manifest.byte_len.max(0) as u64,
                );
                tracing::info!(%blob_uri, "segment durably accepted (new)");
                Ok(StatusCode::OK)
            }
            Persisted::DuplicateSameBytes => {
                crate::observe::counter(
                    "hushai_segments_ingested_total",
                    &[("source", source), ("media", media), ("result", "duplicate")],
                );
                tracing::info!("segment already present (idempotent retry)");
                Ok(StatusCode::OK)
            }
            Persisted::ConflictDifferentBytes => {
                tracing::warn!("segment_id reused for different bytes");
                Err(IngestError::IdempotencyConflict)
            }
        }
    }
    .instrument(span)
    .await
}

/// Collapse the unvalidated client `source_kind` to a bounded metric label (cardinality guard).
/// Known first-party sources pass through; anything else → "other". (The DB keeps the raw value
/// per device, so the breakdown isn't lost — only the metric label is bounded.)
fn source_label(source_kind: &str) -> &'static str {
    match source_kind { // source_kind-allow: bounded metric label (observability, not behaviour)
        "android_app" => "android_app",
        "web_browser" => "web_browser",
        "rtsp" => "rtsp",
        "file_replay" => "file_replay",
        // Synthetic traffic from the capacity/load-test harness (hushai-loadtest). A bounded label
        // so a benchmark's ingest is distinguishable from real cameras in `hushai_segments_ingested_total`.
        "loadtest_replica" => "loadtest_replica",
        _ => "other",
    }
}
