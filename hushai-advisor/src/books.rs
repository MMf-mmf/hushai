//! Read-side queries over the ingested book corpus (migration 0026).

use pgvector::Vector;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// A chapter citation, persisted as jsonb on final answers and memories and surfaced in
/// the SSE `chapters` event ([{no, title}]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChapterRef {
    pub no: i32,
    pub title: Option<String>,
}

/// A chapter's routing card: what the Traffic Controller sees.
#[derive(Debug, Clone)]
pub struct ChapterCard {
    pub chapter_no: i32,
    pub title: Option<String>,
    pub synopsis: Option<String>,
}

/// A chapter's full text for the draft prompt.
#[derive(Debug, Clone)]
pub struct ChapterText {
    pub chapter_no: i32,
    pub title: Option<String>,
    pub clean_text: String,
}

/// A chunk hit from the semantic candidate-widening search.
#[derive(Debug, Clone)]
pub struct ChunkHit {
    pub chapter_no: i32,
    pub content: String,
    pub distance: f64,
}

/// The single ingested book the advisor consults (v1: exactly one). `None` until
/// `ingest-book` has run.
pub async fn default_book(pool: &PgPool) -> anyhow::Result<Option<(Uuid, String)>> {
    let row = sqlx::query("SELECT book_id, title FROM books ORDER BY created_at ASC LIMIT 1")
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| (r.get("book_id"), r.get("title"))))
}

/// All chapter routing cards for a book, in chapter order (50 × ~200 chars — one prompt).
pub async fn load_cards(pool: &PgPool, book_id: Uuid) -> anyhow::Result<Vec<ChapterCard>> {
    let rows = sqlx::query(
        "SELECT chapter_no, title, synopsis FROM book_chapters \
         WHERE book_id = $1 ORDER BY chapter_no ASC",
    )
    .bind(book_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ChapterCard {
            chapter_no: r.get("chapter_no"),
            title: r.get("title"),
            synopsis: r.get("synopsis"),
        })
        .collect())
}

/// Full cleaned texts for the given chapter numbers, in chapter order.
pub async fn load_chapters(
    pool: &PgPool,
    book_id: Uuid,
    chapter_nos: &[i32],
) -> anyhow::Result<Vec<ChapterText>> {
    let rows = sqlx::query(
        "SELECT chapter_no, title, clean_text FROM book_chapters \
         WHERE book_id = $1 AND chapter_no = ANY($2) ORDER BY chapter_no ASC",
    )
    .bind(book_id)
    .bind(chapter_nos)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ChapterText {
            chapter_no: r.get("chapter_no"),
            title: r.get("title"),
            clean_text: r.get("clean_text"),
        })
        .collect())
}

/// Nearest chunks to a query embedding (cosine, HNSW). Used to widen the Traffic
/// Controller's candidate set (anti-tunnel-vision) and to represent an over-budget
/// chapter by its most relevant excerpts.
pub async fn nearest_chunks(
    pool: &PgPool,
    book_id: Uuid,
    embedding: &[f32],
    top_k: i64,
    chapter_no: Option<i32>,
) -> anyhow::Result<Vec<ChunkHit>> {
    let mut tx = pool.begin().await?;
    // Filtered-ANN recall guard (the metadata-filter recall cliff — same rationale as
    // hushai-rag/src/retrieve.rs): a filtered HNSW walk stops after ef_search candidates
    // and post-filters, so a chapter_no filter over ~250 chunks can return FEWER than
    // LIMIT rows — including zero. iterative_scan keeps walking until LIMIT rows pass
    // the filter; the ef_search floor widens the initial candidate pool.
    sqlx::query("SET LOCAL hnsw.iterative_scan = 'strict_order'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL hnsw.ef_search = 100")
        .execute(&mut *tx)
        .await?;
    let rows = sqlx::query(
        "SELECT c.chapter_no, k.content, (k.embedding <=> $2) AS distance \
         FROM book_chunks k \
         JOIN book_chapters c ON c.chapter_id = k.chapter_id \
         WHERE c.book_id = $1 \
           AND k.embedding IS NOT NULL \
           AND ($4::int IS NULL OR c.chapter_no = $4) \
         ORDER BY k.embedding <=> $2 \
         LIMIT $3",
    )
    .bind(book_id)
    .bind(Vector::from(embedding.to_vec()))
    .bind(top_k.max(1))
    .bind(chapter_no)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(|r| ChunkHit {
            chapter_no: r.get("chapter_no"),
            content: r.get("content"),
            distance: r.get("distance"),
        })
        .collect())
}

/// A chapter's leading chunks in reading order — the fallback excerpt for an
/// over-budget chapter when the semantic search returns nothing for it.
pub async fn first_chunks(
    pool: &PgPool,
    book_id: Uuid,
    chapter_no: i32,
    limit: i64,
) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT k.content FROM book_chunks k \
         JOIN book_chapters c ON c.chapter_id = k.chapter_id \
         WHERE c.book_id = $1 AND c.chapter_no = $2 \
         ORDER BY k.seq ASC LIMIT $3",
    )
    .bind(book_id)
    .bind(chapter_no)
    .bind(limit.max(1))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.get("content")).collect())
}

/// Corpus readiness probe: chunk count for the startup/readiness hint and the eval
/// precondition ("run ingest-book" when 0).
pub async fn chunk_count(pool: &PgPool) -> anyhow::Result<i64> {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM book_chunks")
        .fetch_one(pool)
        .await?;
    Ok(n)
}
