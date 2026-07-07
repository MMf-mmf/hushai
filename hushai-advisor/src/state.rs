//! Shared, cheaply-cloned application state for the advisor service.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use sqlx::PgPool;
use uuid::Uuid;

use crate::config::AdvisorConfig;
use crate::embed::Embedder;
use crate::llm::Llm;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub embedder: Arc<Embedder>,
    pub llm: Arc<Llm>,
    pub cfg: Arc<AdvisorConfig>,
    /// Sessions with a turn currently in flight. A consultation turn runs 30–90s of
    /// sequential phase-machine writes with no DB-level serialization (the 0008 seq
    /// UNIQUE only catches same-instant inserts), so a second concurrent turn on the
    /// same session would reset/clobber the live turn's phase state — it is rejected
    /// with 409 instead (chat.rs). In-process is sufficient: the advisor is a single
    /// service instance.
    pub inflight: Arc<Mutex<HashSet<Uuid>>>,
}
