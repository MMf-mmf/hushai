//! HTTP surface: read-only JSON API (`/api/*`), HLS playlists + segments (`/hls/*`),
//! and the bundled static UI (served at `/`). Same-origin ⇒ no CORS.

use std::path::PathBuf;

use anyhow::anyhow;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use serde::Deserialize;
use tokio_util::io::ReaderStream;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::dashboard;
use crate::detections;
use crate::error::{ViewerError, ViewerResult};
use crate::export;
use crate::playlist;
use crate::processing;
use crate::proxy;
use crate::remux::{self, Variant};
use crate::state::ViewerState;
use crate::timeline;

pub fn router(state: ViewerState) -> Router {
    let ui_dir = state.cfg.ui_dir.clone();

    // The login surface: reachable past the password gate but still behind the IP gate.
    let public = Router::new().route(
        "/login",
        get(crate::auth::login_page).post(crate::auth::login_submit),
    );

    // Everything else requires a valid session cookie (the password gate).
    let gated = Router::new()
        .route("/api/devices", get(devices))
        .route("/api/devices/{device_id}/timeline", get(get_timeline))
        .route("/api/devices/{device_id}/detections", get(get_detections))
        .route("/api/devices/{device_id}/processing", get(get_processing))
        // Footage export (streamed MP4 download); viewer-owned (ffmpeg + blob cache), not proxied.
        .route("/api/devices/{device_id}/export.mp4", get(export::export_mp4))
        // System dashboard: cameras + background-process status (one aggregating payload).
        .route("/api/dashboard", get(dashboard::get_dashboard))
        // Static segment "seg" takes priority over the {device_id} param for /hls/*.
        .route("/hls/seg/{seg}", get(segment_ts))
        .route("/hls/{device_id}/{name}", get(get_playlist))
        // Browser capture uploads → hushai-backend `POST /v1/segments` (token injected,
        // large body). Dedicated route so it never falls into the `/v1/*` rag branch below.
        .route("/api/capture/segments", post(proxy::forward_capture))
        // Reverse-proxy the chat/RAG API to hushai-rag (one origin; see proxy.rs).
        .route("/v1/{*rest}", any(proxy::forward))
        .route("/logout", post(crate::auth::logout))
        .fallback_service(ServeDir::new(ui_dir).append_index_html_on_directories(true))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_session,
        ));

    // The IP allowlist wraps BOTH the login surface and the gated app (but not /healthz).
    let protected = public.merge(gated).layer(axum::middleware::from_fn_with_state(
        state.clone(),
        crate::auth::ip_allowlist,
    ));

    Router::new()
        // Liveness probe: outside both gates so external monitors + the dashboard's own
        // server-side probes work without being in the allowlist (returns only "ok").
        .route("/healthz", get(|| async { "ok" }))
        .merge(protected)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[derive(Debug, Deserialize)]
pub struct WindowParams {
    pub from: Option<i64>,
    pub to: Option<i64>,
}

/// Clamp a *playable* window so a single playlist can't blow up. Unbounded windows
/// (no `to`) are left alone — the UI always supplies a bounded window.
fn clamp_window(from: i64, to: i64, max: i64) -> (i64, i64) {
    if to != i64::MAX && to > from && to.saturating_sub(from) > max {
        (to - max, to)
    } else {
        (from, to)
    }
}

// --- JSON API ------------------------------------------------------------------

#[derive(serde::Serialize)]
struct DevicesResponse {
    devices: Vec<timeline::DeviceSummary>,
}

async fn devices(State(state): State<ViewerState>) -> ViewerResult<Json<DevicesResponse>> {
    let devices = timeline::list_devices(&state.pool).await?;
    Ok(Json(DevicesResponse { devices }))
}

async fn get_timeline(
    State(state): State<ViewerState>,
    Path(device_id): Path<String>,
    Query(p): Query<WindowParams>,
) -> ViewerResult<Json<timeline::TimelineResponse>> {
    let from = p.from.unwrap_or(0);
    let to = p.to.unwrap_or(i64::MAX);
    let rows = timeline::windowed_segments(&state.pool, &device_id, from, to).await?;
    Ok(Json(timeline::build_timeline(&device_id, from, to, &rows)))
}

