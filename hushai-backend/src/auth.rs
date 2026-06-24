//! Bearer-token auth seam.
//!
//! Phase 1 validates the token against a configured allowlist (a single
//! `DEVICE_TOKEN`). The token→device resolution lives here so real per-device
//! issuance (a `device_tokens` table lookup) drops in without touching the
//! ingest handler.

use std::collections::HashSet;

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;

use crate::error::IngestError;
use crate::state::AppState;

/// Set of accepted device tokens.
#[derive(Debug, Default)]
pub struct TokenStore {
    allowed: HashSet<String>,
}

impl TokenStore {
    /// Phase-1 store with a single accepted token.
    pub fn single(token: String) -> Self {
        let mut allowed = HashSet::new();
        allowed.insert(token);
        Self { allowed }
    }

    pub fn is_allowed(&self, token: &str) -> bool {
        self.allowed.contains(token)
    }
}

/// Resolved identity attached to the request after successful auth. Carries the
/// authenticated token today; gains a stable `device_id` once issuance is real.
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    pub token: String,
}

/// Middleware: require a valid `Authorization: Bearer <token>` or return 401.
pub async fn require_bearer(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, IngestError> {
    // Own the token so the immutable borrow of `req` ends before we mutate it.
    let token = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);

    match token {
        Some(t) if state.tokens.is_allowed(&t) => {
            req.extensions_mut().insert(DeviceIdentity { token: t });
            Ok(next.run(req).await)
        }
        _ => Err(IngestError::Unauthorized),
    }
}
