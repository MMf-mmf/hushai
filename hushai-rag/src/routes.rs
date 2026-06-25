//! HTTP surface: `POST /v1/rag/query` (embed -> retrieve -> ground -> answer).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::retrieve::{self, Filters, Source, Tuning};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    pub query: String,
    #[serde(default)]
    pub top_k: Option<i64>,
    #[serde(default)]
    pub filters: Option<QueryFilters>,
}

#[derive(Debug, Deserialize, Default)]
pub struct QueryFilters {
    pub device_id: Option<String>,
    pub after_unix_nanos: Option<i64>,
    pub before_unix_nanos: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct QueryResponse {
    pub answer: String,
    pub sources: Vec<Source>,
}

pub async fn rag_query(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, (StatusCode, String)> {
    // Optional bearer auth (enforced only when RAG_TOKEN is configured).
    if let Some(expected) = &st.cfg.rag_token {
        let presented = headers
            .get(AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if presented != Some(expected.as_str()) {
            return Err((StatusCode::UNAUTHORIZED, "missing or invalid bearer token".into()));
        }
    }

    if req.query.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "query must not be empty".into()));
    }

    let top_k = req.top_k.unwrap_or(st.cfg.top_k_default).clamp(1, 50);
    let filters = req
        .filters
        .map(|f| Filters {
            device_id: f.device_id,
            after_unix_nanos: f.after_unix_nanos,
            before_unix_nanos: f.before_unix_nanos,
        })
        .unwrap_or_default();

    // 1. Embed the query in the same 1024-dim space as the stored sentences.
    let embedding = st.embedder.embed_one(&req.query).await.map_err(internal)?;

    // 2. pgvector nearest-neighbour retrieval.
    let tuning = Tuning {
        ef_search: st.cfg.hnsw_ef_search,
        statement_timeout_ms: st.cfg.query_timeout_ms,
    };
    let mut sources = retrieve::nearest(&st.pool, &embedding, top_k, &tuning, &filters)
        .await
        .map_err(internal)?;

    // 3. Drop weak matches so we neither ground on nor cite irrelevant passages.
    sources.retain(|s| s.distance <= st.cfg.distance_threshold);

    // 4. Ask the LLM for a grounded answer (declines when sources is empty).
    let answer = st.llm.answer(&req.query, &sources).await.map_err(internal)?;

    Ok(Json(QueryResponse { answer, sources }))
}

fn internal(e: anyhow::Error) -> (StatusCode, String) {
    tracing::error!(error = format!("{e:#}"), "rag query failed");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}
