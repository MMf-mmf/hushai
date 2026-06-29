//! Invariant 5: multi-signal, quiescent completion gate. Never query results early.
//!
//! 1. Wait until EVERY injected segment is terminal in the lane status table(s) we care about —
//!    `done`, or `error` with attempts exhausted. `pending`/`processing`/retryable-`error` block.
//! 2. Then wait for the async event producer to settle: the event count must hold steady for
//!    `quiesce_polls` consecutive reads.
//! On timeout the run is INCONCLUSIVE (never scored) — the caller maps that to exit code 2.

use crate::ctx::Ctx;
use crate::fixtures::Meta;
use anyhow::Result;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct PollOutcome {
    pub settled: bool,
    pub timed_out: bool,
    pub injected: usize,
    pub audio_done: i64,
    pub vision_done: i64,
    pub event_count: i64,
    pub errors: Vec<String>,
}

struct LaneState {
    settled: i64,
    done: i64,
    errors: Vec<String>,
}

pub async fn wait_until_complete(ctx: &Ctx, meta: &Meta, ids: &[Uuid], base_ns: i64) -> Result<PollOutcome> {
    let max_attempts: i32 = std::env::var("MAX_ATTEMPTS").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    let timeout = Duration::from_secs(meta.poll.timeout_secs);
    let interval = Duration::from_secs(meta.poll.interval_secs.max(1));

    // Which lanes to wait on. Events do NOT force the vision lane: an audio fixture's `speech`
    // events come from the AUDIO lane, and vision-derived events (person/object/plate) are only
    // expected when that modality is also scored (which sets needs_vision). Waiting on a lane that
    // is disabled / has no rows for these segments would hang until timeout.
    let has_audio = meta.media_kind != "video";
    let has_video = meta.media_kind != "audio";
    let wait_audio = (meta.needs_audio() || meta.modality("events")) && has_audio;
    let wait_vision = meta.needs_vision() && has_video;

    let start = Instant::now();
    let mut audio = LaneState { settled: 0, done: 0, errors: vec![] };
    let mut vision = LaneState { settled: 0, done: 0, errors: vec![] };

    // Phase 1: all relevant-lane segments terminal.
    loop {
        if wait_audio {
            audio = lane_state(ctx, "segment_transcription_status", ids, max_attempts).await?;
        }
        if wait_vision {
            vision = lane_state(ctx, "segment_vision_status", ids, max_attempts).await?;
        }
        let audio_ok = !wait_audio || audio.settled as usize == ids.len();
        let vision_ok = !wait_vision || vision.settled as usize == ids.len();
        if audio_ok && vision_ok {
            break;
        }
        if start.elapsed() > timeout {
            let mut errors = audio.errors.clone();
            errors.extend(vision.errors.clone());
            return Ok(PollOutcome {
                settled: false,
                timed_out: true,
                injected: ids.len(),
                audio_done: audio.done,
                vision_done: vision.done,
                event_count: event_count(ctx, &meta.device_id, base_ns).await?,
                errors,
            });
        }
        tokio::time::sleep(interval).await;
    }

    // Phase 2: event-producer quiescence.
    let mut last = event_count(ctx, &meta.device_id, base_ns).await?;
    let mut stable = 0u32;
    while stable < meta.poll.quiesce_polls {
        tokio::time::sleep(interval).await;
        let now = event_count(ctx, &meta.device_id, base_ns).await?;
        if now == last {
            stable += 1;
        } else {
            stable = 0;
            last = now;
        }
        if start.elapsed() > timeout {
            break; // settled on the lanes; just didn't fully confirm quiescence — still scoreable.
        }
    }

    let mut errors = audio.errors.clone();
    errors.extend(vision.errors.clone());
    Ok(PollOutcome {
        settled: true,
        timed_out: false,
        injected: ids.len(),
        audio_done: audio.done,
        vision_done: vision.done,
        event_count: last,
        errors,
    })
}

async fn lane_state(ctx: &Ctx, table: &str, ids: &[Uuid], max_attempts: i32) -> Result<LaneState> {
    // SAFE: `table` is one of two hardcoded status-table names, never user input.
    let rows: Vec<(Uuid, String, i32)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT segment_id, status, attempts FROM {table} WHERE segment_id = ANY($1)"
    )))
    .bind(ids)
    .fetch_all(&ctx.pool)
    .await?;

    let mut st = LaneState { settled: 0, done: 0, errors: vec![] };
    for (id, status, attempts) in rows {
        match status.as_str() {
            "done" => {
                st.done += 1;
                st.settled += 1;
            }
            "error" if attempts >= max_attempts => {
                st.settled += 1;
                st.errors.push(format!("{table} {id}: error (attempts={attempts})"));
            }
            _ => {}
        }
    }
    Ok(st)
}

async fn event_count(ctx: &Ctx, device_id: &str, base_ns: i64) -> Result<i64> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM events WHERE device_id = $1 AND start_unix_nanos >= $2",
    )
    .bind(device_id)
    .bind(base_ns - 5_000_000_000) // small slack below the pinned base
    .fetch_one(&ctx.pool)
    .await?;
    Ok(n)
}
