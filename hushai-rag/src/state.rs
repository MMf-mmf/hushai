//! Shared, cheaply-cloned application state for the RAG service.

use std::sync::Arc;

use sqlx::PgPool;

use crate::clip_text::ClipTextEmbedder;
use crate::config::RagConfig;
use crate::embed::Embedder;
use crate::llm::Llm;
use crate::tts::Tts;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub embedder: Arc<Embedder>,
    pub llm: Arc<Llm>,
    pub cfg: Arc<RagConfig>,
    /// Local Kokoro TTS engine. `None` when TTS is disabled or the model is absent;
    /// `/v1/tts` then returns 503 and the assistant simply shows the answer text.
    pub tts: Option<Arc<Tts>>,
    /// CLIP TEXT tower for open-vocab object retrieval. `None` when disabled or the model/tokenizer
    /// are absent; the `objects` agent then returns 503 (object queries unavailable).
    pub clip: Option<Arc<ClipTextEmbedder>>,
}
