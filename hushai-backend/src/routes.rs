//! Router wiring and health endpoints.
//!
//! Ingest-only concerns (body limit, bearer auth) are layered on the
//! `/v1/segments` sub-router so the health probes stay unauthenticated and
//! unbounded. Cross-cutting concerns (tracing, timeout) wrap everything.

use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post, put};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::audit;
use crate::auth;
use crate::db;
use crate::devices;
use crate::events;
use crate::ingest;
use crate::persons;
use crate::plates;
use crate::speakers;
use crate::state::AppState;
use crate::storage;
use crate::watchlist;

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
        .route("/v1/speakers/{id}/archive", post(speakers::archive_speaker))
        .route(
            "/v1/speakers/{id}/unarchive",
            post(speakers::unarchive_speaker),
        )
        .route("/v1/speakers/{id}/owner", post(speakers::set_speaker_owner))
        .route(
            "/v1/speakers/{id}/unowner",
            post(speakers::clear_speaker_owner),
        )
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
        .route("/v1/persons/{id}/archive", post(persons::archive_person))
        .route("/v1/persons/{id}/unarchive", post(persons::unarchive_person))
        .route("/v1/persons/{id}/owner", post(persons::set_person_owner))
        .route("/v1/persons/{id}/unowner", post(persons::clear_person_owner))
        .route("/v1/persons/{id}/sample-face", get(persons::sample_face))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));

    // License-plate (ALPR) catalog read/admin surface — the vehicle sibling of `persons`,
    // bearer-authenticated the same way. The literal `/search` route is registered before the
    // bare `/{id}` so "search" isn't captured as a plate id.
    let plates = Router::new()
        .route("/v1/plates", get(plates::list_plates))
        .route("/v1/plates/search", get(plates::search_plates))
        .route("/v1/plates/{id}", patch(plates::rename_plate))
        .route("/v1/plates/{id}/merge", post(plates::merge_plate))
        .route("/v1/plates/{id}/archive", post(plates::archive_plate))
        .route("/v1/plates/{id}/unarchive", post(plates::unarchive_plate))
        .route(
            "/v1/plates/{id}/sample-crop",
            get(plates::sample_plate_crop),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));

    // Device management + footage deletion (rename, usage, retention, delete) — bearer-authed,
    // proxied through the viewer like the speaker/person surfaces. Literal sub-paths are registered
    // before the bare `/{device_id}` so they aren't captured as ids.
    let devices = Router::new()
        .route("/v1/devices", get(devices::list_devices))
        .route("/v1/devices/{device_id}/usage", get(devices::device_usage))
        .route("/v1/devices/{device_id}/retention", put(devices::set_retention))
        .route(
            "/v1/devices/{device_id}/footage/bulk-delete",
            post(devices::bulk_delete_footage),
        )
        .route(
            "/v1/devices/{device_id}/footage",
            delete(devices::delete_footage),
        )
        .route(
            "/v1/devices/{device_id}",
            patch(devices::rename_device).delete(devices::delete_device),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));

    // Events / alerts surface (roadmap Pillar A): the materialized event feed, the in-app
    // notification feed + ack, and alert-rule CRUD. Bearer-authed, proxied via the viewer like
    // devices/speakers. Literal `/feed*` routes precede nothing ambiguous; rule `/{id}` is last.
    let events = Router::new()
        .route("/v1/events", get(events::list_events))
        .route("/v1/events/feed", get(events::list_feed))
        .route(
            "/v1/events/feed/{delivery_id}/ack",
            post(events::ack_delivery),
        )
        .route(
            "/v1/alert-rules",
            get(events::list_rules).post(events::create_rule),
        )
        .route(
            "/v1/alert-rules/{rule_id}",
            patch(events::update_rule).delete(events::delete_rule),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));

    // Audit-log read surface (roadmap B6) — bearer-authed, proxied via the viewer like the others.
    let audit = Router::new()
        .route("/v1/audit", get(audit::list_audit))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));

    // Watchlist ("of interest") surface (roadmap A6) — bearer-authed, proxied via the viewer.
    let watchlist = Router::new()
        .route(
            "/v1/watchlist",
            get(watchlist::list_watchlist).post(watchlist::add_watch),
        )
        .route(
            "/v1/watchlist/{watch_id}",
            patch(watchlist::update_watch).delete(watchlist::remove_watch),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        // Prometheus scrape target (unauthenticated, like the health probes — roadmap B1).
        .route("/metrics", get(crate::observe::metrics_handler))
        .merge(ingest)
        .merge(speakers)
        .merge(persons)
        .merge(plates)
        .merge(devices)
        .merge(events)
        .merge(audit)
        .merge(watchlist)
        // One structured access line per request, carrying a generated `request_id` that every
        // handler log inherits (see crate::logging) — so a request is traceable end-to-end.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(crate::logging::make_http_span)
                .on_response(crate::logging::on_http_response),
        )
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
