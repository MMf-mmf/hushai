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
    let res = sqlx::query(
        r#"
        INSERT INTO segment_transcription_status (segment_id)
        SELECT segment_id FROM segments
        WHERE media_type IN (1, 3)
        ON CONFLICT (segment_id) DO NOTHING
        "#,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
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

/// Atomically claim the oldest processable segment, returning its id, or `None`
/// when nothing is claimable. Re-leases crashed `processing` claims older than
/// `lease_secs`. `max_attempts` caps retries of `error` rows.
pub async fn claim_one(
    pool: &PgPool,
    max_attempts: i32,
    lease_secs: f64,
) -> sqlx::Result<Option<Uuid>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
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
        RETURNING t.segment_id
        "#,
    )
    .bind(max_attempts)
    .bind(lease_secs)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

// ---- Vision work queue (segment_vision_status) ----
// The visual sibling of the audio queue above, on a SEPARATE status table so a vision failure and
// an ASR failure are claimed/retried/surfaced independently. Processes VIDEO/MUXED only
// (`media_type IN (2,3)`); a MUXED segment is processed by BOTH the audio and vision paths.

/// Insert a `pending` vision-status row for every VIDEO/MUXED segment without one. Idempotent.
pub async fn ensure_vision_status_rows(pool: &PgPool) -> sqlx::Result<u64> {
    let res = sqlx::query(
        r#"
        INSERT INTO segment_vision_status (segment_id)
        SELECT segment_id FROM segments
        WHERE media_type IN (2, 3)
        ON CONFLICT (segment_id) DO NOTHING
        "#,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Atomically claim the oldest processable VIDEO/MUXED segment for the vision pipeline, or `None`.
/// Same `FOR UPDATE SKIP LOCKED` lease + crash re-lease semantics as [`claim_one`].
pub async fn claim_one_vision(
    pool: &PgPool,
    max_attempts: i32,
    lease_secs: f64,
) -> sqlx::Result<Option<Uuid>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
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
        RETURNING t.segment_id
        "#,
    )
    .bind(max_attempts)
    .bind(lease_secs)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

/// Mark a vision segment `done`.
pub async fn mark_vision_done(pool: &PgPool, segment_id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE segment_vision_status SET status = 'done', updated_at = now() WHERE segment_id = $1",
    )
    .bind(segment_id)
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
