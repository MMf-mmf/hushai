//! System dashboard: one aggregating endpoint (`GET /api/dashboard`) that the bundled
//! `/dashboard.html` page polls. The browser can't reach the sibling services (CORS +
//! localhost-only binds), so the viewer fans out the DB queries and HTTP `/healthz`
//! probes server-side, concurrently, and returns a single JSON document:
//!
//! ```jsonc
//! { generated_at, cameras[], camera_summary, queues{transcription,vision}, services[] }
//! ```
//!
//! All queries are runtime `sqlx::query*` (no `.sqlx/` cache — the worker/viewer house style).
//! Worker liveness is hybrid: the `worker_heartbeat` row is authoritative (idle != dead); the
//! `segment_*_status` queues give backlog/throughput/errors. If the heartbeat table is missing
//! (un-migrated worker) the worker degrades to queue inference and reports `unknown`, never a
//! false `up`.

use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::PgPool;

use crate::config::ViewerConfig;
use crate::error::ViewerResult;
use crate::state::ViewerState;

// ---------------------------------------------------------------------------
// Response shape (field names are the contract for ui/js/dashboard/dashboard.js)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct DashboardResponse {
    pub generated_at: String,
    pub cameras: Vec<CameraStatus>,
    pub camera_summary: CameraSummary,
    pub queues: Queues,
    pub services: Vec<ServiceStatus>,
    /// Live capacity/load-test status (hushai-loadtest writes `live.json`; pointed at via
    /// `$VIEWER_LOADTEST_LIVE_JSON`). Absent when no run is active, so the existing UI is unaffected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loadtest: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct CameraStatus {
    pub device_id: String,
    /// Operator-assigned friendly name (management surface); `null` until renamed.
    pub display_name: Option<String>,
    pub source_kind: String,
    /// "connected" | "idle" | "offline" (by `last_seen` recency — server receipt time).
    pub state: String,
    pub last_seen: String,
    pub last_seen_age_secs: i64,
    pub first_seen: String,
    pub segment_count: i64,
    pub session_count: i64,
    pub has_video: bool,
    pub has_audio: bool,
    pub has_muxed: bool,
    pub first_capture_unix_nanos: Option<i64>,
    pub last_capture_unix_nanos: Option<i64>,
}

#[derive(Debug, Serialize, Default)]
pub struct CameraSummary {
    pub connected: i64,
    pub idle: i64,
    pub offline: i64,
    pub total: i64,
}

#[derive(Debug, Serialize)]
pub struct Queues {
    pub transcription: QueueStats,
    pub vision: QueueStats,
}

#[derive(Debug, Serialize, Default, Clone)]
pub struct QueueStats {
    pub pending: i64,
    pub processing: i64,
    pub error: i64,
    /// `done` rows whose `updated_at` is within the last 24h (throughput signal). Since
    /// migration 0022 this means ACTUALLY processed — content-gate skips are `skipped`, not `done`.
    pub done_recent: i64,
    /// `skipped` rows (static video / silent audio — ingest hint gate or worker backstop gate)
    /// updated within the last 24h. High skipped + low done on an idle camera is HEALTHY.
    pub skipped_recent: i64,
    /// Of `skipped_recent`, how many were decided at ingest from device hints
    /// (`skip_reason LIKE '%_hint'`) vs by the worker's own gate (the remainder).
    pub skipped_by_hint_recent: i64,
    /// Hint-audit verdicts recorded in the last 24h: samples where the device's hints said
    /// "skip" but the segment was processed anyway to grade them. A rising `disagree` count
    /// means the device hints are miscalibrated (or lying) and hint-skips may be losing content.
    pub audit_agree_recent: i64,
    pub audit_disagree_recent: i64,
    pub oldest_pending_age_secs: i64,
    pub max_updated_age_secs: i64,
    /// The most recent error rows (newest first, capped) so the dashboard can show *what* failed,
    /// not just how many. Empty when `error == 0`. Additive field — older UIs ignore it.
    #[serde(default)]
    pub recent_errors: Vec<QueueError>,
}

