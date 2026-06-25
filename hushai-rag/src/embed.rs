//! Query embedding via Rig's Ollama provider — the SAME model + 1024-dim space the
//! worker used to embed sentences, so query/document vectors are comparable.
//!
//! (This mirrors `hushai-worker`'s embedder; kept local to keep `hushai-backend`
//! free of a Rig dependency. A shared `hushai-embed` crate is a future refactor.)

use anyhow::{Context, anyhow};
use rig::embeddings::EmbeddingModel as _;
use rig::providers::ollama;

pub const EMBED_DIM: usize = 1024;

#[derive(Clone)]
pub struct Embedder {
    model: ollama::EmbeddingModel,
}

impl Embedder {
    pub fn new(ollama_base_url: &str, model: &str) -> anyhow::Result<Self> {
        let client = ollama::Client::builder()
            .api_key(ollama::OllamaApiKey::default())
            .base_url(ollama_base_url)
            .build()
            .with_context(|| format!("building Ollama client for {ollama_base_url}"))?;
        Ok(Self {
            model: ollama::EmbeddingModel::new(client, model, EMBED_DIM),
        })
    }

    /// Embed a single query string into a 1024-dim f32 vector.
    pub async fn embed_one(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let embeddings = self
            .model
            .embed_texts(vec![text.to_string()])
            .await
            .map_err(|e| anyhow!("query embedding failed: {e}"))?;
        let first = embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("empty embedding response"))?;
        let v: Vec<f32> = first.vec.iter().map(|x| *x as f32).collect();
        if v.len() != EMBED_DIM {
            return Err(anyhow!("query embedding dim {} != {EMBED_DIM}", v.len()));
        }
        Ok(v)
    }
}
