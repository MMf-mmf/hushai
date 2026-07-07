//! Ahithophel advisor service: multi-agent advice pipeline grounded in an ingested book.
//!
//! `POST /v1/advisor/chat` runs a per-turn state machine (see `pipeline.rs`):
//! sufficiency gate (Min-Info + Yenta) → question refinement → chapter routing
//! (Traffic Controller) → draft → critique → bounded refine loop → streamed final
//! answer → Q&A memory. Sessions persist in `advisor_sessions`/`advisor_messages`
//! (migration 0027); the book corpus lives in `books`/`book_chapters`/`book_chunks`
//! (migration 0026, populated by the `ingest-book` binary).
//!
//! Modules are public so integration tests can exercise the pieces directly.

pub mod books;
pub mod chat;
pub mod clean;
pub mod config;
pub mod embed;
pub mod ingest;
pub mod llm;
pub mod memory;
pub mod pipeline;
pub mod routes;
pub mod state;

use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

use crate::config::AdvisorConfig;
use crate::embed::Embedder;
use crate::llm::Llm;
use crate::state::AppState;

/// Honours `RUST_LOG` / `LOG_FORMAT` / `LOG_DIR` via the shared [`hushai_backend::logging`] module.
pub fn init_tracing() {
    hushai_backend::logging::init("hushai-advisor", "info,hushai_advisor=debug");
}

/// Build the router for a given state (kept separate so tests can mount it too).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(hushai_backend::observe::metrics_handler))
        .route("/v1/advisor/chat", post(chat::advisor_chat))
        .route("/v1/advisor/sessions", get(chat::list_sessions))
        .route(
            "/v1/advisor/sessions/{id}/messages",
            get(chat::list_messages),
        )
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(hushai_backend::logging::make_http_span)
                .on_response(hushai_backend::logging::on_http_response),
        )
        .with_state(state)
}

/// Readiness: DB reachable. 503 when not, so a load balancer can drain.
async fn readyz(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::http::StatusCode {
    match sqlx::query("SELECT 1").execute(&state.pool).await {
        Ok(_) => axum::http::StatusCode::OK,
        Err(_) => axum::http::StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Load config, connect, and serve until Ctrl-C / SIGTERM.
pub async fn run() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    dotenvy::from_path("hushai-backend/.env").ok();
    init_tracing();

    hushai_backend::observe::record_build_info("advisor");
    hushai_backend::observe::describe(
        "hushai_advisor_requests_total",
        "counter",
        "Advisor requests by endpoint(chat).",
    );

    let cfg = AdvisorConfig::from_env()?;

    // Reuse the backend's Config + pool builder (path dependency).
    let backend_cfg = hushai_backend::config::Config::from_env()
        .context("loading shared backend config (DATABASE_URL/BLOB_DIR/DEVICE_TOKEN)")?;
    let pool = hushai_backend::db::connect(&backend_cfg)
        .await
        .context("connecting to Postgres")?;

    // Ensure the advisor migrations (0026/0027, incl. the HNSW indexes) are applied.
    sqlx::migrate!("../hushai-backend/migrations")
        .run(&pool)
        .await
        .context("running migrations")?;

    let embedder = Arc::new(Embedder::new(&cfg.embed_ollama_base_url, &cfg.embed_model)?);
    let llm = Arc::new(Llm::new(
        &cfg.llm_ollama_base_url,
        &cfg.llm_model,
        cfg.judge_model.as_deref(),
        cfg.llm_temperature,
        cfg.llm_seed,
        cfg.llm_num_ctx,
    )?);
    let bind_addr = cfg.bind_addr;
    let tls = cfg.tls.clone();
    // Redacted startup banner (no ADVISOR_TOKEN / DB credentials).
    tracing::info!(
        service = "hushai-advisor",
        version = env!("CARGO_PKG_VERSION"),
        %bind_addr,
        tls = tls.is_some(),
        llm_model = %cfg.llm_model,
        judge_model = %cfg.judge_model.as_deref().unwrap_or("(same as llm_model)"),
        embed_model = %cfg.embed_model,
        num_ctx = cfg.llm_num_ctx,
        logging = %hushai_backend::logging::summary(),
        "starting"
    );
    if cfg.advisor_token.is_none() {
        // Fail closed: the advisor archive is the user's most intimate data (personal
        // situations, finances, relationships) — an unauthenticated non-loopback bind
        // would make it world-readable on the LAN. Allow only loopback, or an explicit
        // opt-in for trusted networks. Same posture as hushai-rag's RAG_TOKEN check.
        let insecure_ok = std::env::var("ADVISOR_ALLOW_INSECURE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if !bind_addr.ip().is_loopback() && !insecure_ok {
            anyhow::bail!(
                "refusing to start: ADVISOR_TOKEN is unset while binding a non-loopback address \
                 ({bind_addr}) — the advisor archive (personal consultations) would be \
                 UNAUTHENTICATED and world-open on the LAN. Set ADVISOR_TOKEN, bind 127.0.0.1, \
                 or set ADVISOR_ALLOW_INSECURE=true to override."
            );
        }
        tracing::warn!(
            "ADVISOR_TOKEN is unset — the advisor service is UNAUTHENTICATED (allowed: loopback \
             or ADVISOR_ALLOW_INSECURE). Set ADVISOR_TOKEN so it isn't world-open on the LAN."
        );
    }

    let state = AppState {
        pool,
        embedder,
        llm,
        cfg: Arc::new(cfg),
        inflight: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
    };

    let app = router(state);
    tracing::info!(%bind_addr, tls = tls.is_some(), "hushai-advisor listening");
    hushai_backend::tls::serve(bind_addr, app, tls, shutdown_signal()).await
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}
