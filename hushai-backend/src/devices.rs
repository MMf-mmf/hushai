//! Authenticated device-management + footage-deletion surface (the destructive sibling of the
//! read-only viewer device list). Lets an operator rename a device, see per-day storage usage,
//! set a per-device retention policy, delete a day (or several) of footage, and delete a device
//! outright. All mutations live here in hushai-backend (the only writer of `segments`/blobs); the
//! viewer reverse-proxies `/v1/devices*` and injects the bearer, exactly as for `/v1/speakers*`.
//!
//! Runtime sqlx (`query`/`query_as`/`query_scalar` + `.bind`), NOT the `query!` macros — same
//! reason as speakers.rs/persons.rs (these tables/columns aren't in the committed `.sqlx/` cache).
//!
//! SAFETY (see the design doc + the adversarial review baked into it):
//!   * Device delete is ONE transaction, ordered, holding the speaker + person advisory locks so a
//!     concurrent voice/face *mint* can't re-point `first_seen_device_id` at the device after we've
//!     NULLed it: NULL speakers/persons.first_seen_device_id → DELETE segments (cascades the 7
//!     derived child tables) → streams → sessions → devices. `streams`/`sessions` do NOT cascade
//!     off `segments`, so they're deleted explicitly, and ONLY in this whole-device teardown.
//!   * Footage/day/retention deletes touch `segments` only (no streams/sessions GC — that would
//!     race live ingest's streams-upsert→segment-insert FK).
//!   * Blob files are content-addressed and may be shared, so they're reclaimed AFTER the row
//!     delete commits, by `storage::reclaim_blobs` which re-checks each candidate against the live
//!     DB. A crash between commit and unlink only orphans a (GC-able) blob, never dangles a row.

use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::error::IngestError;
use crate::state::AppState;
use crate::storage;

/// Global speaker-space advisory lock key — MUST match speakers.rs / worker speaker_match.rs.
const SPEAKER_LOCK_KEY: i64 = 0x6873_7370_6b72; // "hsspkr"
/// Global person-space advisory lock key — MUST match the worker's vision/face_match.rs.
const VISION_PERSON_LOCK_KEY: i64 = 0x6873_7670_736e; // "hsvpsn"

const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

// ---------------------------------------------------------------------------
// GET /v1/devices — management list
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ManagedDevice {
    pub device_id: String,
    pub display_name: Option<String>,
    pub source_kind: String,
    pub segment_count: i64,
    pub session_count: i64,
    /// Sum of segment `byte_len` — the *logical* footage size. May exceed reclaimable disk if any
    /// blob is shared (content-addressed), so it's an estimate, not bytes-on-disk.
    pub logical_bytes: i64,
    pub first_capture_unix_nanos: Option<i64>,
    pub last_capture_unix_nanos: Option<i64>,
    pub retention_days: Option<i32>,
    pub has_video: bool,
    pub has_audio: bool,
    pub has_muxed: bool,
}

