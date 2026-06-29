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
    /// `done` rows whose `updated_at` is within the last 24h (throughput signal).
    pub done_recent: i64,
    pub oldest_pending_age_secs: i64,
    pub max_updated_age_secs: i64,
}

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
    }))
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
    let row: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        r#"
        SELECT
            count(*) FILTER (WHERE status = 'pending')    AS pending,
            count(*) FILTER (WHERE status = 'processing') AS processing,
            count(*) FILTER (WHERE status = 'error')      AS error,
            count(*) FILTER (WHERE status = 'done' AND updated_at > now() - interval '24 hours') AS done_recent,
            COALESCE(EXTRACT(EPOCH FROM now() - min(updated_at) FILTER (WHERE status = 'pending')), 0)::bigint AS oldest_pending_age_secs,
            COALESCE(EXTRACT(EPOCH FROM now() - max(updated_at)), 0)::bigint AS max_updated_age_secs
        FROM {table}
        "#
    )))
    .fetch_one(pool)
    .await?;

    Ok(QueueStats {
        pending: row.0,
        processing: row.1,
        error: row.2,
        done_recent: row.3,
        oldest_pending_age_secs: row.4,
        max_updated_age_secs: row.5,
    })
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
