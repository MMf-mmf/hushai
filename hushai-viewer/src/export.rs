//! Footage export: stream a device's window as a single downloadable MP4.
//!
//! Reuses the scrub pipeline's per-segment MPEG-TS remux (`remux::ensure_ts`, shared cache), then
//! pipes the concatenated TS through ONE long-lived ffmpeg that stream-copies it into a *fragmented*
//! MP4 (`+frag_keyframe+empty_moov`, so the moov is written without a seekable output → true
//! streaming, bounded memory, any window size). Per-segment TS carries each segment's codec init, so
//! heterogeneous fragments concatenate cleanly; a codec/resolution CHANGE within the window still
//! can't go into one clean copy-MP4 (re-encode is a follow-up) — restrict an export to one kind.
//!
//! Auth: this is a viewer route (cookie-gated), NOT proxied — the viewer owns ffmpeg + the blob
//! cache. A plain `<a download>` works because the session cookie rides along.

use std::process::Stdio;

use axum::body::Body;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::header;
use axum::response::Response;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::error::{ViewerError, ViewerResult};
use crate::remux::{self, Variant};
use crate::state::ViewerState;
use crate::timeline;

#[derive(Debug, Deserialize)]
pub struct ExportParams {
    pub from: Option<i64>,
    pub to: Option<i64>,
    /// "muxed" (default) or "video".
    pub kind: Option<String>,
}

pub async fn export_mp4(
    State(state): State<ViewerState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(device_id): Path<String>,
    Query(p): Query<ExportParams>,
) -> ViewerResult<Response> {
    let from = p.from.unwrap_or(0);
    let to = p.to.unwrap_or(i64::MAX);
    // Clamp like the playlist/detections/processing routes so one export can't remux a
    // device's entire history (an ffmpeg-per-segment storm) — bounds worst-case CPU/IO.
    let (from, to) = crate::routes::clamp_window(from, to, state.cfg.max_window_nanos);
    let (variant, media_type) = match p.kind.as_deref() {
        Some("video") => (Variant::Video, 2),
        _ => (Variant::Muxed, 3),
    };

    // Audit the data-egress (roadmap B6): exporting footage off the system is compliance-relevant.
    let actor = if state.cfg.auth_disabled { "local" } else { "admin" };
    hushai_backend::audit::record(
        &state.pool,
        hushai_backend::audit::AuditEntry::event(actor, Some(peer.ip().to_string()), "footage.export")
            .with_target("device", device_id.clone())
            .with_detail(serde_json::json!({ "from": from, "to": to, "kind": variant.suffix() })),
    )
    .await;

    // The window's segments of this kind, already in stitch order.
    let rows = timeline::windowed_segments(&state.pool, &device_id, from, to).await?;
    let segs: Vec<timeline::SegmentRow> = rows
        .into_iter()
        .filter(|r| r.media_type == media_type)
        .collect();
    if segs.is_empty() {
        return Err(ViewerError::NotFound(format!(
            "no {} footage for device {device_id} in the requested window",
            variant.suffix()
        )));
    }

    // Cap concurrent exports on a DEDICATED permit (not ffmpeg_sem, which the per-segment remux
    // feeder below needs — sharing would deadlock at low concurrency). Fail fast with 503 rather
    // than queue a long-lived export; the permit is held by the reaper until ffmpeg exits.
    let export_permit = state
        .export_sem
        .clone()
        .try_acquire_owned()
        .map_err(|_| ViewerError::Busy)?;

    let mut child = tokio::process::Command::new(&state.cfg.ffmpeg_bin)
        .args([
            "-hide_banner", "-loglevel", "error", "-nostdin", "-f", "mpegts", "-i", "pipe:0",
        ])
        .args([
            "-map", "0", "-c", "copy", "-movflags", "+frag_keyframe+empty_moov+default_base_moof",
        ])
        .args(["-f", "mp4", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ViewerError::Internal(anyhow::anyhow!("spawning ffmpeg for export: {e}")))?;

    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");

    // Feeder: remux each segment to its (cached) TS and write it to ffmpeg's stdin in order, then
    // EOF. A read/remux error TRUNCATES the export (break) rather than silently skipping the segment:
    // an interior skip yields a clean-EOF MP4 with a hidden gap, which for evidence footage is worse
    // than an obviously-short download. A write error means the client/ffmpeg went away.
    let feeder_state = state.clone();
    tokio::spawn(async move {
        for seg in &segs {
            match remux::ensure_ts(&feeder_state, &seg.sha_hex, variant).await {
                Ok(path) => match tokio::fs::read(&path).await {
                    Ok(bytes) => {
                        if stdin.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, sha = %seg.sha_hex, "export: read TS failed; truncating export here");
                        break;
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, sha = %seg.sha_hex, "export: remux failed; truncating export here");
                    break;
                }
            }
        }
        let _ = stdin.shutdown().await;
    });

    // Reaper: own the Child so it's reaped once the stream ends; `kill_on_drop` tears ffmpeg down if
    // the client disconnects (stdout reader dropped → ffmpeg SIGPIPEs → exits).
    tokio::spawn(async move {
        let _permit = export_permit; // released when ffmpeg exits (stream done / client disconnect)
        let _ = child.wait().await;
    });

    let filename = format!("{}-export.mp4", sanitize(&device_id));
    let body = Body::from_stream(ReaderStream::new(stdout));
    Response::builder()
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .header(header::CACHE_CONTROL, "no-store")
        .body(body)
        .map_err(|e| ViewerError::Internal(anyhow::anyhow!(e)))
}

/// Conservative filename slug for the Content-Disposition header.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
