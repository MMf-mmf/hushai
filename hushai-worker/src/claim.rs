//! Durable work-claiming over `segment_transcription_status`.
//!
//! - [`ensure_status_rows`] backfills a `pending` row for every segment that lacks
//!   one — this is how both the initial backlog and newly-ingested segments enter
//!   the queue (keep-up mode).
//! - [`reconcile_missing_speaker_segments`] re-queues already-`done` audio that has
//!   no voiceprint yet, so a window where the speaker stage was unavailable can't
//!   permanently strand a voice (self-healing on every startup).
//! - [`claim_one`] atomically leases the oldest processable segment with
//!   `FOR UPDATE SKIP LOCKED`, so multiple worker tasks/instances never
//!   double-process. A `processing` row whose claim is older than the lease is
//!   considered crashed and re-leased (crash-safe / resumable).
//!
//! Statuses are `pending | processing | done | error | skipped`. `skipped` is a
//! TERMINAL no-work verdict (static video / silent audio) written either by the
//! backend's ingest hint gate (`skip_reason='silent_hint'/'static_hint'`) or by the
//! worker's own backstop gates (`'silent_gate'/'static_gate'`). It is deliberately
//! absent from every claimable set here AND from `reconcile_missing_speaker_segments`
//! (which matches `done` only), so skipped rows are never claimed and never
//! resurrected on restart. Re-evaluation after a calibration change is an explicit
//! operator action: flip `skipped` rows back to `pending` (see AGENTS.md).
//!
//! Only AUDIO (`media_type=1`) and MUXED (`media_type=3`) segments are ever
//! processed here — this worker has no video pipeline, and feeding a VIDEO-only
//! blob to ffmpeg audio extraction just fails ("Output file does not contain any
//! stream"). The media filter lives on the claim/backfill queries (the backend
//! inserts a status row for every segment at ingest regardless of media type).

use sqlx::PgPool;
use uuid::Uuid;

/// Insert a `pending` status row for every AUDIO/MUXED segment without one. Idempotent.
/// Returns the number of newly-tracked segments. VIDEO-only segments are skipped (no
/// audio to transcribe or embed).
pub async fn ensure_status_rows(pool: &PgPool) -> sqlx::Result<u64> {
    execute_backfill_with_retry(
        pool,
        r#"
        INSERT INTO segment_transcription_status (segment_id)
        SELECT segment_id FROM segments
        WHERE media_type IN (1, 3)
        ON CONFLICT (segment_id) DO NOTHING
        "#,
    )
    .await
}

