//! Hushai RAG service: single-turn, grounded Q&A over transcribed segments.
//!
//! `POST /v1/rag/query` embeds the query, runs a pgvector nearest-neighbour search
//! over `transcript_sentences`, feeds the retrieved passages to a Rig LLM with a
//! grounding instruction, and returns the answer plus real source citations.
//!
//! Modules are public so integration tests can exercise retrieval directly.

pub mod agents;
pub mod analytics;
pub mod chat;
pub mod clip_text;
pub mod config;
pub mod context;
pub mod embed;
pub mod humanize;
pub mod llm;
pub mod persons;
pub mod plates;
pub mod presence;
pub mod retrieve;
pub mod routes;
pub mod speakers;
pub mod state;
pub mod stats;
pub mod timeparse;
pub mod tts;

use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

use crate::clip_text::ClipTextEmbedder;
use crate::config::RagConfig;
use crate::embed::Embedder;
use crate::llm::Llm;
use crate::state::AppState;
use crate::tts::Tts;

/// Honours `RUST_LOG` / `LOG_FORMAT` / `LOG_DIR` via the shared [`hushai_backend::logging`] module.
pub fn init_tracing() {
    hushai_backend::logging::init("hushai-rag", "info,hushai_rag=debug");
}

/// Build the router for a given state (kept separate so tests can mount it too).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(hushai_backend::observe::metrics_handler))
        .route("/v1/rag/query", post(routes::rag_query))
        .route("/v1/rag/chat", post(chat::rag_chat))
        .route("/v1/rag/chat/sessions", get(chat::list_sessions))
        .route(
            "/v1/rag/chat/sessions/{id}/messages",
            get(chat::list_messages),
        )
        .route("/v1/rag/agents", get(chat::list_agents))
        .route("/v1/rag/conversations", get(routes::list_conversations_route))
        .route(
            "/v1/rag/conversations/{id}",
            get(routes::get_conversation_route),
        )
        .route("/v1/tts", post(routes::tts_synthesize))
        // Structured per-request access log with a correlatable `request_id` (see crate::logging).
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(hushai_backend::logging::make_http_span)
                .on_response(hushai_backend::logging::on_http_response),
        )
        .with_state(state)
}

/// Readiness (roadmap B5): DB reachable. 503 when not, so a load balancer / k8s can drain.
async fn readyz(axum::extract::State(state): axum::extract::State<AppState>) -> axum::http::StatusCode {
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

    // Observability (roadmap B1).
    hushai_backend::observe::record_build_info("rag");
    hushai_backend::observe::describe(
        "hushai_rag_requests_total",
        "counter",
        "RAG requests by endpoint(query|chat).",
    );

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
    let llm = Arc::new(Llm::new(
        &cfg.llm_ollama_base_url,
        &cfg.rag_llm_model,
        cfg.rag_llm_temperature,
        cfg.rag_llm_seed,
    )?);
    let bind_addr = cfg.bind_addr;
    let tls = cfg.tls.clone();
    // Redacted startup banner (no RAG_TOKEN / DB credentials).
    tracing::info!(
        service = "hushai-rag",
        version = env!("CARGO_PKG_VERSION"),
        %bind_addr,
        tls = tls.is_some(),
        llm_model = %cfg.rag_llm_model,
        embed_model = %cfg.embed_model,
        logging = %hushai_backend::logging::summary(),
        "starting"
    );
    if cfg.rag_token.is_none() {
        // Fail closed: an unauthenticated rag on a non-loopback bind is a world-open surveillance
        // archive (transcripts, who-with-whom, plates, reflection digest) — one POST from any LAN
        // host reads everything. Allow only loopback, or an explicit opt-in for trusted networks.
        let insecure_ok = std::env::var("RAG_ALLOW_INSECURE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if !bind_addr.ip().is_loopback() && !insecure_ok {
            anyhow::bail!(
                "refusing to start: RAG_TOKEN is unset while binding a non-loopback address \
                 ({bind_addr}) — the rag archive would be UNAUTHENTICATED and world-open on the \
                 LAN. Set RAG_TOKEN, bind 127.0.0.1, or set RAG_ALLOW_INSECURE=true to override."
            );
        }
        tracing::warn!(
            "RAG_TOKEN is unset — the rag service is UNAUTHENTICATED (allowed: loopback or \
             RAG_ALLOW_INSECURE). Set RAG_TOKEN so it isn't world-open on the LAN."
        );
    }

    // Load the local Kokoro TTS engine (warm for every /v1/tts request). A missing
    // model or load failure is non-fatal: log and serve without spoken answers.
    let tts = if cfg.tts_enabled {
        let (dir, sid, speed, threads) = (
            cfg.tts_dir.clone(),
            cfg.tts_sid,
            cfg.tts_speed,
            cfg.tts_threads,
        );
        match tokio::task::spawn_blocking(move || Tts::new(&dir, sid, speed, threads)).await {
            Ok(Ok(engine)) => Some(Arc::new(engine)),
            Ok(Err(e)) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "TTS disabled (engine load failed)"
                );
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "TTS disabled (load task panicked)");
                None
            }
        }
    } else {
        tracing::info!("TTS disabled via RAG_TTS_ENABLED=false");
        None
    };

    // Load the CLIP TEXT tower (open-vocab object retrieval). Like TTS, a missing model/tokenizer
    // or load failure is non-fatal: log and serve without object queries (the `objects` agent 503s).
    let clip = if cfg.clip_text_enabled {
        let (model, tok, dylib) = (
            cfg.clip_text_model_path.clone(),
            cfg.clip_tokenizer_path.clone(),
            cfg.ort_dylib_path.clone(),
        );
        match tokio::task::spawn_blocking(move || ClipTextEmbedder::new(&model, &tok, &dylib)).await
        {
            Ok(Ok(engine)) => {
                tracing::info!(
                    model = %cfg.clip_text_model_path,
                    "CLIP text tower loaded (open-vocab object retrieval enabled)"
                );
                Some(Arc::new(engine))
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "object retrieval disabled (CLIP text tower load failed)"
                );
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "object retrieval disabled (CLIP load task panicked)");
                None
            }
        }
    } else {
        tracing::info!("object retrieval disabled via RAG_OBJECTS_ENABLED=false");
        None
    };

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
        tts,
        clip,
    };

    let app = router(state);
    tracing::info!(%bind_addr, tls = tls.is_some(), "hushai-rag listening");
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
