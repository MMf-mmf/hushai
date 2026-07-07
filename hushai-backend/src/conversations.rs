//! Conversation threader — DB orchestration around the pure core in [`crate::threading`]
//! (migration 0025).
//!
//! Driver: worker 0 calls [`thread_pass`] on an interval (BEFORE claiming, so sustained
//! ingest load can never starve threading — deliberately NOT the drain-only autoheal
//! cadence). All mutating passes serialize on the `CONVO_LOCK_KEY` advisory lock; the
//! live `write_transcript` path never touches `conversation_id`, so the only writer to
//! the assignment columns is this module.
//!
//! Watermark semantics (the 0024 profiles idiom, globalized): each pass consumes
//! sentences with `created_at > threader_state.watermark AND created_at <= now() -
//! min_age` (the lag lets speaker attribution / autoheal settle before threading sees a
//! row). Wall-clock, not capture time — a reprocessed old-capture backlog gets fresh
//! `created_at` and re-enters the scan naturally. Consumed rows route two ways:
//!   * start time inside a CLOSED conversation's span on that device → append-only
//!     late-attach (closed boundaries are frozen; see the 0025 mutability contract);
//!   * everything else → normal threading over the BATCH'S CAPTURE WINDOW (± lookback)
//!     plus the device's open-conversation members. The window is capture-time around
//!     the batch, never the wall clock, so a backlog upload (an offline phone's
//!     store-and-forward day, an eval fixture pinned in the past) threads in its own
//!     capture-time context instead of silently staying NULL. Pre-feature history that
//!     never re-enters the scan stays NULL until an explicit [`thread_backfill`].
//!
//! A config-hash change (any threader knob) re-threads every device's open tail under
//! the new config on the next pass; closed conversations keep the hash they were closed
//! under (eval lineage provenance).

use std::collections::HashMap;

use serde_json::json;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::events::NewEvent;
use crate::threading::{self, SentenceIn, ThreaderCfg};

/// Global threader advisory lock key ("hconv"). Distinct from the speaker and profile
/// locks: the threader never mutates the speaker catalog or profiles, so no deadlock
/// ordering to worry about.
pub const CONVO_LOCK_KEY: i64 = 0x6863_6f6e_76;

const NANOS_PER_SEC: i64 = 1_000_000_000;

#[derive(Debug, Clone)]
pub struct ThreaderOpts {
    pub cfg: ThreaderCfg,
    /// Sentences younger than this (created_at) are not yet threadable.
    pub min_age_secs: i64,
    /// Open-tail revision window (capture time): unassigned sentences whose start is
    /// within this window join the working set; older unassigned rows wait for backfill.
    pub lookback_secs: i64,
    /// Per-pass scan bound (embeddings held in RAM for the working set).
    pub max_rows_per_pass: i64,
    /// A conversation closes when now - ended_at > gap + grace.
    pub close_grace_secs: i64,
    /// Cross-device linking (link, never merge).
    pub link_enabled: bool,
    /// Fraction of the SHORTER conversation's span that must overlap to link.
    pub link_min_overlap_frac: f64,
}

