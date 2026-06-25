//! pgvector nearest-neighbour retrieval over `transcript_sentences`.
//!
//! Uses cosine distance (`<=>`) to match the HNSW `vector_cosine_ops` index and the
//! normalized mxbai/bge embeddings. Optional filters (device, time window) are
//! appended dynamically with a `QueryBuilder` and applied on `transcript_sentences`'
//! own denormalized `device_id` / `start_unix_nanos` columns — i.e. the same table as
//! the HNSW index, so the planner can combine the filter with the vector scan instead
//! of post-filtering a global ANN walk (the metadata-filter recall cliff).

use pgvector::Vector;
use sqlx::{AssertSqlSafe, PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

/// A retrieved sentence + its similarity, returned to the client as a citation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Source {
    pub segment_id: Uuid,
    pub device_id: String,
    pub text: String,
    pub start_unix_nanos: i64,
    pub distance: f64,
}

/// Optional narrowing of the search space.
#[derive(Debug, Default, Clone)]
pub struct Filters {
    pub device_id: Option<String>,
    pub after_unix_nanos: Option<i64>,
    pub before_unix_nanos: Option<i64>,
}

/// Per-query HNSW/timeout knobs applied via `SET LOCAL` on the retrieval transaction.
#[derive(Debug, Clone)]
pub struct Tuning {
    /// `hnsw.ef_search`. Effective value is raised to at least `top_k`.
    pub ef_search: i64,
    /// `statement_timeout` for the retrieval transaction, in milliseconds (0 = none).
    pub statement_timeout_ms: i64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            ef_search: 100,
            statement_timeout_ms: 10_000,
        }
    }
}

/// Return the `top_k` nearest sentences to `query_embedding`, closest first.
pub async fn nearest(
    pool: &PgPool,
    query_embedding: &[f32],
    top_k: i64,
    tuning: &Tuning,
    filters: &Filters,
) -> anyhow::Result<Vec<Source>> {
    // Bind the query embedding as a native pgvector::Vector (sqlx 0.9 binary protocol)
    // instead of a `[..]::vector` decimal text literal — no client-side decimal
    // formatting and no server-side text re-parse. Cloned because it's bound twice
    // (the SELECT distance projection and the ORDER BY).
    let qvec = Vector::from(query_embedding.to_vec());

    // `SET LOCAL` is transaction-scoped, so these GUCs never leak onto a pooled
    // connection. iterative_scan lets a *filtered* ANN walk keep probing the HNSW
    // graph until it fills `top_k` (fixing the metadata-filter recall cliff);
    // strict_order keeps results in exact cosine-distance order.
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL hnsw.iterative_scan = 'strict_order'")
        .execute(&mut *tx)
        .await?;
    // ef_search / statement_timeout are GUCs and can't be bound — format validated ints.
    // sqlx 0.9 requires a non-'static query string be asserted injection-safe; these are
    // built only from our own i64s (no user input), so AssertSqlSafe is sound here.
    let ef_search = tuning.ef_search.max(top_k).max(1);
    sqlx::query(AssertSqlSafe(format!("SET LOCAL hnsw.ef_search = {ef_search}")))
        .execute(&mut *tx)
        .await?;
    let timeout_ms = tuning.statement_timeout_ms.max(0);
    sqlx::query(AssertSqlSafe(format!("SET LOCAL statement_timeout = {timeout_ms}")))
        .execute(&mut *tx)
        .await?;

    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT ts.segment_id, ts.device_id, ts.text, ts.start_unix_nanos, (ts.embedding <=> ",
    );
    qb.push_bind(qvec.clone());
    qb.push(
        ") AS distance \
         FROM transcript_sentences ts \
         WHERE ts.embedding IS NOT NULL",
    );

    if let Some(device_id) = &filters.device_id {
        qb.push(" AND ts.device_id = ").push_bind(device_id.clone());
    }
    if let Some(after) = filters.after_unix_nanos {
        qb.push(" AND ts.start_unix_nanos >= ").push_bind(after);
    }
    if let Some(before) = filters.before_unix_nanos {
        qb.push(" AND ts.start_unix_nanos < ").push_bind(before);
    }

    qb.push(" ORDER BY ts.embedding <=> ").push_bind(qvec);
    qb.push(" LIMIT ").push_bind(top_k);

    let rows = qb.build().fetch_all(&mut *tx).await?;
    tx.commit().await?;

    let mut sources = Vec::with_capacity(rows.len());
    for row in rows {
        sources.push(Source {
            segment_id: row.try_get("segment_id")?,
            device_id: row
                .try_get::<Option<String>, _>("device_id")?
                .unwrap_or_default(),
            text: row.try_get::<Option<String>, _>("text")?.unwrap_or_default(),
            start_unix_nanos: row.try_get("start_unix_nanos")?,
            distance: row.try_get("distance")?,
        });
    }
    Ok(sources)
}
