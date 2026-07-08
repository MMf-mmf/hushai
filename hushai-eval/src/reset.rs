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
  rolling_summaries, entity_profiles, chat_sessions, chat_messages,
  segment_transcription_status, segment_vision_status,
  conversations, threader_state,
  segments, streams, sessions
RESTART IDENTITY CASCADE";

const ENSURE_PARTITIONS: &[&str] = &[
    "ensure_transcript_partitions",
    "ensure_speaker_segment_partitions",
    "ensure_person_segment_partitions",
    "ensure_scene_object_partitions",
    "ensure_plate_detection_partitions",
];

/// Gotham graph tables (migrations 0028–0030) are DERIVED — they must start empty each case, or a
/// PRIOR case's materialized `entity_edges` bleeds into `score_graph` (which reads them UNWINDOWED)
/// as false-positive edges / false-fail `expect_no_edge`s, and a stale `graph_state` watermark
/// misreports fold quiescence. Kept OUT of `TRUNCATE_SQL` and guarded by `to_regclass` so media /
/// advisor fixtures on a DB WITHOUT the graph migrations are unaffected (the `reset_advisor`
/// precedent: never force a migration a fixture doesn't need). `graph_state` is RESET, not truncated
/// — TRUNCATE would drop the `id = 1` singleton that `graph_pass::load_state` does `fetch_one` on.
const RESET_GRAPH_SQL: &str = "
DO $$ BEGIN
  IF to_regclass('public.entity_edges') IS NOT NULL THEN
    TRUNCATE entity_edges RESTART IDENTITY;
  END IF;
  IF to_regclass('public.entity_baselines') IS NOT NULL THEN TRUNCATE entity_baselines; END IF;
  IF to_regclass('public.entity_journeys') IS NOT NULL THEN TRUNCATE entity_journeys; END IF;
  IF to_regclass('public.graph_state') IS NOT NULL THEN
    UPDATE graph_state SET events_watermark = to_timestamp(0),
      conversations_watermark = to_timestamp(0), config_hash = NULL, updated_at = now() WHERE id = 1;
  END IF;
END $$;";

pub async fn reset_db(ctx: &Ctx) -> Result<()> {
    sqlx::query(TRUNCATE_SQL)
        .execute(&ctx.pool)
        .await
        .context("TRUNCATE result/catalog/status tables")?;
    // Reset the derived Gotham graph (guarded: no-op when the graph migrations aren't applied).
    sqlx::query(RESET_GRAPH_SQL)
        .execute(&ctx.pool)
        .await
        .context("reset Gotham graph tables")?;
    for f in ENSURE_PARTITIONS {
        // SAFE: `f` comes from a fixed const allowlist, never user input.
        sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {f}(1)")))
            .execute(&ctx.pool)
            .await
            .with_context(|| format!("calling {f}(1)"))?;
    }
    Ok(())
}

/// Advisor-surface reset (the `advisor` modality): sessions/messages/memories are per-run state —
/// a prior run's memorized facts would bleed into later answers — while `books`/`book_chapters`/
/// `book_chunks` are the ingested reference corpus and MUST survive (the analog of `devices`
/// above). Kept OUT of `TRUNCATE_SQL` so media fixtures never require the advisor migrations.
pub async fn reset_advisor(ctx: &Ctx) -> Result<()> {
    sqlx::query("TRUNCATE advisor_sessions, advisor_messages, advisor_memories RESTART IDENTITY CASCADE")
        .execute(&ctx.pool)
        .await
        .context("TRUNCATE advisor session/message/memory tables")?;
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
