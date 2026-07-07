//! One-shot, idempotent book ingest: chapter texts → clean → synopsize → chunk → embed.
//!
//! Driven by the `ingest-book` binary. Re-running is safe: the book row upserts by slug,
//! chapters upsert by (book_id, chapter_no), and a chapter's chunks are deleted+reinserted
//! only when its clean text or the embedding model changed (the 0001 generation-tracking
//! idea applied at ingest time). Chapters are processed sequentially — Ollama is shared
//! with the worker/rag services, so the ingest deliberately queues rather than floods.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use pgvector::Vector;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::clean::{Cleaned, chunk_paragraphs, clean_chapter};
use crate::embed::Embedder;
use crate::llm::Llm;

pub struct IngestOpts {
    /// Directory holding `1.txt` … `N.txt` chapter files.
    pub dir: String,
    pub slug: String,
    pub title: String,
    pub author: Option<String>,
    /// Run the guarded LLM copy-edit pass after the deterministic heuristics.
    pub llm_clean: bool,
    /// Paragraph-packing chunk target (chars).
    pub chunk_target_chars: usize,
    /// Recorded on every chunk row (generation tracking).
    pub embed_model: String,
}

#[derive(Debug, Default)]
pub struct IngestReport {
    pub chapters_seen: usize,
    pub chapters_updated: usize,
    pub chapters_skipped: usize,
    pub chunks_embedded: usize,
    pub llm_clean_fallbacks: usize,
}

pub async fn ingest_book(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    llm: &Arc<Llm>,
    opts: &IngestOpts,
) -> anyhow::Result<IngestReport> {
    let chapters = read_chapter_files(Path::new(&opts.dir))?;
    if chapters.is_empty() {
        return Err(anyhow!("no N.txt chapter files found in {}", opts.dir));
    }

    let book_id: Uuid = {
        let row = sqlx::query(
            "INSERT INTO books (book_id, slug, title, author, source_path) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (slug) DO UPDATE \
                SET title = EXCLUDED.title, author = EXCLUDED.author, \
                    source_path = EXCLUDED.source_path \
             RETURNING book_id",
        )
        .bind(Uuid::now_v7())
        .bind(&opts.slug)
        .bind(&opts.title)
        .bind(&opts.author)
        .bind(&opts.dir)
        .fetch_one(pool)
        .await
        .context("upserting book")?;
        row.get("book_id")
    };

    // Running-head strip patterns: the book title repeats at page tops inside chapter
    // text (a lone "Yes!" line mid-chapter). The full title plus its comma/colon parts
    // cover both recto/verso head variants.
    let strip_owned: Vec<String> = std::iter::once(opts.title.trim().to_string())
        .chain(opts.title.split([',', ':']).map(|p| p.trim().to_string()))
        .filter(|p| p.len() > 1)
        .collect();
    let strip_lines: Vec<&str> = strip_owned.iter().map(String::as_str).collect();

    let mut report = IngestReport::default();
    for (chapter_no, raw) in &chapters {
        report.chapters_seen += 1;
        let Cleaned { title, body } = clean_chapter(raw, &strip_lines);
        let clean_text = if opts.llm_clean {
            match llm.clean_ocr(&body).await {
                Some(t) => t,
                None => {
                    report.llm_clean_fallbacks += 1;
                    tracing::warn!(chapter_no, "LLM clean rejected by guard; keeping heuristic text");
                    body.clone()
                }
            }
        } else {
            body.clone()
        };

        // Skip unchanged chapters whose chunks are already on the current embedding
        // model AND whose synopsis exists — a rerun must repair a chapter whose
        // synopsis came back empty, not skip it forever.
        let existing = sqlx::query(
            "SELECT c.chapter_id, c.clean_text, \
                    COALESCE(c.synopsis, '') <> '' AS has_synopsis, \
                    (SELECT count(*) FROM book_chunks k \
                      WHERE k.chapter_id = c.chapter_id AND k.embedding_model = $3) AS current_chunks \
             FROM book_chapters c WHERE c.book_id = $1 AND c.chapter_no = $2",
        )
        .bind(book_id)
        .bind(chapter_no)
        .bind(&opts.embed_model)
        .fetch_optional(pool)
        .await?;
        if let Some(r) = &existing {
            let same_text: String = r.get("clean_text");
            let has_synopsis: bool = r.get("has_synopsis");
            let current_chunks: i64 = r.get("current_chunks");
            if same_text == clean_text && has_synopsis && current_chunks > 0 {
                report.chapters_skipped += 1;
                continue;
            }
        }

        let synopsis = llm
            .synopsize(title.as_deref(), &clean_text)
            .await
            .with_context(|| format!("synopsizing chapter {chapter_no}"))?;

        // Embed BEFORE the transaction (the slow, fallible part), then commit the
        // chapter row and its chunks TOGETHER: a mid-run failure must never leave a
        // chapter whose text says one thing and whose chunks embed another — the skip
        // check above would then see matching clean_text + populated chunks and skip
        // the stale chapter forever.
        let chunks = chunk_paragraphs(&clean_text, opts.chunk_target_chars);
        let mut embedded: Vec<(i32, &String, Vec<f32>)> = Vec::with_capacity(chunks.len());
        for (seq, content) in chunks.iter().enumerate() {
            let v = embedder
                .embed_one(content)
                .await
                .with_context(|| format!("embedding chapter {chapter_no} chunk {seq}"))?;
            embedded.push((seq as i32, content, v));
        }
        let mut tx = pool.begin().await?;
        let chapter_id: Uuid = sqlx::query(
            "INSERT INTO book_chapters (chapter_id, book_id, chapter_no, title, synopsis, raw_text, clean_text) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (book_id, chapter_no) DO UPDATE \
                SET title = EXCLUDED.title, synopsis = EXCLUDED.synopsis, \
                    raw_text = EXCLUDED.raw_text, clean_text = EXCLUDED.clean_text \
             RETURNING chapter_id",
        )
        .bind(Uuid::now_v7())
        .bind(book_id)
        .bind(chapter_no)
        .bind(&title)
        .bind(&synopsis)
        .bind(raw)
        .bind(&clean_text)
        .fetch_one(&mut *tx)
        .await
        .with_context(|| format!("upserting chapter {chapter_no}"))?
        .get("chapter_id");
        sqlx::query("DELETE FROM book_chunks WHERE chapter_id = $1")
            .bind(chapter_id)
            .execute(&mut *tx)
            .await?;
        for (seq, content, v) in embedded {
            sqlx::query(
                "INSERT INTO book_chunks \
                   (chunk_id, chapter_id, seq, content, embedding, embedding_model, embedding_dim) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(Uuid::now_v7())
            .bind(chapter_id)
            .bind(seq)
            .bind(content)
            .bind(Vector::from(v.clone()))
            .bind(&opts.embed_model)
            .bind(v.len() as i32)
            .execute(&mut *tx)
            .await?;
            report.chunks_embedded += 1;
        }
        tx.commit().await?;
        report.chapters_updated += 1;
        tracing::info!(chapter_no, title = title.as_deref().unwrap_or(""), "chapter ingested");
    }
    Ok(report)
}

/// Load `N.txt` files from `dir`, sorted by chapter number.
fn read_chapter_files(dir: &Path) -> anyhow::Result<Vec<(i32, String)>> {
    let mut out: Vec<(i32, String)> = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if ext != "txt" {
            continue;
        }
        let Ok(chapter_no) = stem.parse::<i32>() else {
            continue;
        };
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        out.push((chapter_no, raw));
    }
    out.sort_by_key(|(n, _)| *n);
    Ok(out)
}
