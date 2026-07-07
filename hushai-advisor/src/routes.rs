//! Shared HTTP plumbing: bearer auth + the sanitized internal-error mapper.

use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use subtle::ConstantTimeEq;

use crate::state::AppState;

/// Optional bearer auth, enforced only when `ADVISOR_TOKEN` is configured.
pub(crate) fn check_auth(headers: &HeaderMap, st: &AppState) -> Result<(), (StatusCode, String)> {
    if let Some(expected) = &st.cfg.advisor_token {
        let presented = headers
            .get(AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        // Constant-time compare so the static token isn't recoverable byte-by-byte via a
        // timing side channel — matches the backend/rag auth. The length check isn't
        // itself secret; ct_eq is constant-time for equal lengths.
        let ok = match presented {
            Some(tok) => {
                tok.len() == expected.len() && bool::from(tok.as_bytes().ct_eq(expected.as_bytes()))
            }
            None => false,
        };
        if !ok {
            return Err((
                StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn internal(e: anyhow::Error) -> (StatusCode, String) {
    // Log the full chain server-side, but return a static body — `{e:#}` leaks sqlx
    // table/column names, SQL fragments, Ollama URLs and filesystem paths to any caller.
    tracing::error!(error = format!("{e:#}"), "advisor request failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal error".to_string(),
    )
}
