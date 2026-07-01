//! Shared, cheaply-cloned application state for the viewer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sqlx::PgPool;
use tokio::sync::Semaphore;

use crate::config::ViewerConfig;

#[derive(Clone)]
pub struct ViewerState {
    pub pool: PgPool,
    pub cfg: Arc<ViewerConfig>,
    /// Shared HTTP client used to reverse-proxy `/v1/*` to hushai-rag (cheap to clone).
    pub http: reqwest::Client,
    /// Bounds total concurrent ffmpeg remuxes (a fast scrub must not fork-bomb).
    pub ffmpeg_sem: Arc<Semaphore>,
    /// Bounds concurrent footage EXPORTS, SEPARATELY from `ffmpeg_sem`: an export holds one permit
    /// for its whole long-lived stream AND internally calls remux (which takes `ffmpeg_sem`), so
    /// sharing one semaphore would let an export deadlock its own per-segment remuxes.
    pub export_sem: Arc<Semaphore>,
    /// Per-cache-key single-flight locks so concurrent requests for the same segment
    /// remux it once. Keyed by `<sha>.<variant>`; entries are short-lived.
    pub inflight: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl ViewerState {
    /// Get (or create) the single-flight lock for a cache key.
    pub fn inflight_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.inflight.lock().unwrap();
        map.entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Drop a single-flight entry once a remux finishes. The caller still holds one
    /// `Arc` clone, and the map holds one, so a count `<= 2` means no other waiter is
    /// parked on this key and it is safe to prune (keeps the map ~empty at rest).
    pub fn inflight_release(&self, key: &str) {
        let mut map = self.inflight.lock().unwrap();
        if let Some(lock) = map.get(key) {
            if Arc::strong_count(lock) <= 2 {
                map.remove(key);
            }
        }
    }
}
