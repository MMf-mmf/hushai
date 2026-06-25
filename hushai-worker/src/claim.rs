//! Durable work-claiming over `segment_transcription_status`.
//!
//! - [`ensure_status_rows`] backfills a `pending` row for every segment that lacks
//!   one — this is how both the initial backlog and newly-ingested segments enter
//!   the queue (keep-up mode).
//! - [`claim_one`] atomically leases the oldest processable segment with
//!   `FOR UPDATE SKIP LOCKED`, so multiple worker tasks/instances never
//!   double-process. A `processing` row whose claim is older than the lease is
//!   considered crashed and re-leased (crash-safe / resumable).

use sqlx::PgPool;
use uuid::Uuid;

/// Insert a `pending` status row for every segment without one. Idempotent.
/// Returns the number of newly-tracked segments.
pub async fn ensure_status_rows(pool: &PgPool) -> sqlx::Result<u64> {
    let res = sqlx::query(
        r#"
        INSERT INTO segment_transcription_status (segment_id)
        SELECT segment_id FROM segments
        ON CONFLICT (segment_id) DO NOTHING
        "#,
    )
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
            WHERE s.status = 'pending'
               OR (s.status = 'error'      AND s.attempts < $1)
               OR (s.status = 'processing' AND s.claimed_at < now() - make_interval(secs => $2))
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
