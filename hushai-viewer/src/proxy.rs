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

/// Forward any method on `/v1/*` to the right upstream (rag or backend), streaming back.
pub async fn forward(State(state): State<ViewerState>, req: Request) -> Response {
    match forward_inner(state, req).await {
        Ok(resp) => resp,
        Err(msg) => {
            tracing::warn!(error = %msg, "proxy failed");
            (StatusCode::BAD_GATEWAY, msg).into_response()
        }
    }
}

/// `/v1/speakers*` and `/v1/persons*` are hushai-backend's catalog-admin surfaces (voices and
/// faces); everything else is hushai-rag.
fn is_backend_path(path: &str) -> bool {
    path == "/v1/speakers"
        || path.starts_with("/v1/speakers/")
        || path == "/v1/persons"
        || path.starts_with("/v1/persons/")
}

async fn forward_inner(state: ViewerState, req: Request) -> Result<Response, String> {
    let method = req.method().clone();
    // Pick the upstream (base URL + bearer) by path before consuming the request.
    let (base_url, token) = if is_backend_path(req.uri().path()) {
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