pub async fn list_devices(
    State(st): State<AppState>,
) -> Result<Json<Vec<ManagedDevice>>, IngestError> {
    // LEFT JOIN so a device with no segments still appears. sum(bigint) is NUMERIC → cast to bigint.
    let rows: Vec<(
        String,
        Option<String>,
        String,
        Option<i32>,
        i64,
        i64,
        i64,
        Option<i64>,
        Option<i64>,
        Option<bool>,
        Option<bool>,
        Option<bool>,
    )> = sqlx::query_as(
        r#"
        SELECT
            d.device_id,
            d.display_name,
            d.source_kind,
            d.retention_days,
            count(s.segment_id)                                   AS segment_count,
            count(DISTINCT s.session_id)                          AS session_count,
            COALESCE(sum(s.byte_len), 0)::bigint                  AS logical_bytes,
            min(s.capture_start_unix_nanos)                       AS first_ns,
            max(s.capture_start_unix_nanos + s.duration_nanos)    AS last_ns,
            bool_or(s.media_type = 2)                             AS has_video,
            bool_or(s.media_type = 1)                             AS has_audio,
            bool_or(s.media_type = 3)                             AS has_muxed
        FROM devices d
        LEFT JOIN segments s USING (device_id)
        GROUP BY d.device_id, d.display_name, d.source_kind, d.retention_days
        ORDER BY last_ns DESC NULLS LAST
        "#,
    )
    .fetch_all(&st.pool)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| ManagedDevice {
                device_id: r.0,
                display_name: r.1,
                source_kind: r.2,
                retention_days: r.3,
                segment_count: r.4,
                session_count: r.5,
                logical_bytes: r.6,
                first_capture_unix_nanos: r.7,
                last_capture_unix_nanos: r.8,
                has_video: r.9.unwrap_or(false),
                has_audio: r.10.unwrap_or(false),
                has_muxed: r.11.unwrap_or(false),
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// GET /v1/devices/{id}/usage?tz=<IANA> — per-day footage breakdown
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UsageParams {
    /// IANA tz (e.g. "America/Los_Angeles") for local-day bucketing; defaults to UTC.
    pub tz: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DayUsage {
    pub day: String, // YYYY-MM-DD in the requested tz
    pub segment_count: i64,
    pub logical_bytes: i64,
    pub first_capture_unix_nanos: i64,
    pub last_capture_unix_nanos: i64,
}

pub async fn device_usage(
    State(st): State<AppState>,
    Path(device_id): Path<String>,
    Query(p): Query<UsageParams>,
) -> Result<Json<Vec<DayUsage>>, IngestError> {
    let tz = normalize_tz(p.tz);
    validate_tz(&st.pool, &tz).await?;

    // Bucket by the segment's local calendar day. `/ 1e9` converts the nanos field to the seconds
    // `to_timestamp` expects (the field is NANOS — forgetting this lands everything in 1970).
    let rows: Vec<(String, i64, i64, i64, i64)> = sqlx::query_as(
        r#"
        SELECT
            to_char(date_trunc('day', to_timestamp(capture_start_unix_nanos / 1e9) AT TIME ZONE $2),
                    'YYYY-MM-DD')                                AS day,
            count(*)                                             AS segment_count,
            sum(byte_len)::bigint                                AS logical_bytes,
            min(capture_start_unix_nanos)                        AS first_ns,
            max(capture_start_unix_nanos + duration_nanos)       AS last_ns
        FROM segments
        WHERE device_id = $1
        GROUP BY 1
        ORDER BY 1 DESC
        "#,
    )
    .bind(&device_id)
    .bind(&tz)
    .fetch_all(&st.pool)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| DayUsage {
                day: r.0,
                segment_count: r.1,
                logical_bytes: r.2,
                first_capture_unix_nanos: r.3,
                last_capture_unix_nanos: r.4,
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// PATCH /v1/devices/{id} — rename
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RenameReq {
    pub display_name: String,
}

#[derive(Debug, Serialize)]
pub struct DeviceNameRow {
    pub device_id: String,
    pub display_name: Option<String>,
}

pub async fn rename_device(
    State(st): State<AppState>,
    Path(device_id): Path<String>,
    Json(req): Json<RenameReq>,
) -> Result<Json<DeviceNameRow>, IngestError> {
    let name = req.display_name.trim();
    if name.is_empty() {
        return Err(IngestError::BadRequest(
            "display_name must not be empty".into(),
        ));
    }
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "UPDATE devices SET display_name = $1 WHERE device_id = $2 RETURNING device_id, display_name",
    )
    .bind(name)
    .bind(&device_id)
    .fetch_optional(&st.pool)
    .await?;
    let row = row.ok_or(IngestError::NotFound("device"))?;
    Ok(Json(DeviceNameRow {
        device_id: row.0,
        display_name: row.1,
    }))
}

// ---------------------------------------------------------------------------
// PUT /v1/devices/{id}/retention — set/clear the keep-last-N-days policy
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RetentionReq {
    /// `null` clears the policy (keep forever); `N>=1` keeps the last N days. A dedicated endpoint
    /// (not a field on PATCH) so `null` unambiguously means "clear", never "leave unchanged".
    pub retention_days: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct RetentionRow {
    pub device_id: String,
    pub retention_days: Option<i32>,
}

pub async fn set_retention(
    State(st): State<AppState>,
    Path(device_id): Path<String>,
    Json(req): Json<RetentionReq>,
) -> Result<Json<RetentionRow>, IngestError> {
    if let Some(days) = req.retention_days {
        if days < 1 {
            return Err(IngestError::BadRequest(
                "retention_days must be >= 1, or null to clear".into(),
            ));
        }
    }
    let row: Option<(String, Option<i32>)> = sqlx::query_as(
        "UPDATE devices SET retention_days = $1 WHERE device_id = $2 RETURNING device_id, retention_days",
    )
    .bind(req.retention_days)
    .bind(&device_id)
    .fetch_optional(&st.pool)
    .await?;
    let row = row.ok_or(IngestError::NotFound("device"))?;
    Ok(Json(RetentionRow {
        device_id: row.0,
        retention_days: row.1,
    }))
}

// ---------------------------------------------------------------------------
// DELETE /v1/devices/{id}/footage?tz=&day= — delete one local-day bucket
// POST   /v1/devices/{id}/footage/bulk-delete — delete several days
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct DeleteImpact {
    pub segments_deleted: i64,
    pub logical_bytes: i64,
}

#[derive(Debug, Deserialize)]
pub struct DayQuery {
    pub tz: Option<String>,
    pub day: String,
}

pub async fn delete_footage(
    State(st): State<AppState>,
    Path(device_id): Path<String>,
    Query(q): Query<DayQuery>,
) -> Result<Json<DeleteImpact>, IngestError> {
    let tz = normalize_tz(q.tz);
    validate_tz(&st.pool, &tz).await?;
    validate_day(&q.day)?;

    let (segments_deleted, logical_bytes, shas) = purge_day(&st.pool, &device_id, &tz, &q.day).await?;
    spawn_reclaim(&st, shas);
    Ok(Json(DeleteImpact {
        segments_deleted,
        logical_bytes,
    }))
}

#[derive(Debug, Deserialize)]
pub struct BulkDeleteReq {
    pub tz: Option<String>,
    pub days: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct BulkDayResult {
    pub day: String,
    pub segments_deleted: i64,
    pub logical_bytes: i64,
}

pub async fn bulk_delete_footage(
    State(st): State<AppState>,
    Path(device_id): Path<String>,
    Json(req): Json<BulkDeleteReq>,
) -> Result<Json<Vec<BulkDayResult>>, IngestError> {
    let tz = normalize_tz(req.tz);
    validate_tz(&st.pool, &tz).await?;
    if req.days.is_empty() {
        return Err(IngestError::BadRequest("days must not be empty".into()));
    }
    if req.days.len() > 400 {
        // Keep the (proxied, 1 MiB-bounded) request small and bounded.
        return Err(IngestError::BadRequest("too many days in one request (max 400)".into()));
    }
    for day in &req.days {
        validate_day(day)?;
    }

    // Each day is its own committed tx so a mid-batch failure preserves prior deletes and is
    // reported per-day. Reclaim runs once at the end over the union of freed shas.
    let mut results = Vec::with_capacity(req.days.len());
    let mut all_shas: Vec<[u8; 32]> = Vec::new();
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    for day in &req.days {
        let (segments_deleted, logical_bytes, shas) =
            purge_day(&st.pool, &device_id, &tz, day).await?;
        for sha in shas {
            if seen.insert(sha) {
                all_shas.push(sha);
            }
        }
        results.push(BulkDayResult {
            day: day.clone(),
            segments_deleted,
            logical_bytes,
        });
    }
    spawn_reclaim(&st, all_shas);
    Ok(Json(results))
}

// ---------------------------------------------------------------------------
// DELETE /v1/devices/{id} — delete the device and ALL its footage
// ---------------------------------------------------------------------------

pub async fn delete_device(
    State(st): State<AppState>,
    Path(device_id): Path<String>,
) -> Result<Json<DeleteImpact>, IngestError> {
    let exists: Option<(String,)> = sqlx::query_as("SELECT device_id FROM devices WHERE device_id = $1")
        .bind(&device_id)
        .fetch_optional(&st.pool)
        .await?;
    if exists.is_none() {
        return Err(IngestError::NotFound("device"));
    }

    // At most two attempts: a 23503 (a speaker/face re-pointing first_seen at the device) should be
    // impossible while we hold the identity locks, but retry once as a backstop.
    for attempt in 0..2 {
        match teardown_device(&st.pool, &device_id).await {
            Ok((impact, shas)) => {
                spawn_reclaim(&st, shas);
                return Ok(Json(impact));
            }
            Err(e) if attempt == 0 && is_fk_violation(&e) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!("delete_device loop always returns")
}

async fn teardown_device(
    pool: &PgPool,
    device_id: &str,
) -> Result<(DeleteImpact, Vec<[u8; 32]>), sqlx::Error> {
    let mut tx = pool.begin().await?;
    // Serialize against the online voice/face matcher's match-or-mint so it can't insert a row
    // pointing first_seen_device_id at this device between our NULLs and the device-row delete.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SPEAKER_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(VISION_PERSON_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE speakers SET first_seen_device_id = NULL WHERE first_seen_device_id = $1")
        .bind(device_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE persons SET first_seen_device_id = NULL WHERE first_seen_device_id = $1")
        .bind(device_id)
        .execute(&mut *tx)
        .await?;
    let rows: Vec<(Vec<u8>, i64)> =
        sqlx::query_as("DELETE FROM segments WHERE device_id = $1 RETURNING content_sha256, byte_len")
            .bind(device_id)
            .fetch_all(&mut *tx)
            .await?;
    sqlx::query("DELETE FROM streams WHERE device_id = $1")
        .bind(device_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE device_id = $1")
        .bind(device_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM devices WHERE device_id = $1")
        .bind(device_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let (segments_deleted, logical_bytes, shas) = aggregate_deleted(rows);
    Ok((
        DeleteImpact {
            segments_deleted,
            logical_bytes,
        },
        shas,
    ))
}

// ---------------------------------------------------------------------------
// Retention sweep (driven by the background task spawned from lib.rs::run)
// ---------------------------------------------------------------------------

/// Spawn the periodic retention task: run once at startup, then every `RETENTION_SWEEP_SECONDS`
/// (default 6h). Each pass is idempotent (re-deleting an already-purged window is a 0-row no-op),
/// so concurrent passes across processes are harmless and no cross-process lock is needed.
pub fn spawn_retention_task(state: AppState) {
    let secs: u64 = std::env::var("RETENTION_SWEEP_SECONDS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(21_600);
    tokio::spawn(async move {
        let interval = Duration::from_secs(secs);
        loop {
            run_retention_sweep(&state).await;
            tokio::time::sleep(interval).await;
        }
    });
}

/// One retention pass: for every device with a policy, delete segments whose footage ends before
/// `now - N days` (fully past the window — a segment still partly inside the last N days is kept).
pub async fn run_retention_sweep(st: &AppState) {
    let devices: Vec<(String, i32)> = match sqlx::query_as(
        "SELECT device_id, retention_days FROM devices WHERE retention_days IS NOT NULL",
    )
    .fetch_all(&st.pool)
    .await
    {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "retention: failed to list devices");
            return;
        }
    };
    if devices.is_empty() {
        return;
    }
    let now_ns = unix_now_nanos();
    for (device_id, days) in devices {
        let cutoff = now_ns.saturating_sub((days as i64).saturating_mul(NANOS_PER_DAY));
        match purge_older_than(&st.pool, &device_id, cutoff).await {
            Ok((deleted, _bytes, shas)) if deleted > 0 => {
                tracing::info!(device = %device_id, retention_days = days, segments = deleted, "retention purge");
                spawn_reclaim(st, shas);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(device = %device_id, error = %e, "retention purge failed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared delete helpers
// ---------------------------------------------------------------------------

/// Delete the segments whose local-day bucket (in `tz`) is `day`, returning impact + the distinct
/// content hashes freed (for blob reclamation). The `[day_start, day_end)` instants are computed in
/// SQL from the tz so the predicate is indexable (`segments_device_capture_start_idx`) and exactly
/// matches what `device_usage` counts — no boundary bleed into adjacent days.
async fn purge_day(
    pool: &PgPool,
    device_id: &str,
    tz: &str,
    day: &str,
) -> Result<(i64, i64, Vec<[u8; 32]>), IngestError> {
    let mut tx = pool.begin().await?;
    let rows: Vec<(Vec<u8>, i64)> = sqlx::query_as(
        r#"
        WITH bounds AS (
            SELECT
                (extract(epoch FROM (($3::date)::timestamp        AT TIME ZONE $2)) * 1e9)::bigint AS day_start_ns,
                (extract(epoch FROM ((($3::date) + 1)::timestamp  AT TIME ZONE $2)) * 1e9)::bigint AS day_end_ns
        )
        DELETE FROM segments s
        USING bounds b
        WHERE s.device_id = $1
          AND s.capture_start_unix_nanos >= b.day_start_ns
          AND s.capture_start_unix_nanos <  b.day_end_ns
        RETURNING s.content_sha256, s.byte_len
        "#,
    )
    .bind(device_id)
    .bind(tz)
    .bind(day)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(aggregate_deleted(rows))
}

/// Delete segments fully older than `cutoff_ns` (footage END before the cutoff). The redundant
/// `capture_start <= cutoff` clause lets the planner range-scan the capture-start index, while
/// `capture_start + duration <= cutoff` keeps the semantics exact (never deletes a straddling
/// segment that's still partly within the kept window).
async fn purge_older_than(
    pool: &PgPool,
    device_id: &str,
    cutoff_ns: i64,
) -> Result<(i64, i64, Vec<[u8; 32]>), IngestError> {
    let mut tx = pool.begin().await?;
    let rows: Vec<(Vec<u8>, i64)> = sqlx::query_as(
        "DELETE FROM segments \
         WHERE device_id = $1 \
           AND capture_start_unix_nanos <= $2 \
           AND capture_start_unix_nanos + duration_nanos <= $2 \
         RETURNING content_sha256, byte_len",
    )
    .bind(device_id)
    .bind(cutoff_ns)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(aggregate_deleted(rows))
}

/// Fold DELETE … RETURNING rows into (count, summed logical bytes, distinct content hashes).
fn aggregate_deleted(rows: Vec<(Vec<u8>, i64)>) -> (i64, i64, Vec<[u8; 32]>) {
    let segments_deleted = rows.len() as i64;
    let mut logical_bytes: i64 = 0;
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut shas: Vec<[u8; 32]> = Vec::new();
    for (sha, byte_len) in rows {
        logical_bytes = logical_bytes.saturating_add(byte_len);
        if let Ok(arr) = <[u8; 32]>::try_from(sha.as_slice()) {
            if seen.insert(arr) {
                shas.push(arr);
            }
        }
    }
    (segments_deleted, logical_bytes, shas)
}

/// Reclaim the (now-unreferenced) blobs in the background so the HTTP response returns promptly.
/// Zero grace is safe here: the candidate set is exactly the just-committed-deleted segments' shas,
/// and `reclaim_blobs` re-checks each against the live DB, so a blob still shared by a kept segment
/// is preserved. (The grace window only matters for a future full-tree sweep vs in-flight ingest of
/// genuinely new content — out of scope here.)
fn spawn_reclaim(st: &AppState, shas: Vec<[u8; 32]>) {
    if shas.is_empty() {
        return;
    }
    let pool = st.pool.clone();
    let root = st.blob_root.clone();
    tokio::spawn(async move {
        let n = shas.len();
        let freed = storage::reclaim_blobs(&pool, &root, &shas, Duration::ZERO).await;
        tracing::info!(candidate_blobs = n, freed_bytes = freed, "blob reclamation complete");
    });
}

// ---------------------------------------------------------------------------
// small utilities
// ---------------------------------------------------------------------------

fn normalize_tz(tz: Option<String>) -> String {
    match tz {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => "UTC".to_string(),
    }
}

/// Probe the tz against Postgres so an invalid IANA name is a clean 400, not a 500. Postgres
/// raises `22023 invalid_parameter_value` for an unrecognized zone.
async fn validate_tz(pool: &PgPool, tz: &str) -> Result<(), IngestError> {
    match sqlx::query_scalar::<_, String>("SELECT (now() AT TIME ZONE $1)::text")
        .bind(tz)
        .fetch_one(pool)
        .await
    {
        Ok(_) => Ok(()),
        Err(e) if db_code(&e).as_deref() == Some("22023") => {
            Err(IngestError::BadRequest(format!("invalid timezone: {tz}")))
        }
        Err(e) => Err(e.into()),
    }
}

/// Cheap shape guard for a YYYY-MM-DD day string (the SQL `::date` cast is the real validator).
fn validate_day(day: &str) -> Result<(), IngestError> {
    let ok = day.len() == 10
        && day.as_bytes()[4] == b'-'
        && day.as_bytes()[7] == b'-'
        && day
            .as_bytes()
            .iter()
            .enumerate()
            .all(|(i, b)| if i == 4 || i == 7 { *b == b'-' } else { b.is_ascii_digit() });
    if ok {
        Ok(())
    } else {
        Err(IngestError::BadRequest(format!(
            "day must be YYYY-MM-DD, got {day:?}"
        )))
    }
}

fn unix_now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn is_fk_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23503"))
}

fn db_code(e: &sqlx::Error) -> Option<String> {
    match e {
        sqlx::Error::Database(db) => db.code().map(|c| c.into_owned()),
        _ => None,
    }
}
