//! Local text embeddings via Rig's Ollama provider.
//!
//! The schema column is `vector(1024)`, so [`EMBED_DIM`] is a hard constraint:
//! every vector is checked before it can be written. The default model is
//! `mxbai-embed-large` (1024-dim, fully local). Vectors come back as `f64` and are
//! narrowed to `f32` for pgvector.

use anyhow::{Context, anyhow};
use rig::embeddings::EmbeddingModel as _;
use rig::providers::ollama;

/// Required embedding dimensionality (matches `transcript_sentences.embedding vector(1024)`).
pub const EMBED_DIM: usize = 1024;

#[derive(Clone)]
pub struct Embedder {
    model: ollama::EmbeddingModel,
    name: String,
}

impl Embedder {
    pub fn new(ollama_base_url: &str, model: &str) -> anyhow::Result<Self> {
        let client = ollama::Client::builder()
            .api_key(ollama::OllamaApiKey::default()) // local Ollama: no auth
            .base_url(ollama_base_url)
            .build()
            .with_context(|| format!("building Ollama client for {ollama_base_url}"))?;
        let embedding_model = ollama::EmbeddingModel::new(client, model, EMBED_DIM);
        Ok(Self {
            model: embedding_model,
            name: model.to_string(),
        })
    }

    /// The model name to persist in `embedding_model`.
    pub fn model_name(&self) -> &str {
        &self.name
    }

    /// Embed many texts, returning 1024-dim f32 vectors in input order.
    pub async fn embed(&self, texts: Vec<String>) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let n = texts.len();
        let embeddings = self
            .model
            .embed_texts(texts)
            .await
            .map_err(|e| anyhow!("embedding request failed: {e}"))?;
        if embeddings.len() != n {
            return Err(anyhow!(
                "embedding provider returned {} vectors for {} inputs",
                embeddings.len(),
                n
            ));
        }
        let mut out = Vec::with_capacity(embeddings.len());
        for e in embeddings {
            let v: Vec<f32> = e.vec.iter().map(|x| *x as f32).collect();
            check_dim(&v)?;
            out.push(v);
        }
        Ok(out)
    }

    /// Embed a single text (used by the RAG query path).
    pub async fn embed_one(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut v = self.embed(vec![text.to_string()]).await?;
        v.pop()
            .ok_or_else(|| anyhow!("empty embedding response for a single input"))
    }
}

/// Enforce the 1024-dim hard constraint before any DB write.
pub fn check_dim(v: &[f32]) -> anyhow::Result<()> {
    if v.len() != EMBED_DIM {
        return Err(anyhow!(
            "embedding has dim {}, expected {EMBED_DIM}",
            v.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dim_guard_accepts_1024_rejects_others() {
        assert!(check_dim(&vec![0.0f32; EMBED_DIM]).is_ok());
        assert!(check_dim(&vec![0.0f32; 768]).is_err());
        assert!(check_dim(&[]).is_err());
    }
}
