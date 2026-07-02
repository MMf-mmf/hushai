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
use crate::sentiment;
use crate::state::ViewerState;
use crate::stills;
use crate::timeline;

pub fn router(state: ViewerState) -> Router {
    let ui_dir = state.cfg.ui_dir.clone();

    // The login surface: reachable past the password gate but still behind the IP gate.
    // The shared stylesheet is deliberately public too, so login.html can use the same
    // design system (CSS carries no secrets; every other UI asset stays session-gated).
    let public = Router::new()
        .route(
            "/login",
            get(crate::auth::login_page).post(crate::auth::login_submit),
        )
        .route("/styles.css", get(serve_styles));

    // Everything else requires a valid session cookie (the password gate).
    let gated = Router::new()
        .route("/api/devices", get(devices))
        .route("/api/devices/{device_id}/timeline", get(get_timeline))
        .route("/api/devices/{device_id}/detections", get(get_detections))
        .route("/api/devices/{device_id}/processing", get(get_processing))
        // Sentiment coverage (mood ribbon on the scrub bar).
        .route("/api/devices/{device_id}/sentiment", get(get_sentiment))
        // Still frames (ffmpeg single-frame extraction, cached like the TS remux):
        // thumb.jpg backs the timeline hover preview, poster.jpg the camera-grid tiles.
        .route("/api/devices/{device_id}/thumb.jpg", get(thumb_jpg))
        .route("/api/devices/{device_id}/poster.jpg", get(poster_jpg))
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
        // Liveness/readiness/metrics: outside both gates so external monitors + Prometheus + the
        // dashboard's own server-side probes work without being in the allowlist (roadmap B1/B5).
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(hushai_backend::observe::metrics_handler))
        .merge(protected)
        // Structured per-request access log with a correlatable `request_id` (see crate::logging).
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(hushai_backend::logging::make_http_span)
                .on_response(hushai_backend::logging::on_http_response),
        )
        .with_state(state)
}

/// The shared stylesheet, served past the password gate (see `router`). Route-match wins
/// over the gated `ServeDir` fallback, so authed pages keep fetching the same URL.
async fn serve_styles(State(state): State<ViewerState>) -> Response {
    match tokio::fs::read(state.cfg.ui_dir.join("styles.css")).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "text/css; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

/// Readiness (roadmap B5): the DB the viewer reads is reachable.
async fn readyz(State(state): State<crate::state::ViewerState>) -> axum::http::StatusCode {
    match sqlx::query("SELECT 1").execute(&state.pool).await {
        Ok(_) => axum::http::StatusCode::OK,
        Err(_) => axum::http::StatusCode::SERVICE_UNAVAILABLE,
    }
}

#[derive(Debug, Deserialize)]
pub struct WindowParams {
    pub from: Option<i64>,
    pub to: Option<i64>,
}

/// Clamp a *playable* window so a single playlist can't blow up. Unbounded windows
/// (no `to`) are left alone — the UI always supplies a bounded window.
pub(crate) fn clamp_window(from: i64, to: i64, max: i64) -> (i64, i64) {
    // Resolve an unbounded upper bound (`to` omitted → i64::MAX) to "now" first, so the
    // max-window cap ALSO applies to the default unbounded request — otherwise the clamp
    // was a no-op for exactly the request it exists to bound (whole-history scan).
    let to = if to == i64::MAX {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(i64::MAX)
    } else {
        to
    };
    if to > from && to.saturating_sub(from) > max {
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
    // Same 6h clamp the detections/processing/playlist routes use, so an unbounded
    // timeline request can't load a device's entire history into one JSON body.
    let (from, to) = clamp_window(from, to, state.cfg.max_window_nanos);
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

async fn get_sentiment(
    State(state): State<ViewerState>,
    Path(device_id): Path<String>,
    Query(p): Query<WindowParams>,
) -> ViewerResult<Json<sentiment::SentimentResponse>> {
    let from = p.from.unwrap_or(0);
    let to = p.to.unwrap_or(i64::MAX);
    // Same 6h clamp the playlists use, so an unbounded request can't run away.
    let (from, to) = clamp_window(from, to, state.cfg.max_window_nanos);
    let resp = sentiment::windowed_sentiment(
        &state.pool,
        &device_id,
        from,
        to,
        state.cfg.processing_max_rows,
    )
    .await?;
    Ok(Json(resp))
}

// --- still frames (hover thumbs + grid posters) ----------------------------------

#[derive(Debug, Deserialize)]
pub struct StillParams {
    pub t: Option<i64>,
}

/// Timeline hover preview: the frame at wall-clock `t` (unix nanos). Content-addressed —
/// the URL's `t` maps to an immutable segment, so long cache + ETag revalidation.
async fn thumb_jpg(
    State(state): State<ViewerState>,
    Path(device_id): Path<String>,
    Query(p): Query<StillParams>,
    headers: axum::http::HeaderMap,
) -> ViewerResult<Response> {
    let t = p
        .t
        .ok_or_else(|| ViewerError::BadRequest("missing t (unix nanos)".into()))?;
    let sha = stills::video_sha_at(&state.pool, &device_id, t)
        .await?
        .ok_or_else(|| ViewerError::NotFound("no video at that instant".into()))?;
    serve_still(&state, &headers, &sha, stills::THUMB_W, "public, max-age=86400").await
}

/// Camera-grid tile: the newest frame (or the frame at `t` when given). "Latest" moves
/// as footage arrives, so cache briefly and revalidate by ETag.
async fn poster_jpg(
    State(state): State<ViewerState>,
    Path(device_id): Path<String>,
    Query(p): Query<StillParams>,
    headers: axum::http::HeaderMap,
) -> ViewerResult<Response> {
    let sha = match p.t {
        Some(t) => stills::video_sha_at(&state.pool, &device_id, t).await?,
        None => stills::latest_video_sha(&state.pool, &device_id).await?,
    }
    .ok_or_else(|| ViewerError::NotFound("no video for device".into()))?;
    serve_still(&state, &headers, &sha, stills::POSTER_W, "private, max-age=5").await
}

async fn serve_still(
    state: &ViewerState,
    headers: &axum::http::HeaderMap,
    sha: &str,
    width: u32,
    cache_control: &str,
) -> ViewerResult<Response> {
    let etag = format!("\"{sha}.{width}\"");
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        == Some(etag.as_str())
    {
        return Response::builder()
            .status(axum::http::StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .header(header::CACHE_CONTROL, cache_control)
            .body(Body::empty())
            .map_err(|e| ViewerError::Internal(anyhow!(e)));
    }
    let path = stills::ensure_still(state, sha, width).await?;
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| ViewerError::Internal(anyhow!("reading cached still: {e}")))?;
    Response::builder()
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CONTENT_LENGTH, bytes.len())
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::ETAG, etag)
        .body(Body::from(bytes))
        .map_err(|e| ViewerError::Internal(anyhow!(e)))
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