/// One failed segment's error detail, surfaced from `segment_*_status.last_error`.
#[derive(Debug, Serialize, Default, Clone)]
pub struct QueueError {
    pub segment_id: String,
    /// The worker's recorded failure message (truncated to 2000 chars at write time).
    pub last_error: String,
    /// How many times the worker retried before this error (capped by `MAX_ATTEMPTS`).
    pub attempts: i64,
    /// Seconds since the row was last updated (i.e. since this error was recorded).
    pub age_secs: i64,
}

/// How many recent error rows the dashboard surfaces per queue. Small: it's a "what's wrong right
/// now" peek, not a full error log (the per-device processing view drills deeper).
const RECENT_ERRORS_LIMIT: i64 = 5;

/// A uniform status row the frontend renders identically for every service/dependency.
#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    /// "service" | "worker" | "dependency"
    pub kind: String,
    /// "up" | "degraded" | "down" | "unknown"
    pub state: String,
    pub detail: String,
    pub latency_ms: Option<i64>,
    pub last_beat: Option<String>,
    /// `true` ⇒ a `down` here is non-fatal (e.g. Ollama); the UI mutes it.
    pub optional: bool,
    pub extra: serde_json::Value,
}

impl ServiceStatus {
    fn new(name: &str, kind: &str, state: &str, detail: String) -> Self {
        Self {
            name: name.to_string(),
            kind: kind.to_string(),
            state: state.to_string(),
            detail,
            latency_ms: None,
            last_beat: None,
            optional: false,
            extra: json!({}),
        }
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

pub async fn get_dashboard(State(state): State<ViewerState>) -> ViewerResult<Json<DashboardResponse>> {
    let cfg = &state.cfg;

    // Queues first — the worker fallback (no heartbeat) needs the backlog to decide.
    let (cameras_res, transcription, vision) = tokio::join!(
        build_cameras(&state.pool, cfg),
        queue_stats(&state.pool, "segment_transcription_status"),
        queue_stats(&state.pool, "segment_vision_status"),
    );
    let (cameras, camera_summary) = cameras_res?;
    let transcription = transcription.unwrap_or_default();
    let vision = vision.unwrap_or_default();
    let backlog = transcription.pending
        + transcription.processing
        + vision.pending
        + vision.processing;

    // Fan out every service/dependency probe concurrently; each is independently timeout-bounded
    // so one hung sibling can't stall the endpoint.
    let (worker, postgres, backend, rag, ollama, disk) = tokio::join!(
        worker_status(&state.pool, cfg, backlog),
        postgres_status(&state.pool),
        http_service("hushai-backend", &cfg.backend_base_url, &state.http, cfg.dash_probe_timeout_ms, true),
        http_service("hushai-rag", &cfg.rag_base_url, &state.http, cfg.dash_probe_timeout_ms, false),
        ollama_status(&state.http, &cfg.ollama_base_url, cfg.dash_probe_timeout_ms),
        disk_status(cfg.blob_dir.clone(), cfg.disk_watermark_bytes),
    );

    // Order: services, then dependencies. The viewer itself is up by definition (this runs in it).
    let services = vec![
        backend,
        rag,
        viewer_self_status(),
        worker,
        postgres,
        ollama,
        disk,
    ];

    Ok(Json(DashboardResponse {
        generated_at: Utc::now().to_rfc3339(),
        cameras,
        camera_summary,
        queues: Queues { transcription, vision },
        services,
        loadtest: load_loadtest_status(),
    }))
}

/// Read the load-test harness's rolling `live.json` if `$VIEWER_LOADTEST_LIVE_JSON` points at a
/// readable, parseable file. Tiny file (a few KB); best-effort — any failure yields `None` so the
/// dashboard is unaffected when no benchmark is running.
fn load_loadtest_status() -> Option<serde_json::Value> {
    let path = std::env::var("VIEWER_LOADTEST_LIVE_JSON").ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

// ---------------------------------------------------------------------------
// Cameras
// ---------------------------------------------------------------------------

#[allow(clippy::type_complexity)]
async fn build_cameras(
    pool: &PgPool,
    cfg: &ViewerConfig,
) -> ViewerResult<(Vec<CameraStatus>, CameraSummary)> {
    // Keyed on `devices.last_seen` (server receipt, bumped on every accepted upload) — the true
    // connectivity signal. Segment aggregates mirror timeline::list_devices for the card details.
    let rows: Vec<(
        String,
        Option<String>,
        String,
        DateTime<Utc>,
        DateTime<Utc>,
        i64,
        Option<i64>,
        Option<i64>,
        i64,
        i64,
        Option<bool>,
        Option<bool>,
        Option<bool>,
    )> = sqlx::query_as(
        r#"
        SELECT
            d.device_id,
            d.display_name,
            d.source_kind,
            d.first_seen,
            d.last_seen,
            EXTRACT(EPOCH FROM now() - d.last_seen)::bigint   AS last_seen_age_secs,
            min(s.capture_start_unix_nanos)                   AS first_ns,
            max(s.capture_start_unix_nanos + s.duration_nanos) AS last_ns,
            count(s.segment_id)                               AS segment_count,
            count(DISTINCT s.session_id)                      AS session_count,
            bool_or(s.media_type = 2)                         AS has_video,
            bool_or(s.media_type = 1)                         AS has_audio,
            bool_or(s.media_type = 3)                         AS has_muxed
        FROM devices d
        LEFT JOIN segments s USING (device_id)
        GROUP BY d.device_id, d.display_name, d.source_kind, d.first_seen, d.last_seen
        ORDER BY d.last_seen DESC
        "#,
    )
    .fetch_all(pool)
    .await?;

    let mut summary = CameraSummary::default();
    let cameras = rows
        .into_iter()
        .map(|r| {
            let age = r.5;
            let state = if age <= cfg.dash_connected_secs {
                summary.connected += 1;
                "connected"
            } else if age <= cfg.dash_idle_secs {
                summary.idle += 1;
                "idle"
            } else {
                summary.offline += 1;
                "offline"
            };
            summary.total += 1;
            CameraStatus {
                device_id: r.0,
                display_name: r.1,
                source_kind: r.2,
                state: state.to_string(),
                first_seen: r.3.to_rfc3339(),
                last_seen: r.4.to_rfc3339(),
                last_seen_age_secs: age,
                first_capture_unix_nanos: r.6,
                last_capture_unix_nanos: r.7,
                segment_count: r.8,
                session_count: r.9,
                has_video: r.10.unwrap_or(false),
                has_audio: r.11.unwrap_or(false),
                has_muxed: r.12.unwrap_or(false),
            }
        })
        .collect();

    Ok((cameras, summary))
}

// ---------------------------------------------------------------------------
// Work queues
// ---------------------------------------------------------------------------

/// `table` is a compile-time constant (one of the two `segment_*_status` tables, never user input),
/// so wrapping the formatted SQL in `AssertSqlSafe` after this audit is sound.
async fn queue_stats(pool: &PgPool, table: &str) -> ViewerResult<QueueStats> {
    let row: (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64) =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            r#"
        SELECT
            count(*) FILTER (WHERE status = 'pending')    AS pending,
            count(*) FILTER (WHERE status = 'processing') AS processing,
            count(*) FILTER (WHERE status = 'error')      AS error,
            count(*) FILTER (WHERE status = 'done' AND updated_at > now() - interval '24 hours') AS done_recent,
            count(*) FILTER (WHERE status = 'skipped' AND updated_at > now() - interval '24 hours') AS skipped_recent,
            count(*) FILTER (WHERE status = 'skipped' AND skip_reason LIKE '%\_hint'
                               AND updated_at > now() - interval '24 hours') AS skipped_by_hint_recent,
            count(*) FILTER (WHERE audit_verdict = 'agree'
                               AND updated_at > now() - interval '24 hours') AS audit_agree_recent,
            count(*) FILTER (WHERE audit_verdict = 'disagree'
                               AND updated_at > now() - interval '24 hours') AS audit_disagree_recent,
            COALESCE(EXTRACT(EPOCH FROM now() - min(updated_at) FILTER (WHERE status = 'pending')), 0)::bigint AS oldest_pending_age_secs,
            COALESCE(EXTRACT(EPOCH FROM now() - max(updated_at)), 0)::bigint AS max_updated_age_secs
        FROM {table}
        "#
        )))
        .fetch_one(pool)
        .await?;

