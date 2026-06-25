//! The per-segment pipeline: resolve media -> ASR -> chunk -> embed -> atomic write.
//!
//! The DB write is idempotent per source segment: in one transaction we DELETE any
//! existing `transcript_sentences` for the segment, insert the freshly-computed rows,
//! and mark the status `done`. Re-processing replaces, never duplicates; a no-speech
//! segment writes zero sentences and is still marked `done`.

use anyhow::Context;
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::asr::Transcriber;
use crate::chunk::{self, Sentence};
use crate::config::WorkerConfig;
use crate::embed::{self, Embedder};
use crate::media;

/// Run the full pipeline for one claimed segment. Returns the number of sentences written.
pub async fn process_segment(
    pool: &PgPool,
    transcriber: &Transcriber,
    embedder: &Embedder,
    cfg: &WorkerConfig,
    segment_id: Uuid,
) -> anyhow::Result<usize> {
    let seg = media::load_segment(pool, segment_id).await?;
    let pcm = media::extract_pcm(cfg, &seg).await?;
    let utterances = transcriber.transcribe(pcm).await?;
    let sentences = chunk::chunk_into_sentences(&utterances, seg.capture_start_unix_nanos);

    let texts: Vec<String> = sentences.iter().map(|s| s.text.clone()).collect();
    let embeddings = embedder.embed(texts).await?;

    write_transcript(
        pool,
        segment_id,
        &seg.device_id,
        &sentences,
        &embeddings,
        embedder.model_name(),
    )
    .await?;

    Ok(sentences.len())
}

/// Max rows per batched INSERT. Each row binds 8 params; chunking keeps us well under
/// Postgres' 65535 bind-parameter limit even for an implausibly long segment.
const ROWS_PER_INSERT: usize = 1000;

/// Atomically replace a segment's transcript and mark it `done`. Idempotent:
/// running twice with the same input leaves the same rows (delete-then-insert).
///
/// `device_id` is denormalized onto each sentence so the RAG query can filter on the
/// same table as the HNSW index. `sentences` and `embeddings` must be the same length
/// and aligned by index.
pub async fn write_transcript(
    pool: &PgPool,
    segment_id: Uuid,
    device_id: &str,
    sentences: &[Sentence],
    embeddings: &[Vec<f32>],
    embed_model: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        sentences.len() == embeddings.len(),
        "sentence/embedding count mismatch: {} vs {}",
        sentences.len(),
        embeddings.len()
    );
    // Validate every vector up front so a bad dim fails before we touch the DB.
    for emb in embeddings {
        embed::check_dim(emb)?;
    }

    let mut tx = pool.begin().await.context("begin transcript tx")?;

    sqlx::query("DELETE FROM transcript_sentences WHERE segment_id = $1")
        .bind(segment_id)
        .execute(&mut *tx)
        .await
        .context("clearing prior sentences")?;

    // One multi-row INSERT per chunk (one round-trip) instead of one INSERT per sentence.
    for (s_chunk, e_chunk) in sentences
        .chunks(ROWS_PER_INSERT)
        .zip(embeddings.chunks(ROWS_PER_INSERT))
    {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            "INSERT INTO transcript_sentences \
             (segment_id, device_id, text, start_unix_nanos, end_unix_nanos, \
              embedding, embedding_model, embedding_dim) ",
        );
        qb.push_values(s_chunk.iter().zip(e_chunk.iter()), |mut b, (s, emb)| {
            // Bind the embedding as a native pgvector::Vector (sqlx 0.9 binary protocol)
            // rather than a `[..]::vector` decimal text literal — less CPU + bandwidth
            // per row and no server-side re-parse. Encoding is byte-equivalent.
            b.push_bind(segment_id)
                .push_bind(device_id)
                .push_bind(&s.text)
                .push_bind(s.start_unix_nanos)
                .push_bind(s.end_unix_nanos)
                .push_bind(pgvector::Vector::from(emb.clone()))
                .push_bind(embed_model)
                .push_bind(embed::EMBED_DIM as i32);
        });
        qb.build()
            .execute(&mut *tx)
            .await
            .context("inserting sentences")?;
    }

    sqlx::query(
        r#"
        UPDATE segment_transcription_status
           SET status = 'done', last_error = NULL, updated_at = now()
         WHERE segment_id = $1
        "#,
    )
    .bind(segment_id)
    .execute(&mut *tx)
    .await
    .context("marking status done")?;

    tx.commit().await.context("commit transcript tx")?;
    Ok(())
}
