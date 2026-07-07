//! Long-term Q&A memory (spec agents 9/10): store a summary of every delivered answer,
//! retrieve the nearest past consultations into new ones.
//!
//! Deliberately a SEMANTIC store (embed + cosine top-k), not a deterministic fold: it
//! answers "what did we discuss like this before?". A future deterministic *user profile*
//! ("married, two kids, runs a business") should follow the `entity_profiles` fold
//! pattern (hushai-backend/src/profiles.rs, migration 0024) rather than growing this
//! table's role.

use pgvector::Vector;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::books::ChapterRef;
use crate::embed::Embedder;

#[derive(Debug, Clone)]
pub struct Memory {
    pub question: String,
    pub answer_summary: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Nearest past consultations to the refined question (cosine top-k + distance cutoff).
pub async fn retrieve_memories(
    pool: &PgPool,
    embedding: &[f32],
    top_k: i64,
    distance_threshold: f64,
) -> anyhow::Result<Vec<Memory>> {
    let rows = sqlx::query(
        "SELECT question, answer_summary, created_at, (embedding <=> $1) AS distance \
         FROM advisor_memories \
         WHERE embedding IS NOT NULL AND (embedding <=> $1) <= $3 \
         ORDER BY embedding <=> $1 \
         LIMIT $2",
    )
    .bind(Vector::from(embedding.to_vec()))
    .bind(top_k.max(1))
    .bind(distance_threshold)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Memory {
            question: r.get("question"),
            answer_summary: r.get("answer_summary"),
            created_at: r.get("created_at"),
        })
        .collect())
}

/// Persist one consultation's memory row. Best-effort at the call site — a failure here
/// must never fail the turn that already delivered its answer.
pub async fn store_memory(
    pool: &PgPool,
    embedder: &Embedder,
    embed_model: &str,
    session_id: Uuid,
    question: &str,
    answer_summary: &str,
    chapters: &[ChapterRef],
) -> anyhow::Result<()> {
    let embedding = embedder
        .embed_one(&format!("{question}\n{answer_summary}"))
        .await?;
    let dim = embedding.len() as i32;
    sqlx::query(
        "INSERT INTO advisor_memories \
           (memory_id, session_id, question, answer_summary, chapters, embedding, \
            embedding_model, embedding_dim) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(Uuid::now_v7())
    .bind(session_id)
    .bind(question)
    .bind(answer_summary)
    .bind(sqlx::types::Json(chapters))
    .bind(Vector::from(embedding))
    .bind(embed_model)
    .bind(dim)
    .execute(pool)
    .await?;
    Ok(())
}

/// Render retrieved memories as the bounded "Past context" block injected into the
/// routing + draft prompts. Empty string when there are none.
pub fn render_memory_context(memories: &[Memory]) -> String {
    if memories.is_empty() {
        return String::new();
    }
    let mut out = String::from("Past consultations (earlier advice given to this person):\n");
    for m in memories {
        out.push_str(&format!(
            "- [{}] {} — {}\n",
            m.created_at.format("%Y-%m-%d"),
            m.question,
            m.answer_summary
        ));
    }
    out
}