    // Only pay for the detail query when there's something to show.
    let recent_errors = if row.2 > 0 {
        recent_errors(pool, table).await?
    } else {
        Vec::new()
    };

    Ok(QueueStats {
        pending: row.0,
        processing: row.1,
        error: row.2,
        done_recent: row.3,
        skipped_recent: row.4,
        skipped_by_hint_recent: row.5,
        audit_agree_recent: row.6,
        audit_disagree_recent: row.7,
        oldest_pending_age_secs: row.8,
        max_updated_age_secs: row.9,
        recent_errors,
    })
}

/// The most recently-failed rows for a queue, newest first. `table` is the same compile-time
/// constant as `queue_stats` (never user input), so `AssertSqlSafe` over the formatted SQL holds.
async fn recent_errors(pool: &PgPool, table: &str) -> ViewerResult<Vec<QueueError>> {
    let rows: Vec<(String, Option<String>, i32, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        r#"
        SELECT segment_id::text,
               last_error,
               attempts,
               COALESCE(EXTRACT(EPOCH FROM now() - updated_at), 0)::bigint AS age_secs
        FROM {table}
        WHERE status = 'error'
        ORDER BY updated_at DESC
        LIMIT {RECENT_ERRORS_LIMIT}
        "#
    )))
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(segment_id, last_error, attempts, age_secs)| QueueError {
            segment_id,
            last_error: last_error.unwrap_or_else(|| "(no message recorded)".to_string()),
            attempts: attempts as i64,
            age_secs,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Services & dependencies
// ---------------------------------------------------------------------------

/// hushai-worker: the heartbeat row is authoritative liveness. Falls back to queue inference if
/// the table doesn't exist yet (un-migrated worker) — `unknown` when idle, never a false `up`.
async fn worker_status(pool: &PgPool, cfg: &ViewerConfig, backlog: i64) -> ServiceStatus {
    let row: Result<
        Option<(
            String,
            String,
            i32,
            String,
            DateTime<Utc>,
            DateTime<Utc>,
            i64,
            i32,
            Option<i32>,
        )>,
        sqlx::Error,
    > = sqlx::query_as(
        r#"
        SELECT worker_id, instance, pid, version, started_at, last_beat,
               EXTRACT(EPOCH FROM now() - last_beat)::bigint AS beat_age_secs,
               concurrency, queue_depth
        FROM worker_heartbeat
        ORDER BY last_beat DESC
        LIMIT 1
        "#,
    )
    .fetch_optional(pool)
    .await;

    match row {
        Ok(Some(r)) => {
            let beat_age = r.6;
            let (state, detail) = if beat_age <= cfg.dash_worker_stale_secs {
                ("up", format!("heartbeat {beat_age}s ago"))
            } else {
                ("down", format!("no heartbeat for {beat_age}s (stale)"))
            };
            let mut s = ServiceStatus::new("hushai-worker", "worker", state, detail);
            s.last_beat = Some(r.5.to_rfc3339());
            s.extra = json!({
                "worker_id": r.0,
                "instance": r.1,
                "pid": r.2,
                "version": r.3,
                "started_at": r.4.to_rfc3339(),
                "concurrency": r.7,
                "queue_depth": r.8,
                "backlog": backlog,
            });
            s
        }
        // No heartbeat row: can't prove liveness. Backlog that's clearly being held ⇒ likely down;
        // an empty queue is indistinguishable from idle ⇒ unknown.
        Ok(None) => {
            let (state, detail) = if backlog > 0 {
                ("down", format!("no heartbeat; {backlog} item(s) stuck in queue"))
            } else {
                ("unknown", "no heartbeat row (idle, or worker not running)".to_string())
            };
            let mut s = ServiceStatus::new("hushai-worker", "worker", state, detail);
            s.extra = json!({ "backlog": backlog });
            s
        }
        Err(e) => {
            // Table missing (un-migrated) or query error: degrade to queue inference.
            let (state, detail) = if backlog > 0 {
                ("down", format!("heartbeat unavailable; {backlog} item(s) queued"))
            } else {
                ("unknown", format!("heartbeat unavailable: {e}"))
            };
            let mut s = ServiceStatus::new("hushai-worker", "worker", state, detail);
            s.extra = json!({ "backlog": backlog });
            s
        }
    }
}

/// Postgres: timed `SELECT 1` + sqlx pool gauges.
async fn postgres_status(pool: &PgPool) -> ServiceStatus {
    let start = Instant::now();
    let res = sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(pool).await;
    let latency = start.elapsed().as_millis() as i64;
    let size = pool.size() as i64;
    let idle = pool.num_idle() as i64;
    match res {
        Ok(_) => {
            let mut s = ServiceStatus::new("postgres", "dependency", "up", format!("SELECT 1 in {latency}ms"));
            s.latency_ms = Some(latency);
            s.extra = json!({ "pool_size": size, "pool_idle": idle, "pool_in_use": (size - idle).max(0) });
            s
        }
        Err(e) => ServiceStatus::new("postgres", "dependency", "down", e.to_string()),
    }
}

/// Generic HTTP service probe. `check_readyz` additionally probes `/readyz` (only the backend has
/// it) and reports `degraded` on a 503 (DB/disk pressure) while still reachable.
async fn http_service(
    name: &str,
    base_url: &str,
    http: &reqwest::Client,
    timeout_ms: u64,
    check_readyz: bool,
) -> ServiceStatus {
    let timeout = Duration::from_millis(timeout_ms);
    let start = Instant::now();
    let healthz = http.get(format!("{base_url}/healthz")).timeout(timeout).send().await;
    let latency = start.elapsed().as_millis() as i64;

    match healthz {
        Ok(resp) => {
            let code = resp.status().as_u16();
            if !(200..300).contains(&code) {
                let mut s = ServiceStatus::new(name, "service", "down", format!("/healthz → {code}"));
                s.latency_ms = Some(latency);
                return s;
            }
            // Reachable. Optionally fold in readiness (DB + disk) from /readyz.
            let (state, detail) = if check_readyz {
                match http.get(format!("{base_url}/readyz")).timeout(timeout).send().await {
                    Ok(r) if r.status().as_u16() == 503 => {
                        ("degraded", "reachable but /readyz 503 (DB or disk)".to_string())
                    }
                    Ok(r) if (200..300).contains(&r.status().as_u16()) => ("up", "healthy".to_string()),
                    Ok(r) => ("degraded", format!("/readyz → {}", r.status().as_u16())),
                    Err(_) => ("up", "healthy (/readyz unreachable)".to_string()),
                }
            } else {
                ("up", "healthy".to_string())
            };
            let mut s = ServiceStatus::new(name, "service", state, detail);
            s.latency_ms = Some(latency);
            s
        }
        Err(e) => ServiceStatus::new(name, "service", "down", probe_err(&e)),
    }
}

/// The viewer is serving this very request, so it's up by definition (never loopback-probe).
fn viewer_self_status() -> ServiceStatus {
    ServiceStatus::new("hushai-viewer", "service", "up", "serving this request".to_string())
}

/// Ollama is best-effort: `/api/tags` proves the server is up (not that models are loaded), and a
/// timeout is shown as `down` but flagged `optional` so the UI mutes it (it can't redden the page).
async fn ollama_status(http: &reqwest::Client, base_url: &str, timeout_ms: u64) -> ServiceStatus {
    let timeout = Duration::from_millis(timeout_ms);
    let start = Instant::now();
    let res = http.get(format!("{base_url}/api/tags")).timeout(timeout).send().await;
    let latency = start.elapsed().as_millis() as i64;
    let mut s = match res {
        Ok(resp) if (200..300).contains(&resp.status().as_u16()) => {
            let mut s = ServiceStatus::new("ollama", "dependency", "up", format!("reachable in {latency}ms"));
            s.latency_ms = Some(latency);
            s
        }
        Ok(resp) => ServiceStatus::new("ollama", "dependency", "down", format!("/api/tags → {}", resp.status().as_u16())),
        Err(e) => ServiceStatus::new("ollama", "dependency", "down", probe_err(&e)),
    };
    s.optional = true;
    s
}

/// Disk free space on the volume backing BLOB_DIR vs the backend's watermark (mirrors backend
/// readyz). `free_space_bytes` is a sync syscall ⇒ run on a blocking thread.
async fn disk_status(blob_dir: std::path::PathBuf, watermark: u64) -> ServiceStatus {
    let res = tokio::task::spawn_blocking(move || hushai_backend::storage::free_space_bytes(&blob_dir)).await;
    match res {
        Ok(Ok(free)) => {
            let state = if free >= watermark { "up" } else { "degraded" };
            let mut s = ServiceStatus::new(
                "disk",
                "dependency",
                state,
                format!("{} free", human_bytes(free)),
            );
            s.extra = json!({ "free_bytes": free, "watermark_bytes": watermark });
            s
        }
        Ok(Err(e)) => ServiceStatus::new("disk", "dependency", "down", e.to_string()),
        Err(e) => ServiceStatus::new("disk", "dependency", "down", e.to_string()),
    }
}

fn probe_err(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".to_string()
    } else if e.is_connect() {
        "connection refused".to_string()
    } else {
        e.to_string()
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    // Live-DB test, gated on DATABASE_URL like the worker/backend integration tests — skips cleanly
    // when unset so `cargo test` is green without a database.
    async fn pool() -> Option<PgPool> {
        let url = std::env::var("DATABASE_URL").ok()?;
        PgPoolOptions::new().max_connections(4).connect(&url).await.ok()
    }

    /// Minimal device/session/stream/segment so the FK on `segment_transcription_status` holds.
    async fn insert_fixture_segment(pool: &PgPool, device_id: &str) -> Uuid {
        let session_id = Uuid::now_v7();
        let segment_id = Uuid::now_v7();
        sqlx::query("INSERT INTO devices (device_id, source_kind) VALUES ($1,'test') ON CONFLICT (device_id) DO NOTHING")
            .bind(device_id).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO sessions (session_id, device_id) VALUES ($1,$2)")
            .bind(session_id).bind(device_id).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO streams (session_id, stream_id, device_id, media_type, codec, container) VALUES ($1,'s0',$2,3,'h264+aac','fmp4')")
            .bind(session_id).bind(device_id).execute(pool).await.unwrap();
        sqlx::query(
            "INSERT INTO segments (segment_id, device_id, stream_id, session_id, sequence, media_type, codec, container, \
                capture_start_unix_nanos, monotonic_start_nanos, duration_nanos, content_sha256, byte_len, blob_uri, storage_backend) \
             VALUES ($1,$2,'s0',$3,0,3,'h264+aac','fmp4',1,0,2000000000,$4,100,$5,'file')",
        )
        .bind(segment_id).bind(device_id).bind(session_id).bind(vec![0u8; 32])
        .bind(format!("file:///nonexistent/{segment_id}"))
        .execute(pool).await.unwrap();
        segment_id
    }

    async fn cleanup(pool: &PgPool, device_id: &str) {
        for sql in [
            "DELETE FROM segment_transcription_status WHERE segment_id IN (SELECT segment_id FROM segments WHERE device_id=$1)",
            "DELETE FROM segments WHERE device_id=$1",
            "DELETE FROM streams WHERE device_id=$1",
            "DELETE FROM sessions WHERE device_id=$1",
            "DELETE FROM devices WHERE device_id=$1",
        ] {
            let _ = sqlx::query(sql).bind(device_id).execute(pool).await;
        }
    }

    #[tokio::test]
    async fn queue_stats_surfaces_recent_error_detail() {
        let Some(pool) = pool().await else {
            eprintln!("skipping queue_stats_surfaces_recent_error_detail: DATABASE_URL unset");
            return;
        };
        let device = format!("test-dash-err-{}", Uuid::now_v7());
        let seg = insert_fixture_segment(&pool, &device).await;

        // Record a freshly-failed status row (updated_at = now(), so it sorts to the front of the
        // newest-first recent_errors query).
        sqlx::query(
            "INSERT INTO segment_transcription_status (segment_id, status, attempts, last_error, updated_at) \
             VALUES ($1,'error',5,'whisper OOM', now())",
        )
        .bind(seg).execute(&pool).await.unwrap();

        let stats = queue_stats(&pool, "segment_transcription_status").await.unwrap();

        assert!(stats.error >= 1, "error count should include our seeded row");
        let mine = stats
            .recent_errors
            .iter()
            .find(|e| e.segment_id == seg.to_string())
            .expect("our seeded error should be among the most-recent errors");
        assert_eq!(mine.last_error, "whisper OOM");
        assert_eq!(mine.attempts, 5);
        assert!(mine.age_secs >= 0);

        cleanup(&pool, &device).await;
    }

    /// `skipped` rows (migration 0022) surface in the 24h skip counters — split by decider —
    /// and audit verdicts are tallied; none of them leak into pending/error/done.
    #[tokio::test]
    async fn queue_stats_counts_skipped_and_audit() {
        let Some(pool) = pool().await else {
            eprintln!("skipping queue_stats_counts_skipped_and_audit: DATABASE_URL unset");
            return;
        };
        let device = format!("test-dash-skip-{}", Uuid::now_v7());
        let ingest_skip = insert_fixture_segment(&pool, &device).await;
        let worker_skip = insert_fixture_segment(&pool, &device).await;
        let audited = insert_fixture_segment(&pool, &device).await;

        let before = queue_stats(&pool, "segment_transcription_status").await.unwrap();

        sqlx::query(
            "INSERT INTO segment_transcription_status (segment_id, status, skip_reason) \
             VALUES ($1,'skipped','silent_hint')",
        )
        .bind(ingest_skip).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO segment_transcription_status (segment_id, status, skip_reason, measured_rms) \
             VALUES ($1,'skipped','silent_gate',0.002)",
        )
        .bind(worker_skip).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO segment_transcription_status (segment_id, status, hint_audit, audit_verdict) \
             VALUES ($1,'done',true,'disagree')",
        )
        .bind(audited).execute(&pool).await.unwrap();

        // `>=`: the live rig (when tests run against it) bumps these concurrently.
        let after = queue_stats(&pool, "segment_transcription_status").await.unwrap();
        assert!(after.skipped_recent - before.skipped_recent >= 2);
        assert!(after.skipped_by_hint_recent - before.skipped_by_hint_recent >= 1);
        assert!(after.audit_disagree_recent - before.audit_disagree_recent >= 1);
        assert!(after.done_recent - before.done_recent >= 1, "audited row is real work: done");

        cleanup(&pool, &device).await;
    }
}