impl Default for ThreaderOpts {
    fn default() -> Self {
        Self {
            cfg: ThreaderCfg::default(),
            min_age_secs: 10,
            lookback_secs: 900,
            max_rows_per_pass: 5000,
            close_grace_secs: 120,
            link_enabled: true,
            link_min_overlap_frac: 0.5,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ThreadStats {
    pub rows_scanned: u64,
    pub rows_assigned: u64,
    pub late_attached: u64,
    pub convos_minted: u64,
    pub convos_deleted: u64,
    pub convos_closed: u64,
    pub links_created: u64,
}

// ---------------------------------------------------------------------------------------------
// thread_pass — the interval driver
// ---------------------------------------------------------------------------------------------

/// One threading pass over everything new since the watermark. Cheap when idle (one
/// indexed scan finding nothing). Never touches closed conversations except append-only
/// late-attach.
pub async fn thread_pass(pool: &PgPool, opts: &ThreaderOpts) -> anyhow::Result<ThreadStats> {
    let mut stats = ThreadStats::default();
    let hash = threading::config_hash(&opts.cfg);

    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(CONVO_LOCK_KEY)
        .execute(&mut *tx)
        .await?;

    // Migration 0025 seeds the singleton, but a harness TRUNCATE (hushai-eval reset) or a
    // hand-cleaned DB removes it — self-heal with the epoch default instead of erroring
    // every pass forever (fetch_one on zero rows).
    sqlx::query("INSERT INTO threader_state (id) VALUES (1) ON CONFLICT (id) DO NOTHING")
        .execute(&mut *tx)
        .await?;
    let state = sqlx::query("SELECT watermark, config_hash FROM threader_state WHERE id = 1")
        .fetch_one(&mut *tx)
        .await?;
    let stored_hash: Option<String> = state.get("config_hash");
    let rethread_all = stored_hash.as_deref() != Some(hash.as_str());

    // 1. Scan new rows past the watermark, older than the settle lag. created_at rides
    //    as exact epoch micros (timestamptz is µs-resolution) — no chrono dep, same trick
    //    as profiles.rs.
    let scanned = sqlx::query(
        r#"
        SELECT id, (EXTRACT(EPOCH FROM created_at) * 1e6)::bigint AS created_us,
               device_id, speaker_id, start_unix_nanos
        FROM transcript_sentences
        WHERE created_at > (SELECT watermark FROM threader_state WHERE id = 1)
          AND created_at <= now() - make_interval(secs => $1)
        ORDER BY created_at, id
        LIMIT $2
        "#,
    )
    .bind(opts.min_age_secs as f64)
    .bind(opts.max_rows_per_pass)
    .fetch_all(&mut *tx)
    .await?;
    stats.rows_scanned = scanned.len() as u64;

    let now_ns = unix_now_nanos();

    // Batch rows per device. The threading window derives from the BATCH'S CAPTURE TIMES
    // (± lookback), never the wall clock — a backlog upload (an offline phone's
    // store-and-forward day, an eval fixture pinned in the past, a reprocessed archive)
    // threads in its own capture-time context instead of silently staying NULL.
    let mut batch_by_device: HashMap<String, Vec<(i64, i64, Option<Uuid>)>> = HashMap::new();
    for r in &scanned {
        let Some(device) = r.get::<Option<String>, _>("device_id") else {
            continue;
        };
        let speaker = r
            .get::<Option<String>, _>("speaker_id")
            .and_then(|s| Uuid::parse_str(&s).ok());
        batch_by_device.entry(device).or_default().push((
            r.get("id"),
            r.get("start_unix_nanos"),
            speaker,
        ));
    }
    let mut devices: Vec<String> = batch_by_device.keys().cloned().collect();
    if rethread_all {
        // Config change: also re-thread every device with an open conversation.
        let extra = sqlx::query(
            "SELECT DISTINCT primary_device_id AS device_id              FROM conversations WHERE status = 'open' AND primary_device_id IS NOT NULL",
        )
        .fetch_all(&mut *tx)
        .await?;
        devices.extend(extra.iter().map(|r| r.get::<String, _>("device_id")));
    }
    devices.sort();
    devices.dedup();

    // 2. Per affected device: rows inside a CLOSED conversation's span late-attach
    //    (append-only; closed boundaries are frozen); everything else threads through
    //    the pure core over the batch's capture window + the device's open tail.
    let lookback_ns = opts.lookback_secs * NANOS_PER_SEC;
    for device in &devices {
        let batch = batch_by_device.get(device).cloned().unwrap_or_default();
        let (late_rows, live_rows) = split_by_closed_spans(&mut tx, device, &batch).await?;
        stats.late_attached += late_attach(&mut tx, device, &late_rows).await?;
        let window = if live_rows.is_empty() {
            None // rethread_all device with no new rows: open tail only
        } else {
            let lo = live_rows.iter().map(|r| r.1).min().unwrap();
            let hi = live_rows.iter().map(|r| r.1).max().unwrap();
            Some((lo.saturating_sub(lookback_ns), hi.saturating_add(lookback_ns)))
        };
        let s = thread_one_device(&mut tx, device, opts, window, &hash).await?;
        stats.rows_assigned += s.rows_assigned;
        stats.convos_minted += s.convos_minted;
        stats.convos_deleted += s.convos_deleted;
    }

    // 3. Close stale opens (global — a device with no new rows still closes out).
    stats.convos_closed = close_stale(&mut tx, opts, now_ns, &hash).await?;

    // 4. Cross-device linking over recently-updated conversations.
    if opts.link_enabled {
        stats.links_created = link_pass(&mut tx, opts).await?;
    }

    // 5. Advance the watermark to the newest consumed row (never past it: a LIMITed
    //    batch resumes exactly where it stopped). Micros round-trip exactly (timestamptz
    //    is µs-resolution).
    if let Some(last) = scanned.last() {
        let max_created_us: i64 = last.get("created_us");
        sqlx::query(
            "UPDATE threader_state SET watermark = GREATEST(watermark, to_timestamp($1::double precision / 1e6)), config_hash = $2, updated_at = now() WHERE id = 1",
        )
        .bind(max_created_us)
        .bind(&hash)
        .execute(&mut *tx)
        .await?;
    } else if rethread_all {
        sqlx::query("UPDATE threader_state SET config_hash = $1, updated_at = now() WHERE id = 1")
            .bind(&hash)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(stats)
}

fn unix_now_nanos() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Default, Clone, Copy)]
struct DeviceStats {
    rows_assigned: u64,
    convos_minted: u64,
    convos_deleted: u64,
}

/// Partition a device's batch rows into (inside-a-closed-span, everything-else). Closed
/// conversations are frozen — a reprocessed row landing inside one late-attaches instead
/// of re-threading it.
async fn split_by_closed_spans(
    tx: &mut Transaction<'_, Postgres>,
    device: &str,
    batch: &[(i64, i64, Option<Uuid>)],
) -> anyhow::Result<(Vec<(i64, i64, Option<Uuid>)>, Vec<(i64, i64, Option<Uuid>)>)> {
    if batch.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let lo = batch.iter().map(|r| r.1).min().unwrap();
    let hi = batch.iter().map(|r| r.1).max().unwrap();
    let spans: Vec<(i64, i64)> = sqlx::query(
        r#"
        SELECT started_at_unix_nanos, ended_at_unix_nanos FROM conversations
        WHERE primary_device_id = $1 AND status = 'closed'
          AND started_at_unix_nanos <= $3 AND ended_at_unix_nanos >= $2
        "#,
    )
    .bind(device)
    .bind(lo)
    .bind(hi)
    .fetch_all(&mut **tx)
    .await?
    .iter()
    .map(|r| (r.get("started_at_unix_nanos"), r.get("ended_at_unix_nanos")))
    .collect();
    let mut late = Vec::new();
    let mut live = Vec::new();
    for &row in batch {
        if spans.iter().any(|&(s, e)| s <= row.1 && row.1 <= e) {
            late.push(row);
        } else {
            live.push(row);
        }
    }
    Ok((late, live))
}

/// Append-only late-attach: a reprocessed sentence whose capture time falls inside a
/// CLOSED conversation's span joins it without reopening or recomputing boundaries.
async fn late_attach(
    tx: &mut Transaction<'_, Postgres>,
    device: &str,
    old_rows: &[(i64, i64, Option<Uuid>)],
) -> anyhow::Result<u64> {
    if old_rows.is_empty() {
        return Ok(0);
    }
    let lo = old_rows.iter().map(|r| r.1).min().unwrap();
    let hi = old_rows.iter().map(|r| r.1).max().unwrap();
    let spans = sqlx::query(
        r#"
        SELECT conversation_id, started_at_unix_nanos, ended_at_unix_nanos
        FROM conversations
        WHERE primary_device_id = $1 AND status = 'closed'
          AND started_at_unix_nanos <= $3 AND ended_at_unix_nanos >= $2
        ORDER BY started_at_unix_nanos
        "#,
    )
    .bind(device)
    .bind(lo)
    .bind(hi)
    .fetch_all(&mut **tx)
    .await?;
    if spans.is_empty() {
        return Ok(0);
    }
    let mut attached = 0u64;
    for &(id, start, speaker) in old_rows {
        let Some(span) = spans.iter().find(|s| {
            s.get::<i64, _>("started_at_unix_nanos") <= start
                && start <= s.get::<i64, _>("ended_at_unix_nanos")
        }) else {
            continue;
        };
        let cid: Uuid = span.get("conversation_id");
        // Turn: inherit the nearest preceding member's turn (append-only; no reindex).
        let turn: i32 = sqlx::query(
            r#"
            SELECT turn_index FROM transcript_sentences
            WHERE conversation_id = $1 AND start_unix_nanos <= $2
            ORDER BY start_unix_nanos DESC LIMIT 1
            "#,
        )
        .bind(cid)
        .bind(start)
        .fetch_optional(&mut **tx)
        .await?
        .and_then(|r| r.get::<Option<i32>, _>("turn_index"))
        .unwrap_or(0);
        let n = sqlx::query(
            "UPDATE transcript_sentences SET conversation_id = $1, turn_index = $2 WHERE id = $3",
        )
        .bind(cid)
        .bind(turn)
        .bind(id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if n > 0 {
            attached += 1;
            sqlx::query(
                r#"
                UPDATE conversations SET
                    sentence_count = sentence_count + 1,
                    speaker_ids = CASE
                        WHEN $2::uuid IS NULL OR speaker_ids @> ARRAY[$2::uuid]
                            THEN speaker_ids
                        ELSE array_append(speaker_ids, $2::uuid)
                    END,
                    updated_at = now()
                WHERE conversation_id = $1
                "#,
            )
            .bind(cid)
            .bind(speaker)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(attached)
}

/// Load one device's working set (open-conversation members + unassigned rows inside the
/// batch's capture window, all settled), run the pure core, apply the diff, upsert/delete
/// conversation rows. `window = None` (config-change re-thread with no new rows) keeps the
/// open tail only.
async fn thread_one_device(
    tx: &mut Transaction<'_, Postgres>,
    device: &str,
    opts: &ThreaderOpts,
    window: Option<(i64, i64)>,
    hash: &str,
) -> anyhow::Result<DeviceStats> {
    let mut stats = DeviceStats::default();

    // Working set. NB speaker_id is TEXT on this table (0006 contract) — parse to uuid.
    // The NULL-row window is CAPTURE time around the batch (backlog threads in its own
    // context); (i64::MAX, i64::MIN) makes the NULL clause match nothing when window=None.
    let (win_lo, win_hi) = window.unwrap_or((i64::MAX, i64::MIN));
    let rows = sqlx::query(
        r#"
        SELECT ts.id, ts.speaker_id, ts.start_unix_nanos, ts.end_unix_nanos,
               ts.embedding, ts.conversation_id, ts.turn_index
        FROM transcript_sentences ts
        WHERE ts.device_id = $1
          AND ts.created_at <= now() - make_interval(secs => $2)
          AND (
                ts.conversation_id IN (
                    SELECT conversation_id FROM conversations
                    WHERE primary_device_id = $1 AND status = 'open')
             OR (ts.conversation_id IS NULL
                 AND ts.start_unix_nanos >= $3 AND ts.start_unix_nanos <= $4)
          )
        ORDER BY ts.start_unix_nanos, ts.id
        "#,
    )
    .bind(device)
    .bind(opts.min_age_secs as f64)
    .bind(win_lo)
    .bind(win_hi)
    .fetch_all(&mut **tx)
    .await?;
    if rows.is_empty() {
        return Ok(stats);
    }

    let mut prior: std::collections::HashMap<i64, (Option<Uuid>, Option<i32>)> =
        std::collections::HashMap::new();
    let sentences: Vec<SentenceIn> = rows
        .iter()
        .map(|r| {
            let id: i64 = r.get("id");
            let prior_cid: Option<Uuid> = r.get("conversation_id");
            let prior_turn: Option<i32> = r.get("turn_index");
            prior.insert(id, (prior_cid, prior_turn));
            SentenceIn {
                id,
                speaker_id: r
                    .get::<Option<String>, _>("speaker_id")
                    .and_then(|s| Uuid::parse_str(&s).ok()),
                start_unix_nanos: r.get("start_unix_nanos"),
                end_unix_nanos: r.get("end_unix_nanos"),
                embedding: r
                    .get::<Option<pgvector::Vector>, _>("embedding")
                    .map(|v| v.to_vec()),
                prior_conversation_id: prior_cid,
            }
        })
        .collect();

    let mut mint = Uuid::now_v7;
    let result = threading::thread_device(sentences, &opts.cfg, &mut mint);

    // Diff-only assignment updates (bounded UPDATE churn on the partitioned table).
    let mut upd_ids: Vec<i64> = Vec::new();
    let mut upd_cids: Vec<Uuid> = Vec::new();
    let mut upd_turns: Vec<i32> = Vec::new();
    for a in &result.assignments {
        let (pc, pt) = prior.get(&a.sentence_id).copied().unwrap_or((None, None));
        if pc != Some(a.conversation_id) || pt != Some(a.turn_index) {
            upd_ids.push(a.sentence_id);
            upd_cids.push(a.conversation_id);
            upd_turns.push(a.turn_index);
        }
    }
    if !upd_ids.is_empty() {
        sqlx::query(
            r#"
            UPDATE transcript_sentences AS t
            SET conversation_id = u.cid, turn_index = u.turn
            FROM (SELECT unnest($1::bigint[]) AS id, unnest($2::uuid[]) AS cid,
                         unnest($3::int[]) AS turn) AS u
            WHERE t.id = u.id
            "#,
        )
        .bind(&upd_ids)
        .bind(&upd_cids)
        .bind(&upd_turns)
        .execute(&mut **tx)
        .await?;
        stats.rows_assigned = upd_ids.len() as u64;
    }

    // Upsert conversation rows (open until the close pass says otherwise).
    for c in &result.conversations {
        sqlx::query(
            r#"
            INSERT INTO conversations (
                conversation_id, primary_device_id, device_ids,
                started_at_unix_nanos, ended_at_unix_nanos, status,
                speaker_ids, sentence_count, config_hash
            )
            VALUES ($1, $2, ARRAY[$2], $3, $4, 'open', $5, $6, $7)
            ON CONFLICT (conversation_id) DO UPDATE SET
                started_at_unix_nanos = EXCLUDED.started_at_unix_nanos,
                ended_at_unix_nanos   = EXCLUDED.ended_at_unix_nanos,
                speaker_ids           = EXCLUDED.speaker_ids,
                sentence_count        = EXCLUDED.sentence_count,
                config_hash           = EXCLUDED.config_hash,
                updated_at            = now()
            "#,
        )
        .bind(c.conversation_id)
        .bind(device)
        .bind(c.started_at_unix_nanos)
        .bind(c.ended_at_unix_nanos)
        .bind(&c.speaker_ids)
        .bind(c.sentence_count as i32)
        .bind(hash)
        .execute(&mut **tx)
        .await?;
        if c.minted {
            stats.convos_minted += 1;
        }
    }

    // Open conversations of this device that lost every sentence this pass die.
    let keep: Vec<Uuid> = result
        .conversations
        .iter()
        .map(|c| c.conversation_id)
        .collect();
    let deleted = sqlx::query(
        r#"
        DELETE FROM conversations
        WHERE primary_device_id = $1 AND status = 'open'
          AND conversation_id <> ALL($2::uuid[])
        "#,
    )
    .bind(device)
    .bind(&keep)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    stats.convos_deleted = deleted;

    Ok(stats)
}

/// Close conversations whose tail has been silent past gap + grace, emitting ONE
/// `conversation` event each (dedup_key makes re-emission idempotent).
async fn close_stale(
    tx: &mut Transaction<'_, Postgres>,
    opts: &ThreaderOpts,
    now_ns: i64,
    hash: &str,
) -> anyhow::Result<u64> {
    let cutoff_ns =
        now_ns - ((opts.cfg.gap_secs as i64) + opts.close_grace_secs) * NANOS_PER_SEC;
    // TWO conditions: the conversation's tail is silent past gap+grace in CAPTURE time,
    // AND the row hasn't been touched for the grace in WALL time. The second guard is
    // what keeps a backlog upload (capture times far in the past — an offline phone's
    // day, an eval fixture) from closing between threader passes while its batches are
    // still streaming in; without it every pass minted a NEW conversation because the
    // previous fragment was already frozen (the probe's "conversation ×4" fragmentation).
    let closed = sqlx::query(
        r#"
        UPDATE conversations
        SET status = 'closed', config_hash = $2, updated_at = now()
        WHERE status = 'open' AND ended_at_unix_nanos < $1
          AND updated_at < now() - make_interval(secs => $3)
        RETURNING conversation_id, primary_device_id, device_ids,
                  started_at_unix_nanos, ended_at_unix_nanos, speaker_ids, sentence_count
        "#,
    )
    .bind(cutoff_ns)
    .bind(hash)
    .bind(opts.close_grace_secs.max(0) as f64)
    .fetch_all(&mut **tx)
    .await?;

    for row in &closed {
        let cid: Uuid = row.get("conversation_id");
        let speaker_ids: Vec<Uuid> = row.get("speaker_ids");
        let label = conversation_label(tx, &speaker_ids).await?;
        let ev = NewEvent {
            device_id: row.get("primary_device_id"),
            event_type: "conversation".into(),
            severity: "info".into(),
            subject_type: None,
            subject_id: None,
            subject_label: Some(label),
            segment_id: None,
            start_unix_nanos: row.get("started_at_unix_nanos"),
            end_unix_nanos: row.get("ended_at_unix_nanos"),
            score: None,
            metadata: json!({
                "conversation_id": cid,
                "speaker_ids": speaker_ids,
                "device_ids": row.get::<Vec<String>, _>("device_ids"),
                "sentence_count": row.get::<i32, _>("sentence_count"),
            }),
            dedup_key: Some(format!("convo:{cid}")),
        };
        // record_event takes a pool; inside our tx we inline the same idempotent insert.
        record_event_in_tx(tx, &ev).await?;
    }
    Ok(closed.len() as u64)
}

/// "Alice, Bob + 1 unidentified" from the speaker catalog; "unattributed voices" when empty.
async fn conversation_label(
    tx: &mut Transaction<'_, Postgres>,
    speaker_ids: &[Uuid],
) -> anyhow::Result<String> {
    if speaker_ids.is_empty() {
        return Ok("unattributed voices".into());
    }
    let rows = sqlx::query(
        "SELECT display_name FROM speakers WHERE speaker_id = ANY($1::uuid[]) ORDER BY display_name NULLS LAST",
    )
    .bind(speaker_ids)
    .fetch_all(&mut **tx)
    .await?;
    let named: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get::<Option<String>, _>("display_name"))
        .collect();
    let unnamed = speaker_ids.len().saturating_sub(named.len());
    if named.is_empty() {
        return Ok(format!(
            "{} unidentified speaker{}",
            unnamed,
            if unnamed == 1 { "" } else { "s" }
        ));
    }
    let mut label = named.join(", ");
    if unnamed > 0 {
        label.push_str(&format!(" + {unnamed} unidentified"));
    }
    Ok(label)
}

/// [`record_event`]'s UPSERT, executable inside the threader's transaction.
async fn record_event_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    ev: &NewEvent,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO events (
            event_id, device_id, event_type, severity, subject_type, subject_id,
            subject_label, segment_id, start_unix_nanos, end_unix_nanos, score, metadata, dedup_key
        )
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
        ON CONFLICT (dedup_key) WHERE dedup_key IS NOT NULL
        DO UPDATE SET
            end_unix_nanos = GREATEST(events.end_unix_nanos, EXCLUDED.end_unix_nanos),
            subject_label  = COALESCE(EXCLUDED.subject_label, events.subject_label),
            metadata       = EXCLUDED.metadata,
            updated_at     = now()
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(&ev.device_id)
    .bind(&ev.event_type)
    .bind(&ev.severity)
    .bind(&ev.subject_type)
    .bind(ev.subject_id)
    .bind(&ev.subject_label)
    .bind(ev.segment_id)
    .bind(ev.start_unix_nanos)
    .bind(ev.end_unix_nanos)
    .bind(ev.score)
    .bind(&ev.metadata)
    .bind(&ev.dedup_key)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// link_pass — cross-device LINK (v1: shared-speaker evidence only; never a merge)
// ---------------------------------------------------------------------------------------------

/// Link conversations on DIFFERENT devices that overlap in wall-clock time and share at
/// least one participant (speaker identity is already global/cross-device). Sharing a
/// link_group_id is presentation-level: consumers may render a group as one logical
/// conversation; transcripts stay per-device (merging would interleave duplicate ASR
/// text of the same audio).
async fn link_pass(tx: &mut Transaction<'_, Postgres>, opts: &ThreaderOpts) -> anyhow::Result<u64> {
    let pairs = sqlx::query(
        r#"
        SELECT a.conversation_id AS a_id, b.conversation_id AS b_id,
               a.link_group_id  AS a_grp, b.link_group_id  AS b_grp
        FROM conversations a
        JOIN conversations b
          ON a.conversation_id < b.conversation_id
         AND a.primary_device_id <> b.primary_device_id
         AND a.speaker_ids && b.speaker_ids
         AND a.started_at_unix_nanos < b.ended_at_unix_nanos
         AND b.started_at_unix_nanos < a.ended_at_unix_nanos
        WHERE a.updated_at > now() - interval '1 hour'
           OR b.updated_at > now() - interval '1 hour'
        "#,
    )
    .fetch_all(&mut **tx)
    .await?;

    let mut created = 0u64;
    for p in &pairs {
        let (a_id, b_id): (Uuid, Uuid) = (p.get("a_id"), p.get("b_id"));
        // Overlap fraction of the shorter span.
        let frac_ok = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT (LEAST(a.ended_at_unix_nanos, b.ended_at_unix_nanos)
                  - GREATEST(a.started_at_unix_nanos, b.started_at_unix_nanos))::float8
                 / GREATEST(1, LEAST(a.ended_at_unix_nanos - a.started_at_unix_nanos,
                                     b.ended_at_unix_nanos - b.started_at_unix_nanos))::float8
                 >= $3
            FROM conversations a, conversations b
            WHERE a.conversation_id = $1 AND b.conversation_id = $2
            "#,
        )
        .bind(a_id)
        .bind(b_id)
        .bind(opts.link_min_overlap_frac)
        .fetch_one(&mut **tx)
        .await?;
        if !frac_ok {
            continue;
        }
        let a_grp: Option<Uuid> = p.get("a_grp");
        let b_grp: Option<Uuid> = p.get("b_grp");
        let group = match (a_grp, b_grp) {
            (Some(g1), Some(g2)) => g1.min(g2),
            (Some(g), None) | (None, Some(g)) => g,
            (None, None) => Uuid::now_v7(),
        };
        let n = sqlx::query(
            r#"
            UPDATE conversations SET link_group_id = $1, updated_at = now()
            WHERE (conversation_id = $2 OR conversation_id = $3
                   OR link_group_id IN ($4, $5))
              AND link_group_id IS DISTINCT FROM $1
            "#,
        )
        .bind(group)
        .bind(a_id)
        .bind(b_id)
        .bind(a_grp.unwrap_or(group))
        .bind(b_grp.unwrap_or(group))
        .execute(&mut **tx)
        .await?
        .rows_affected();
        created += n;
    }
    Ok(created)
}

// ---------------------------------------------------------------------------------------------
// thread_backfill — explicit historical pass
// ---------------------------------------------------------------------------------------------

/// Thread historical rows in `[from_ns, to_ns)`. `rethread=false` touches only
/// `conversation_id IS NULL` rows (pre-feature history); `rethread=true` recomputes
/// everything in range (dev/eval tool — it will revise CLOSED conversations). Chunks by
/// device × day through the same pure core; each chunk pads one gap-width on both sides
/// so a conversation straddling a chunk edge lands whole.
pub async fn thread_backfill(
    pool: &PgPool,
    opts: &ThreaderOpts,
    from_ns: i64,
    to_ns: i64,
    rethread: bool,
) -> anyhow::Result<ThreadStats> {
    let mut stats = ThreadStats::default();
    let hash = threading::config_hash(&opts.cfg);
    let day_ns: i64 = 86_400 * NANOS_PER_SEC;
    let pad_ns = (opts.cfg.gap_secs as i64) * NANOS_PER_SEC;

    let devices: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT DISTINCT device_id FROM transcript_sentences
        WHERE device_id IS NOT NULL AND start_unix_nanos >= $1 AND start_unix_nanos < $2
        ORDER BY device_id
        "#,
    )
    .bind(from_ns)
    .bind(to_ns)
    .fetch_all(pool)
    .await?;

    for device in &devices {
        let mut chunk_start = from_ns;
        while chunk_start < to_ns {
            let chunk_end = (chunk_start + day_ns).min(to_ns);
            let mut tx = pool.begin().await?;
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(CONVO_LOCK_KEY)
                .execute(&mut *tx)
                .await?;

            let rows = sqlx::query(
                r#"
                SELECT ts.id, ts.speaker_id, ts.start_unix_nanos, ts.end_unix_nanos,
                       ts.embedding, ts.conversation_id
                FROM transcript_sentences ts
                WHERE ts.device_id = $1
                  AND ts.start_unix_nanos >= $2 - $4 AND ts.start_unix_nanos < $3 + $4
                  AND ($5 OR ts.conversation_id IS NULL)
                ORDER BY ts.start_unix_nanos, ts.id
                "#,
            )
            .bind(device)
            .bind(chunk_start)
            .bind(chunk_end)
            .bind(pad_ns)
            .bind(rethread)
            .fetch_all(&mut *tx)
            .await?;

            if !rows.is_empty() {
                let sentences: Vec<SentenceIn> = rows
                    .iter()
                    .map(|r| SentenceIn {
                        id: r.get("id"),
                        speaker_id: r
                            .get::<Option<String>, _>("speaker_id")
                            .and_then(|s| Uuid::parse_str(&s).ok()),
                        start_unix_nanos: r.get("start_unix_nanos"),
                        end_unix_nanos: r.get("end_unix_nanos"),
                        embedding: r
                            .get::<Option<pgvector::Vector>, _>("embedding")
                            .map(|v| v.to_vec()),
                        prior_conversation_id: r.get("conversation_id"),
                    })
                    .collect();
                let mut mint = Uuid::now_v7;
                let result = threading::thread_device(sentences, &opts.cfg, &mut mint);

                let ids: Vec<i64> = result.assignments.iter().map(|a| a.sentence_id).collect();
                let cids: Vec<Uuid> = result
                    .assignments
                    .iter()
                    .map(|a| a.conversation_id)
                    .collect();
                let turns: Vec<i32> = result.assignments.iter().map(|a| a.turn_index).collect();
                sqlx::query(
                    r#"
                    UPDATE transcript_sentences AS t
                    SET conversation_id = u.cid, turn_index = u.turn
                    FROM (SELECT unnest($1::bigint[]) AS id, unnest($2::uuid[]) AS cid,
                                 unnest($3::int[]) AS turn) AS u
                    WHERE t.id = u.id
                    "#,
                )
                .bind(&ids)
                .bind(&cids)
                .bind(&turns)
                .execute(&mut *tx)
                .await?;
                stats.rows_assigned += ids.len() as u64;

                for c in &result.conversations {
                    sqlx::query(
                        r#"
                        INSERT INTO conversations (
                            conversation_id, primary_device_id, device_ids,
                            started_at_unix_nanos, ended_at_unix_nanos, status,
                            speaker_ids, sentence_count, config_hash
                        )
                        VALUES ($1, $2, ARRAY[$2], $3, $4, 'closed', $5, $6, $7)
                        ON CONFLICT (conversation_id) DO UPDATE SET
                            started_at_unix_nanos = EXCLUDED.started_at_unix_nanos,
                            ended_at_unix_nanos   = EXCLUDED.ended_at_unix_nanos,
                            speaker_ids           = EXCLUDED.speaker_ids,
                            sentence_count        = EXCLUDED.sentence_count,
                            config_hash           = EXCLUDED.config_hash,
                            updated_at            = now()
                        "#,
                    )
                    .bind(c.conversation_id)
                    .bind(device)
                    .bind(c.started_at_unix_nanos)
                    .bind(c.ended_at_unix_nanos)
                    .bind(&c.speaker_ids)
                    .bind(c.sentence_count as i32)
                    .bind(&hash)
                    .execute(&mut *tx)
                    .await?;
                    if c.minted {
                        stats.convos_minted += 1;
                    }
                }
            }
            tx.commit().await?;
            chunk_start = chunk_end;
        }
    }
    Ok(stats)
}
