//! Invariant 1: clean, pinned starting state. TRUNCATE every result + catalog + status table
//! (and `segments`) so speaker/face match-vs-mint sees a byte-identical empty catalog each run.
//! Preserves `devices`, `audit_log`, `worker_heartbeat`, and `_sqlx_migrations`.

use crate::ctx::Ctx;
use anyhow::{Context, Result};

/// Validated against the live schema. TRUNCATE on a partitioned parent cascades to all monthly
/// partitions atomically, so we list parents only.
const TRUNCATE_SQL: &str = "
TRUNCATE
  transcript_sentences, speaker_segments, person_segments, scene_objects, plate_detections,
  speakers, persons, license_plates,
  events, video_events, alert_rules, alert_deliveries, watchlist,
  rolling_summaries, chat_sessions, chat_messages,
  segment_transcription_status, segment_vision_status,
  segments, streams, sessions
RESTART IDENTITY CASCADE";

const ENSURE_PARTITIONS: &[&str] = &[
    "ensure_transcript_partitions",
    "ensure_speaker_segment_partitions",
    "ensure_person_segment_partitions",
    "ensure_scene_object_partitions",
    "ensure_plate_detection_partitions",
];

pub async fn reset_db(ctx: &Ctx) -> Result<()> {
    sqlx::query(TRUNCATE_SQL)
        .execute(&ctx.pool)
        .await
        .context("TRUNCATE result/catalog/status tables")?;
    for f in ENSURE_PARTITIONS {
        // SAFE: `f` comes from a fixed const allowlist, never user input.
        sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {f}(1)")))
            .execute(&ctx.pool)
            .await
            .with_context(|| format!("calling {f}(1)"))?;
    }
    Ok(())
}

/// Ensure the fixture's device exists so catalog FKs (first_seen_device_id) resolve before any
/// direct seed / enrollment. Ingest also upserts the device, so this is belt-and-suspenders.
pub async fn upsert_device(ctx: &Ctx, device_id: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO devices (device_id, source_kind) VALUES ($1, 'file_replay')
         ON CONFLICT (device_id) DO NOTHING",
    )
    .bind(device_id)
    .execute(&ctx.pool)
    .await
    .context("upsert device")?;
    Ok(())
}
