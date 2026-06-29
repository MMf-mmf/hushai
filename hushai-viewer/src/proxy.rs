//! Reverse proxy: forward `/v1/*` to the right backing service.
//!
//! Most of `/v1/*` is the chat/RAG API, proxied to **hushai-rag**. The speaker-admin
//! surface (`/v1/speakers*`) lives in **hushai-backend** instead (its own bind + token),
//! so we dispatch by path: one `/v1/*` route, two upstreams. A second axum wildcard at the
//! same position would conflict, hence the in-handler branch.
//!
//! Why proxy instead of letting the browser hit the services directly:
//!   - One origin ⇒ no CORS and SSE streaming "just works".
//!   - The bearer (`RAG_TOKEN` / `DEVICE_TOKEN`) is injected server-side, so the page never
//!     holds a secret.
//!
//! The upstream response BODY is streamed through unbuffered (`bytes_stream`), which is
//! what lets the chat `text/event-stream` deliver tokens incrementally to the client.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::state::ViewerState;

/// Max proxied request-body size. Chat/query payloads are tiny JSON; this is a guard.
const MAX_PROXY_BODY: usize = 1024 * 1024;

/// Max capture-upload body. A 2s muxed H.264 segment (~2 Mbps video + AAC) is well under
/// this; it stays below the backend's own 32 MiB `MAX_BODY_BYTES`. Much larger than
/// `MAX_PROXY_BODY` because media segments dwarf chat/admin JSON.
const MAX_CAPTURE_BODY: usize = 16 * 1024 * 1024;

/// Forward any method on `/v1/*` to the right upstream (rag or backend), streaming back.
pub async fn forward(State(state): State<ViewerState>, req: Request) -> Response {
    // Capture audit facets before the request is consumed. We audit only MUTATING requests to the
    // ADMIN surfaces (backend paths) — not high-volume rag chat/query or ingest — at the gateway.
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let audit_worthy =
        hushai_backend::audit::is_mutating(&method) && is_backend_path(&path);
    let client_ip = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().to_string());
    let actor = if state.cfg.auth_disabled { "local" } else { "admin" };
    let pool = state.pool.clone();

    let resp = match forward_inner(state, req).await {
        Ok(resp) => resp,
        Err(msg) => {
            tracing::warn!(error = %msg, "proxy failed");
            (StatusCode::BAD_GATEWAY, msg).into_response()
        }
    };

    // Gateway audit (roadmap B6): record the action + its outcome (incl. a BAD_GATEWAY failure).
    // Awaited inline (durable, no fire-and-forget loss) but bounded by record()'s internal timeout.
    if audit_worthy {
        let entry = hushai_backend::audit::AuditEntry::proxied(
            actor,
            client_ip,
            &method,
            &path,
            resp.status().as_u16(),
        );
        hushai_backend::audit::record(&pool, entry).await;
    }
    resp
}

/// Forward a browser capture upload (`POST /api/capture/segments`) to hushai-backend's
/// `POST /v1/segments`. A dedicated route (not the `/v1/*` branch in `forward`) because:
///   - the upstream is **always** the backend (never rag), and
///   - media bodies are large, so we use `MAX_CAPTURE_BODY` instead of the 1 MiB chat guard.
/// Same same-origin rationale as `forward`: the browser holds no token and triggers no CORS;
/// the backend `DEVICE_TOKEN` bearer is injected here, server-side.
pub async fn forward_capture(State(state): State<ViewerState>, req: Request) -> Response {
    match forward_capture_inner(state, req).await {
        Ok(resp) => resp,
        Err(msg) => {
            tracing::warn!(error = %msg, "capture proxy failed");
            (StatusCode::BAD_GATEWAY, msg).into_response()
        }
    }
}

