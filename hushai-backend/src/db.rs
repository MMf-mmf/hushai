//! Postgres access: pool construction, the idempotent persist transaction, and
//! the readiness probe. All `sqlx` query macros live here so the offline
//! `.sqlx/` data stays localized.

use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::config::Config;
use crate::error::IngestError;
use crate::proto::DecodedManifest;

/// Outcome of attempting to persist a segment (drives the HTTP status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persisted {
    /// Row newly inserted this request.
    Inserted,
    /// `segment_id` already present with identical bytes — a legitimate retry.
    DuplicateSameBytes,
    /// `segment_id` already present with DIFFERENT bytes — contract violation.
    ConflictDifferentBytes,
}

pub async fn connect(config: &Config) -> anyhow::Result<PgPool> {
    // Server-side per-connection timeouts + connection recycling so a stuck/abandoned transaction
    // can't pin a pooled connection indefinitely: a client-cancelled request leaves an orphaned
    // Postgres backend holding locks, and worker background txns (process/vision) have no HTTP timeout
    // above them. `idle_in_transaction_session_timeout` is always safe (no legitimate idle-open tx);
    // `statement_timeout` is generous (real queries are indexed/sub-second) and env-tunable for any
    // atypically-heavy analytics — 0 disables (Postgres semantics). Values are milliseconds.
    let stmt_ms = std::env::var("DB_STATEMENT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120)
        .saturating_mul(1000);
    let idle_tx_ms = std::env::var("DB_IDLE_TX_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60)
        .saturating_mul(1000);
    let init = format!(
        "SET statement_timeout = {stmt_ms}; SET idle_in_transaction_session_timeout = {idle_tx_ms};"
    );
    let pool = PgPoolOptions::new()
        .max_connections(config.db_max_connections)
        .acquire_timeout(Duration::from_secs(config.db_acquire_timeout_secs))
        .max_lifetime(Duration::from_secs(30 * 60))
        .idle_timeout(Duration::from_secs(10 * 60))
        .after_connect(move |conn, _meta| {
            // SAFE: `init` is built only from integer millisecond values (env-parsed u64), no user
            // input — the SET statements can't carry an injection.
            let init = sqlx::AssertSqlSafe(init.clone());
            Box::pin(async move {
                // Two `;`-separated SET commands MUST go through the SIMPLE query protocol
                // (`raw_sql`). `sqlx::query` prepares the statement, and Postgres rejects a
                // multi-command prepared statement ("cannot insert multiple commands into a
                // prepared statement") — which fails `after_connect` on every pooled connection,
                // so the pool never opens and the backend exits with "pool timed out".
                sqlx::raw_sql(init).execute(&mut *conn).await?;
                Ok(())
            })
        })
        .connect(&config.database_url)
        .await?;
    Ok(pool)
}

/// Liveness of the DB dependency for `/readyz`.
pub async fn readiness(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}

/// Persist a fully-validated segment, upholding exactly-once-at-rest semantics.
///
/// Upsert order is FK-safe: devices → sessions → streams → segments. The segment
/// PK (`segment_id`) is the idempotency key; `ON CONFLICT DO NOTHING` + a digest
/// re-read closes the duplicate race (equal bytes → accept, different → reject).
pub async fn persist_segment(
    pool: &PgPool,
    m: &DecodedManifest,
    blob_uri: &str,
    storage_backend: &str,
) -> Result<Persisted, IngestError> {
    let mut tx = pool.begin().await?;

    // Device: store source_kind (never branched on), bump last_seen on repeat.
    sqlx::query!(
        r#"
        INSERT INTO devices (device_id, source_kind, attrs)
        VALUES ($1, $2, $3)
        ON CONFLICT (device_id) DO UPDATE SET last_seen = now()
        "#,
        m.device_id,
        m.source_kind,
        m.attrs,
    )
    .execute(&mut *tx)
    .await?;

    // Session.
    sqlx::query!(
        r#"
        INSERT INTO sessions (session_id, device_id)
        VALUES ($1, $2)
        ON CONFLICT (session_id) DO NOTHING
        "#,
        m.session_id,
        m.device_id,
    )
    .execute(&mut *tx)
    .await?;

    // Stream (composite PK).
    sqlx::query!(
        r#"
        INSERT INTO streams (session_id, stream_id, device_id, media_type, codec, container, codec_init_data)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (session_id, stream_id) DO NOTHING
        "#,
        m.session_id,
        m.stream_id,
        m.device_id,
        m.media_type,
        m.codec,
        m.container,
        m.codec_init_data.as_slice(),
    )
    .execute(&mut *tx)
    .await?;

    // Segment: the idempotency-bearing insert.
    let insert = sqlx::query!(
        r#"
        INSERT INTO segments (
            segment_id, device_id, stream_id, session_id, sequence,
            media_type, codec, container, codec_init_data,
            capture_start_unix_nanos, monotonic_start_nanos, duration_nanos,
            content_sha256, byte_len, gap_before, blob_uri, storage_backend, attrs
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
        ON CONFLICT (segment_id) DO NOTHING
        RETURNING segment_id
        "#,
        m.segment_id,
        m.device_id,
        m.stream_id,
        m.session_id,
        m.sequence,
        m.media_type,
        m.codec,
        m.container,
        m.codec_init_data.as_slice(),
        m.capture_start_unix_nanos,
        m.monotonic_start_nanos,
        m.duration_nanos,
        &m.content_sha256[..],
        m.byte_len,
        m.gap_before,
        blob_uri,
        storage_backend,
        m.attrs,
    )
    .fetch_optional(&mut *tx)
    .await;

    let inserted = match insert {
        Ok(row) => row.is_some(),
        // A different segment_id colliding on UNIQUE(session_id, stream_id, sequence).
        Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
            tx.rollback().await.ok();
            return Err(IngestError::SequenceConflict);
        }
        Err(e) => return Err(e.into()),
    };

    if inserted {
        // Queue the new segment for transcription right here instead of relying on the
        // worker's periodic full-table backfill scan, and wake idle workers via
        // LISTEN/NOTIFY. Both ride this transaction, so they only take effect if the
        // segment commit succeeds (and roll back with it otherwise).
        //
        // Only AUDIO (1) / MUXED (3) carry audio the worker can transcribe + voiceprint;
        // VIDEO-only (2) segments have nothing for it to do (and would just fail ffmpeg
        // audio extraction), so they are never queued or notified.
        if matches!(m.media_type, 1 | 3) {
            sqlx::query!(
                r#"
                INSERT INTO segment_transcription_status (segment_id)
                VALUES ($1)
                ON CONFLICT (segment_id) DO NOTHING
                "#,
                m.segment_id,
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query!(
                r#"SELECT pg_notify('hushai_segment_ingested', $1)"#,
                m.segment_id.to_string(),
            )
            .execute(&mut *tx)
            .await?;
        }

        // VIDEO (2) / MUXED (3) carry frames the vision worker processes (face identity +
        // objects), on a SEPARATE queue (migration 0009). A MUXED segment is queued for BOTH.
        // Runtime query (not the `query!` macro) so adding the vision table needs no
        // `cargo sqlx prepare` — same deliberate choice as `speakers.rs`.
        if matches!(m.media_type, 2 | 3) {
            sqlx::query(
                r#"
                INSERT INTO segment_vision_status (segment_id)
                VALUES ($1)
                ON CONFLICT (segment_id) DO NOTHING
                "#,
            )
            .bind(m.segment_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query("SELECT pg_notify('hushai_segment_ingested', $1)")
                .bind(m.segment_id.to_string())
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        return Ok(Persisted::Inserted);
    }

    // Conflict on segment_id PK: compare stored bytes to decide retry vs misuse.
    let existing = sqlx::query!(
        r#"SELECT content_sha256 FROM segments WHERE segment_id = $1"#,
        m.segment_id,
    )
    .fetch_one(&mut *tx)
    .await?;

    if existing.content_sha256.as_slice() == m.content_sha256.as_slice() {
        tx.commit().await?;
        Ok(Persisted::DuplicateSameBytes)
    } else {
        tx.rollback().await.ok();
        Ok(Persisted::ConflictDifferentBytes)
    }
}
