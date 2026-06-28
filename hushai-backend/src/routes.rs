//! Router wiring and health endpoints.
//!
//! Ingest-only concerns (body limit, bearer auth) are layered on the
//! `/v1/segments` sub-router so the health probes stay unauthenticated and
//! unbounded. Cross-cutting concerns (tracing, timeout) wrap everything.

use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::auth;
use crate::db;
use crate::ingest;
use crate::persons;
use crate::speakers;
use crate::state::AppState;
use crate::storage;

pub fn router(state: AppState) -> Router {
    let max_body = state.config.max_body_bytes;
    let request_timeout = Duration::from_secs(state.config.request_timeout_secs);

    let ingest = Router::new()
        .route("/v1/segments", post(ingest::post_segment))
        .layer(DefaultBodyLimit::max(max_body))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));

    // Speaker catalog read/admin surface — bearer-authenticated (same as ingest), kept off
    // the unauthenticated health merge.
    let speakers = Router::new()
        .route("/v1/speakers", get(speakers::list_speakers))
        .route("/v1/speakers/recluster", post(speakers::recluster))
        .route(
            "/v1/speakers/recluster-deep",
            post(speakers::recluster_deep),
        )
        .route("/v1/speakers/duplicates", get(speakers::list_duplicates))
        .route("/v1/speakers/merge-group", post(speakers::merge_group))
        // Literal `/unattributed*` routes must precede `/{id}` so they aren't captured as ids.
        .route(
            "/v1/speakers/unattributed",
            get(speakers::list_unattributed),
        )
        .route(
            "/v1/speakers/unattributed/name",
            post(speakers::name_unattributed),
        )
        .route(
            "/v1/speakers/unattributed/sample-audio",
            get(speakers::sample_audio_segment),
        )
        .route("/v1/speakers/{id}", patch(speakers::rename_speaker))
        .route("/v1/speakers/{id}/merge", post(speakers::merge_speaker))
        .route(
            "/v1/speakers/{id}/sample-audio",
            get(speakers::sample_audio),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));

    // Person (face) catalog read/admin surface — the visual sibling of `speakers`,
    // bearer-authenticated the same way.
    let persons = Router::new()
        .route("/v1/persons", get(persons::list_persons))
        .route("/v1/persons/{id}", patch(persons::rename_person))
        .route("/v1/persons/{id}/merge", post(persons::merge_person))
        .route("/v1/persons/{id}/sample-face", get(persons::sample_face))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .merge(ingest)
        .merge(speakers)
        .merge(persons)
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            request_timeout,
        ))
        .with_state(state)
}

/// Liveness: the process is up.
async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Readiness: DB reachable AND blob volume writable with headroom. Returns 503
/// when not ready (the same checks back the ingest path's 507 signal).
async fn readyz(State(state): State<AppState>) -> StatusCode {
    if db::readiness(&state.pool).await.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    let enough_space = storage::free_space_bytes(&state.blob_root)
        .map(|free| free >= state.config.disk_watermark_bytes)
        .unwrap_or(false);
    if !enough_space {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    StatusCode::OK
}