async fn get_detections(
    State(state): State<ViewerState>,
    Path(device_id): Path<String>,
    Query(p): Query<WindowParams>,
) -> ViewerResult<Json<detections::DetectionsResponse>> {
    let from = p.from.unwrap_or(0);
    let to = p.to.unwrap_or(i64::MAX);
    // Same 6h clamp the playlists use, so an unbounded detections request can't run away.
    let (from, to) = clamp_window(from, to, state.cfg.max_window_nanos);
    let resp = detections::windowed_detections(
        &state.pool,
        &device_id,
        from,
        to,
        state.cfg.detections_max_rows,
    )
    .await?;
    Ok(Json(resp))
}

async fn get_processing(
    State(state): State<ViewerState>,
    Path(device_id): Path<String>,
    Query(p): Query<WindowParams>,
) -> ViewerResult<Json<processing::ProcessingResponse>> {
    let from = p.from.unwrap_or(0);
    let to = p.to.unwrap_or(i64::MAX);
    // Same 6h clamp the playlists use, so an unbounded request can't run away.
    let (from, to) = clamp_window(from, to, state.cfg.max_window_nanos);
    let resp = processing::windowed_processing(
        &state.pool,
        &device_id,
        from,
        to,
        state.cfg.processing_max_rows,
    )
    .await?;
    Ok(Json(resp))
}

// --- HLS playlists -------------------------------------------------------------

async fn get_playlist(
    State(state): State<ViewerState>,
    Path((device_id, name)): Path<(String, String)>,
    Query(p): Query<WindowParams>,
) -> ViewerResult<Response> {
    let from = p.from.unwrap_or(0);
    let to = p.to.unwrap_or(i64::MAX);
    let (from, to) = clamp_window(from, to, state.cfg.max_window_nanos);

    match name.as_str() {
        "master.m3u8" => {
            let mts: Vec<(i32,)> = sqlx::query_as(
                r#"
                SELECT DISTINCT media_type
                FROM segments
                WHERE device_id = $1
                  AND capture_start_unix_nanos < $3
                  AND capture_start_unix_nanos + duration_nanos > $2
                "#,
            )
            .bind(&device_id)
            .bind(from)
            .bind(to)
            .fetch_all(&state.pool)
            .await?;
            let media_types: Vec<i32> = mts.into_iter().map(|r| r.0).collect();
            Ok(m3u8(playlist::master_playlist(&media_types, from, to)))
        }
        "video.m3u8" | "audio.m3u8" | "muxed.m3u8" => {
            let (media_type, variant) = match name.as_str() {
                "video.m3u8" => (2, "video"),
                "audio.m3u8" => (1, "audio"),
                _ => (3, "muxed"),
            };
            let rows = timeline::windowed_segments(&state.pool, &device_id, from, to).await?;
            let filtered: Vec<timeline::SegmentRow> = rows
                .into_iter()
                .filter(|r| r.media_type == media_type)
                .collect();
            Ok(m3u8(playlist::media_playlist(&filtered, variant)))
        }
        _ => Err(ViewerError::NotFound(format!("unknown playlist {name}"))),
    }
}

// --- HLS segments (remux on demand, cached) ------------------------------------

async fn segment_ts(
    State(state): State<ViewerState>,
    Path(seg): Path<String>,
) -> ViewerResult<Response> {
    // `seg` = "<sha>.<variant>.ts"
    let stem = seg
        .strip_suffix(".ts")
        .ok_or_else(|| ViewerError::BadRequest("expected .ts".into()))?;
    let (sha, variant_str) = stem
        .rsplit_once('.')
        .ok_or_else(|| ViewerError::BadRequest("expected <sha>.<variant>.ts".into()))?;
    let variant =
        Variant::parse(variant_str).ok_or_else(|| ViewerError::BadRequest("bad variant".into()))?;
    let path = remux::ensure_ts(&state, sha, variant).await?;
    serve_ts(path).await
}

// --- file/response helpers -----------------------------------------------------

async fn serve_ts(path: PathBuf) -> ViewerResult<Response> {
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| ViewerError::Internal(anyhow!("opening cached TS: {e}")))?;
    let len = file.metadata().await.map_err(anyhow::Error::from)?.len();
    let body = Body::from_stream(ReaderStream::new(file));
    Response::builder()
        .header(header::CONTENT_TYPE, "video/mp2t")
        .header(header::CONTENT_LENGTH, len)
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(body)
        .map_err(|e| ViewerError::Internal(anyhow!(e)))
}

fn m3u8(body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}
