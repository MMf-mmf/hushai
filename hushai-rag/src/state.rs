//! Shared, cheaply-cloned application state for the RAG service.

use std::sync::Arc;

use sqlx::PgPool;

use crate::config::RagConfig;
use crate::embed::Embedder;
use crate::llm::Llm;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub embedder: Arc<Embedder>,
    pub llm: Arc<Llm>,
    pub cfg: Arc<RagConfig>,
}