async fn forward_capture_inner(state: ViewerState, req: Request) -> Result<Response, String> {
    let method = req.method().clone();
    let url = format!(
        "{}/v1/segments",
        state.cfg.backend_base_url.trim_end_matches('/')
    );

    // Preserve the multipart Content-Type **verbatim** — it carries the `boundary=` the
    // backend's `Multipart` extractor needs; reconstructing it would break parsing. Inject
    // the backend bearer ourselves so the page never holds the secret.
    let mut headers = HeaderMap::new();
    if let Some(v) = req.headers().get(header::CONTENT_TYPE) {
        headers.insert(header::CONTENT_TYPE, v.clone());
    }
    if let Some(token) = state.cfg.backend_token.as_ref() {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
            headers.insert(header::AUTHORIZATION, v);
        }
    }

    // Buffer the segment, bounded to 16 MiB. A 2s muxed H.264 segment is ~0.5 MB, so this is
    // a generous cap, not a memory concern; the backend separately enforces its 32 MiB limit.
    let body_bytes = axum::body::to_bytes(req.into_body(), MAX_CAPTURE_BODY)
        .await
        .map_err(|e| format!("reading capture body: {e}"))?;

    let upstream = state
        .http
        .request(method, &url)
        .headers(headers)
        .body(body_bytes.to_vec())
        .send()
        .await
        .map_err(|e| format!("upstream capture request to {url} failed: {e}"))?;

    // Pass the upstream status through verbatim: 200/401/422/429/507 are all
    // contract-meaningful to the capture client's retry logic.
    let status = upstream.status();
    let mut builder = Response::builder().status(status);
    if let Some(v) = upstream.headers().get(header::CONTENT_TYPE) {
        builder = builder.header(header::CONTENT_TYPE, v.clone());
    }
    let body = Body::from_stream(upstream.bytes_stream());
    builder
        .body(body)
        .map_err(|e| format!("building proxied capture response: {e}"))
}

/// `/v1/speakers*`, `/v1/persons*`, `/v1/plates*`, `/v1/devices*`, `/v1/events*`, and
/// `/v1/alert-rules*` are hushai-backend's catalog-/device-admin surfaces (voices, faces, license
/// plates, device management + footage deletion, and the events/alerts feed + rules); everything
/// else is hushai-rag.
fn is_backend_path(path: &str) -> bool {
    path == "/v1/speakers"
        || path.starts_with("/v1/speakers/")
        || path == "/v1/persons"
        || path.starts_with("/v1/persons/")
        || path == "/v1/plates"
        || path.starts_with("/v1/plates/")
        || path == "/v1/devices"
        || path.starts_with("/v1/devices/")
        || path == "/v1/events"
        || path.starts_with("/v1/events/")
        || path == "/v1/alert-rules"
        || path.starts_with("/v1/alert-rules/")
        || path == "/v1/audit"
        || path.starts_with("/v1/audit/")
        || path == "/v1/watchlist"
        || path.starts_with("/v1/watchlist/")
}

async fn forward_inner(state: ViewerState, req: Request) -> Result<Response, String> {
    let method = req.method().clone();
    // Pick the upstream (base URL + bearer) by path before consuming the request.
    let to_backend = is_backend_path(req.uri().path());
    hushai_backend::observe::counter(
        "hushai_viewer_proxy_total",
        &[("upstream", if to_backend { "backend" } else { "rag" })],
    );
    let (base_url, token) = if to_backend {
        (&state.cfg.backend_base_url, state.cfg.backend_token.as_ref())
    } else {
        (&state.cfg.rag_base_url, state.cfg.rag_token.as_ref())
    };
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let url = format!("{}{}", base_url.trim_end_matches('/'), path_and_query);

    // Forward only the headers the upstream cares about; inject the bearer ourselves.
    let mut headers = HeaderMap::new();
    for name in [header::CONTENT_TYPE, header::ACCEPT] {
        if let Some(v) = req.headers().get(&name) {
            headers.insert(name, v.clone());
        }
    }
    if let Some(token) = token {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
            headers.insert(header::AUTHORIZATION, v);
        }
    }

    // Buffer the (small) request body before forwarding.
    let body_bytes = axum::body::to_bytes(req.into_body(), MAX_PROXY_BODY)
        .await
        .map_err(|e| format!("reading request body: {e}"))?;

    let upstream = state
        .http
        .request(method, &url)
        .headers(headers)
        .body(body_bytes.to_vec())
        .send()
        .await
        .map_err(|e| format!("upstream request to {url} failed: {e}"))?;

    let status = upstream.status();
    let mut builder = Response::builder().status(status);
    // Preserve content-type (esp. `text/event-stream` for chat) and cache-control.
    for name in [header::CONTENT_TYPE, header::CACHE_CONTROL] {
        if let Some(v) = upstream.headers().get(&name) {
            builder = builder.header(name, v.clone());
        }
    }
    // Stream the body through unbuffered so SSE tokens flow as they arrive.
    let body = Body::from_stream(upstream.bytes_stream());
    builder
        .body(body)
        .map_err(|e| format!("building proxied response: {e}"))
}
