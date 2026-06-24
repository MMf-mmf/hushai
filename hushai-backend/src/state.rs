//! Shared application state, cheaply cloned per request.

use std::path::Path;
use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::Semaphore;

use crate::auth::TokenStore;
use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    /// Postgres connection pool.
    pub pool: PgPool,
    /// Canonical, absolute blob root (`{BLOB_DIR}` after `canonicalize`).
    pub blob_root: Arc<Path>,
    /// Accepted device tokens (phase 1: a single configured token).
    pub tokens: Arc<TokenStore>,
    /// Global concurrency limiter; a permit is held for each in-flight ingest.
    pub limiter: Arc<Semaphore>,
    /// Full configuration.
    pub config: Arc<Config>,
}
