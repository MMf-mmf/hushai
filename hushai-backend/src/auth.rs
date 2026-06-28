//! Bearer-token auth seam.
//!
//! Validates the presented token against a configured allowlist. The allowlist can be
//! a single `DEVICE_TOKEN` (back-compat) or a per-device `DEVICE_TOKENS` map of
//! `label:token` entries, so a lost/decommissioned device can be revoked individually
//! (drop its entry + restart). The token→device resolution lives here so real
//! per-device issuance (a `device_tokens` table lookup) drops in without touching the
//! ingest handler.

use std::collections::HashMap;

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use subtle::ConstantTimeEq;

use crate::error::IngestError;
use crate::state::AppState;

/// Accepted device tokens → the device label they authorise. A label is a human tag
/// for logs/attribution; revocation = removing a token from the source + restart.
#[derive(Debug, Default)]
pub struct TokenStore {
    /// token -> device label (e.g. "galaxy-s8", "cam-A").
    allowed: HashMap<String, String>,
}

impl TokenStore {
    /// Back-compat: a single accepted token, labelled "default".
    pub fn single(token: String) -> Self {
        let mut allowed = HashMap::new();
        allowed.insert(token, "default".to_string());
        Self { allowed }
    }

    /// Parse `DEVICE_TOKENS`: comma-separated `label:token` entries (or a bare `token`,
    /// labelled by its index). Blank/whitespace entries are skipped. An entry whose
    /// token half is empty (e.g. `label:`) is treated as a bare token of `label`.
    pub fn from_spec(spec: &str) -> Self {
        let mut allowed = HashMap::new();
        for (i, raw) in spec.split(',').enumerate() {
            let entry = raw.trim();
            if entry.is_empty() {
                continue;
            }
            match entry.split_once(':') {
                Some((label, tok)) if !tok.trim().is_empty() => {
                    allowed.insert(tok.trim().to_string(), label.trim().to_string());
                }
                _ => {
                    allowed.insert(entry.to_string(), format!("device-{i}"));
                }
            }
        }
        Self { allowed }
    }

    /// Constant-time membership: compare the presented token against every accepted
    /// token without an early exit, returning the matched device label. Avoids the
    /// timing oracle a plain `HashMap`/`HashSet` lookup would leak on the secret.
    pub fn resolve(&self, presented: &str) -> Option<&str> {
        let pb = presented.as_bytes();
        let mut hit: Option<&str> = None;
        for (tok, label) in &self.allowed {
            // `ct_eq` is constant-time for equal-length inputs; the length itself isn't
            // secret (tokens are a fixed random format). We still scan every entry so
            // the number of comparisons doesn't depend on which (if any) token matched.
            if tok.len() == pb.len() && bool::from(tok.as_bytes().ct_eq(pb)) {
                hit = Some(label.as_str());
            }
        }
        hit
    }

    /// Back-compat shim for existing callers/tests.
    pub fn is_allowed(&self, token: &str) -> bool {
        self.resolve(token).is_some()
    }
}

/// Resolved identity attached to the request after successful auth. Carries the
/// authenticated token + the device label it resolved to (logs/attribution); gains a
/// stable `device_id` once issuance is real.
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    pub token: String,
    pub device_label: String,
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
        Some(t) => match state.tokens.resolve(&t) {
            Some(label) => {
                let device_label = label.to_string();
                req.extensions_mut().insert(DeviceIdentity {
                    token: t,
                    device_label,
                });
                Ok(next.run(req).await)
            }
            None => Err(IngestError::Unauthorized),
        },
        None => Err(IngestError::Unauthorized),
    }
}

#[cfg(test)]
mod tests {
    use super::TokenStore;

    #[test]
    fn single_token_back_compat() {
        let store = TokenStore::single("dev-secret-token".into());
        assert_eq!(store.resolve("dev-secret-token"), Some("default"));
        assert!(store.is_allowed("dev-secret-token"));
        assert_eq!(store.resolve("nope"), None);
        assert!(!store.is_allowed("nope"));
    }

    #[test]
    fn multi_token_label_and_revocation() {
        // Two devices accepted, each with its own label; a third token is rejected.
        let store = TokenStore::from_spec("phone:TOK_A, replay:TOK_B");
        assert_eq!(store.resolve("TOK_A"), Some("phone"));
        assert_eq!(store.resolve("TOK_B"), Some("replay"));
        assert_eq!(store.resolve("TOK_C"), None);

        // "Revoking" phone = dropping its entry from the spec (operator edits + restarts).
        let revoked = TokenStore::from_spec("replay:TOK_B");
        assert_eq!(revoked.resolve("TOK_A"), None);
        assert_eq!(revoked.resolve("TOK_B"), Some("replay"));
    }

    #[test]
    fn from_spec_handles_bare_tokens_and_blanks() {
        // Bare tokens get an indexed label; whitespace/empty entries are skipped.
        let store = TokenStore::from_spec("BARE0, ,  named:TOK1 ,");
        assert_eq!(store.resolve("BARE0"), Some("device-0"));
        assert_eq!(store.resolve("TOK1"), Some("named"));
        assert_eq!(store.resolve(""), None);
    }

    #[test]
    fn resolve_rejects_length_mismatch_and_empty() {
        let store = TokenStore::single("abcdef".into());
        assert_eq!(store.resolve("abcde"), None); // shorter
        assert_eq!(store.resolve("abcdefg"), None); // longer
        assert_eq!(store.resolve("abcdef"), Some("default"));
    }
}
