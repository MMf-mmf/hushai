//! Hushai RAG service: single-turn, grounded Q&A over transcribed segments.
//!
//! `POST /v1/rag/query` embeds the query, runs a pgvector nearest-neighbour search
//! over `transcript_sentences`, feeds the retrieved passages to a Rig LLM with a
//! grounding instruction, and returns the answer plus real source citations.
//!
//! Modules are public so integration tests can exercise retrieval directly.

pub mod config;
pub mod embed;
pub mod llm;
pub mod retrieve;
pub mod routes;
pub mod state;

use std::sync::Arc;

use anyhow::Context;
use axum::routing::{get, post};
use axum::Router;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::RagConfig;
use crate::embed::Embedder;
use crate::llm::Llm;
use crate::state::AppState;

pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hushai_rag=debug"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Build the router for a given state (kept separate so tests can mount it too).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/rag/query", post(routes::rag_query))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Load config, connect, and serve until Ctrl-C / SIGTERM.
pub async fn run() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    dotenvy::from_path("hushai-backend/.env").ok();
    init_tracing();

    let cfg = RagConfig::from_env()?;

    // Reuse the backend's Config + pool builder (path dependency).
    let backend_cfg = hushai_backend::config::Config::from_env()
        .context("loading shared backend config (DATABASE_URL/BLOB_DIR/DEVICE_TOKEN)")?;
    let pool = hushai_backend::db::connect(&backend_cfg)
        .await
        .context("connecting to Postgres")?;

    // Ensure the phase-2 migration (incl. the HNSW index used by retrieval) is applied.
    sqlx::migrate!("../hushai-backend/migrations")
        .run(&pool)
        .await
        .context("running migrations")?;

    let embedder = Arc::new(Embedder::new(&cfg.embed_ollama_base_url, &cfg.embed_model)?);
    let llm = Arc::new(Llm::new(&cfg.llm_ollama_base_url, &cfg.rag_llm_model)?);
    let bind_addr = cfg.bind_addr;

    tracing::info!(
        embed_ollama = %cfg.embed_ollama_base_url,
        llm_ollama = %cfg.llm_ollama_base_url,
        embed_model = %cfg.embed_model,
        llm_model = %cfg.rag_llm_model,
        "rag endpoints resolved (embed/LLM independently configurable)"
    );

    let state = AppState {
        pool,
        embedder,
        llm,
        cfg: Arc::new(cfg),
    };

    let app = router(state);
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;
    tracing::info!(%bind_addr, "hushai-rag listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
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