/// Run a backfill `INSERT … SELECT` with a bounded retry on FK violations (23503).
/// The SELECT reads the statement snapshot while the FK check runs against current data,
/// so a segment deleted mid-statement (footage/device delete, retention) raises 23503 even
/// though the statement is "atomic". A retry re-snapshots without the vanished row; deletes
/// are rare + bounded, so this converges — and worker STARTUP depends on it not failing
/// spuriously (`run()` aborts on a backfill error).
async fn execute_backfill_with_retry(pool: &PgPool, sql: &'static str) -> sqlx::Result<u64> {
    let mut last_err: Option<sqlx::Error> = None;
    for _ in 0..3 {
        match sqlx::query(sql).execute(pool).await {
            Ok(res) => return Ok(res.rows_affected()),
            Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23503") => {
                last_err = Some(sqlx::Error::Database(db));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.expect("loop ran"))
}

/// Re-queue every AUDIO/MUXED segment that is `done` but has no `speaker_segments`
/// row — i.e. it was transcribed during a window when the speaker stage wasn't
/// running, so its `speaker_id` is permanently NULL and its voice never reaches the
/// catalog. Resets such rows to `pending` so the normal pipeline re-runs and assigns
/// (or mints) a speaker. Returns the number re-queued.
///
/// Safe + convergent:
/// - Reprocessing is idempotent — `write_transcript` delete-then-inserts the
///   transcript rows and `assign_speaker` delete-then-inserts the speaker_segments
///   row, so no duplicates.
/// - The `NOT EXISTS` check matches on `segment_id` only (index-backed), so a row
///   the matcher *deliberately* left unattributed still counts as "has a voiceprint"
///   (the embedding is stored even when `speaker_id` is NULL) and is NOT re-queued.
///   Once a segment has a speaker_segments row this is a no-op for it forever, so a
///   clean restart re-queues nothing.
/// - `done`-only (not `error`): genuinely broken audio that erred out is left alone
///   instead of looping.
pub async fn reconcile_missing_speaker_segments(pool: &PgPool) -> sqlx::Result<u64> {
    let res = sqlx::query(
        r#"
        UPDATE segment_transcription_status t
           SET status     = 'pending',
               attempts   = 0,
               last_error = NULL,
               claimed_at = NULL,
               updated_at = now()
          FROM segments g
         WHERE t.segment_id = g.segment_id
           AND t.status = 'done'
           AND g.media_type IN (1, 3)
           AND NOT EXISTS (
                 SELECT 1 FROM speaker_segments ss WHERE ss.segment_id = t.segment_id
               )
        "#,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Delete `quality='reject'` tombstones so the segments they marked are re-evaluated under
/// the current gates. A tombstone is sticky by design (it stops the reconcile re-queueing a
/// no-voice segment every restart); this is the deliberate one-shot escape hatch to run after
/// a calibration change. Returns the number cleared. The caller then runs
/// [`reconcile_missing_speaker_segments`], which re-queues the now-rowless `done` segments.
pub async fn clear_reject_tombstones(pool: &PgPool) -> sqlx::Result<u64> {
    let res = sqlx::query("DELETE FROM speaker_segments WHERE quality = 'reject'")
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Atomically claim the oldest processable segment, returning its id plus its
/// `hint_audit` flag (true = the ingest hint gate would have skipped this segment but
/// enqueued it as an audit sample; the pipeline records agree/disagree after its own
/// gate runs), or `None` when nothing is claimable. Re-leases crashed `processing`
/// claims older than `lease_secs`. `max_attempts` caps retries of `error` rows.
pub async fn claim_one(
    pool: &PgPool,
    max_attempts: i32,
    lease_secs: f64,
) -> sqlx::Result<Option<(Uuid, bool)>> {
    let row: Option<(Uuid, bool)> = sqlx::query_as(
        r#"
        WITH next AS (
            SELECT s.segment_id
            FROM segment_transcription_status s
            JOIN segments g ON g.segment_id = s.segment_id
            WHERE g.media_type IN (1, 3)
              AND ( s.status = 'pending'
                 OR (s.status = 'error'      AND s.attempts < $1)
                 OR (s.status = 'processing' AND s.claimed_at < now() - make_interval(secs => $2)) )
            ORDER BY g.capture_start_unix_nanos
            FOR UPDATE SKIP LOCKED
            LIMIT 1
        )
        UPDATE segment_transcription_status t
           SET status     = 'processing',
               attempts   = t.attempts + 1,
               claimed_at = now(),
               updated_at = now()
          FROM next
         WHERE t.segment_id = next.segment_id
        RETURNING t.segment_id, t.hint_audit
        "#,
    )
    .bind(max_attempts)
    .bind(lease_secs)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Persist the audio gate's measured inputs (calibration telemetry) and, for audit rows,
/// the hint-vs-gate verdict, onto an already-terminal status row. Best-effort side channel:
/// runs OUTSIDE the transcript tx (a crash between the two loses telemetry, never data).
pub async fn record_audio_gate_telemetry(
    pool: &PgPool,
    segment_id: Uuid,
    measured_rms: Option<f32>,
    measured_speech_secs: Option<f32>,
    audit_verdict: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        UPDATE segment_transcription_status
           SET measured_rms         = COALESCE($2, measured_rms),
               measured_speech_secs = COALESCE($3, measured_speech_secs),
               audit_verdict        = COALESCE($4, audit_verdict)
         WHERE segment_id = $1
        "#,
    )
    .bind(segment_id)
    .bind(measured_rms)
    .bind(measured_speech_secs)
    .bind(audit_verdict)
    .execute(pool)
    .await?;
    Ok(())
}

// ---- Vision work queue (segment_vision_status) ----
// The visual sibling of the audio queue above, on a SEPARATE status table so a vision failure and
// an ASR failure are claimed/retried/surfaced independently. Processes VIDEO/MUXED only
// (`media_type IN (2,3)`); a MUXED segment is processed by BOTH the audio and vision paths.

/// Insert a `pending` vision-status row for every VIDEO/MUXED segment without one. Idempotent.
pub async fn ensure_vision_status_rows(pool: &PgPool) -> sqlx::Result<u64> {
    execute_backfill_with_retry(
        pool,
        r#"
        INSERT INTO segment_vision_status (segment_id)
        SELECT segment_id FROM segments
        WHERE media_type IN (2, 3)
        ON CONFLICT (segment_id) DO NOTHING
        "#,
    )
    .await
}

/// Atomically claim the oldest processable VIDEO/MUXED segment for the vision pipeline
/// (returning its id + `hint_audit` flag, see [`claim_one`]), or `None`.
/// Same `FOR UPDATE SKIP LOCKED` lease + crash re-lease semantics as [`claim_one`].
pub async fn claim_one_vision(
    pool: &PgPool,
    max_attempts: i32,
    lease_secs: f64,
) -> sqlx::Result<Option<(Uuid, bool)>> {
    let row: Option<(Uuid, bool)> = sqlx::query_as(
        r#"
        WITH next AS (
            SELECT s.segment_id
            FROM segment_vision_status s
            JOIN segments g ON g.segment_id = s.segment_id
            WHERE g.media_type IN (2, 3)
              AND ( s.status = 'pending'
                 OR (s.status = 'error'      AND s.attempts < $1)
                 OR (s.status = 'processing' AND s.claimed_at < now() - make_interval(secs => $2)) )
            ORDER BY g.capture_start_unix_nanos
            FOR UPDATE SKIP LOCKED
            LIMIT 1
        )
        UPDATE segment_vision_status t
           SET status     = 'processing',
               attempts   = t.attempts + 1,
               claimed_at = now(),
               updated_at = now()
          FROM next
         WHERE t.segment_id = next.segment_id
        RETURNING t.segment_id, t.hint_audit
        "#,
    )
    .bind(max_attempts)
    .bind(lease_secs)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Mark a vision segment `done`, persisting the motion gate's measured distance (calibration
/// telemetry; `None` when the gate didn't run or had no baseline) and, for audit rows, the
/// hint-vs-gate verdict.
pub async fn mark_vision_done(
    pool: &PgPool,
    segment_id: Uuid,
    measured_motion_distance: Option<f32>,
    audit_verdict: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        UPDATE segment_vision_status
           SET status = 'done',
               measured_motion_distance = COALESCE($2, measured_motion_distance),
               audit_verdict            = COALESCE($3, audit_verdict),
               updated_at = now()
         WHERE segment_id = $1
        "#,
    )
    .bind(segment_id)
    .bind(measured_motion_distance)
    .bind(audit_verdict)
    .execute(pool)
    .await?;
    Ok(())
}

/// Terminal `skipped` mark for the vision lane: the motion gate decided the scene is static, so
/// no inference ran and no rows were written. Never re-claimed (see module docs); flip back to
/// `pending` manually to re-evaluate after a calibration change.
pub async fn mark_vision_skipped(
    pool: &PgPool,
    segment_id: Uuid,
    reason: &str,
    measured_motion_distance: Option<f32>,
    audit_verdict: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        UPDATE segment_vision_status
           SET status = 'skipped',
               skip_reason = $2,
               last_error  = NULL,
               measured_motion_distance = COALESCE($3, measured_motion_distance),
               audit_verdict            = COALESCE($4, audit_verdict),
               updated_at = now()
         WHERE segment_id = $1
        "#,
    )
    .bind(segment_id)
    .bind(reason)
    .bind(measured_motion_distance)
    .bind(audit_verdict)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a failed vision attempt (caps retries via `claim_one_vision`'s `attempts < max`).
pub async fn mark_vision_error(
    pool: &PgPool,
    segment_id: Uuid,
    last_error: &str,
) -> sqlx::Result<()> {
    let truncated: String = last_error.chars().take(2000).collect();
    sqlx::query(
        "UPDATE segment_vision_status SET status = 'error', last_error = $2, updated_at = now() WHERE segment_id = $1",
    )
    .bind(segment_id)
    .bind(truncated)
    .execute(pool)
    .await?;
    Ok(())
}

/// True if the segment still exists. Used to demote a processing failure from a real error to a
/// benign skip: a footage/device delete (or retention) can remove a segment WHILE the worker is
/// mid-flight (the worker claims the *status* row, not the segment), so the derived-row INSERTs
/// FK-violate (`23503`) and the status row is itself cascade-gone — there's nothing to retry or
/// record. On a transient DB error we assume it still exists, so a genuine failure is never hidden.
pub async fn segment_exists(pool: &PgPool, segment_id: Uuid) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM segments WHERE segment_id = $1)")
        .bind(segment_id)
        .fetch_one(pool)
        .await
        .unwrap_or(true)
}

/// Record a failed attempt. `attempts` was already incremented at claim time;
/// once it reaches `max_attempts` the row stops being re-claimed by `claim_one`.
pub async fn mark_error(pool: &PgPool, segment_id: Uuid, last_error: &str) -> sqlx::Result<()> {
    // Cap stored error text so a giant message can't bloat the row.
    let truncated: String = last_error.chars().take(2000).collect();
    sqlx::query(
        r#"
        UPDATE segment_transcription_status
           SET status = 'error', last_error = $2, updated_at = now()
         WHERE segment_id = $1
        "#,
    )
    .bind(segment_id)
    .bind(truncated)
    .execute(pool)
    .await?;
    Ok(())
}
