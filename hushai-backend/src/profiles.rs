//! Entity profiles — accumulated "running memory" per identity (migration 0024).
//!
//! The chat should get to KNOW the people it deals with: every face identity (`persons`) and
//! voice identity (`speakers`) accumulates one observation line per coalesced visit /
//! conversation, folded incrementally from the already-sessionized `events` table. Anonymous
//! identities accumulate too — the moment one is named, its whole history is already attached
//! (naming never re-keys a profile; merging duplicate identities folds their profiles).
//!
//! DETERMINISTIC BY DESIGN: no LLM anywhere in accumulation (the RAG service narrates the
//! profile at chat time — the reflection-agent precedent of "deterministic digest, narrate at
//! answer time"). Everything here is rebuildable from `events` + `transcript_sentences`.
//!
//! Drivers: the worker calls [`update_all`] at drain time (beside speaker auto-heal) to keep
//! profiles warm; the RAG People arm calls [`refresh_subject`] on demand so a profile answer
//! is never staler than the events table. Both serialize on the `PROFILE_LOCK_KEY` advisory
//! lock so lines are never double-appended.
//!
//! Watermark semantics (the subtle part): each profile consumes events with
//! `updated_at > last_event_at`, EXCLUDING (a) events younger than a grace window (a 30s
//! session bucket row keeps being UPSERT-extended until the visit moves on — grace ≥ 2×
//! bucket guarantees it settled) and (b) events whose `end_unix_nanos` is within the visit
//! gap of now (an in-progress visit must not be split across passes). Wall-clock watermark —
//! not capture time — survives the worker reprocessing an old-capture backlog late; each
//! line carries its own capture date, so rare out-of-order appends are harmless.

use sqlx::{PgPool, Postgres, QueryBuilder, Row, Transaction};
use uuid::Uuid;

/// Global profile-writer advisory lock key ("hprof"). Distinct from SPEAKER_LOCK_KEY: profile
/// folding never mutates the speaker/person catalogs, so the two locks can't deadlock.
const PROFILE_LOCK_KEY: i64 = 0x6870_726f_66;

const NANOS_PER_SEC: i64 = 1_000_000_000;

#[derive(Debug, Clone)]
pub struct ProfileOpts {
    /// Person sightings closer than this are one visit (mirror of PRESENCE_VISIT_GAP_SECS).
    pub visit_gap_secs: i64,
    /// Speech events closer than this are one conversation (mirror of CONVERSATION_GAP_SECS).
    pub convo_gap_secs: i64,
    /// Ignore events updated more recently than this — lets a session bucket settle.
    pub grace_secs: i64,
    /// Per-pass event budget per subject type (backfill converges over a few passes).
    pub max_events_per_pass: i64,
    /// Profile text cap; oldest lines are compacted into one rollup line beyond it.
    pub max_chars: usize,
    /// Fixed offset for rendering the `[YYYY-MM-DD HH:MM]` line prefixes in local civil time.
    pub tz_offset_secs: i64,
}

impl Default for ProfileOpts {
    fn default() -> Self {
        Self {
            visit_gap_secs: 120,
            convo_gap_secs: 300,
            grace_secs: 90,
            max_events_per_pass: 2000,
            max_chars: 8000,
            tz_offset_secs: 0,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ProfileStats {
    pub events_consumed: u64,
    pub profiles_touched: u64,
}

/// One stored profile row, as the chat surfaces it.
#[derive(Debug, Clone)]
pub struct ProfileRow {
    pub profile_text: String,
    pub visit_count: i64,
    pub first_seen_unix_nanos: Option<i64>,
    pub last_seen_unix_nanos: Option<i64>,
}

/// A consumed event, minimal fields needed for folding. Public only because the pure folding
/// cores that take it are (they're unit-tested and reused); constructed nowhere else.
#[derive(Debug, Clone)]
pub struct Ev {
    subject_id: Uuid,
    device_id: String,
    subject_label: Option<String>,
    start_unix_nanos: i64,
    end_unix_nanos: i64,
    updated_at_micros: i64, // epoch micros (watermark math without a chrono dep)
}

/// One coalesced visit/conversation for one subject.
#[derive(Debug, Clone, PartialEq)]
pub struct Visit {
    pub device_id: String,
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
}

// ---------------------------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------------------------

/// Fold new events into ALL profiles (both subject types). The worker's drain-time driver.
pub async fn update_all(pool: &PgPool, opts: &ProfileOpts) -> anyhow::Result<ProfileStats> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(PROFILE_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let mut stats = ProfileStats::default();
    for st in ["person", "speaker"] {
        let s = fold_subject_events(&mut tx, st, None, opts).await?;
        stats.events_consumed += s.events_consumed;
        stats.profiles_touched += s.profiles_touched;
    }
    tx.commit().await?;
    Ok(stats)
}

/// Fold new events for ONE subject (the chat-time freshen — bounded, cheap when idle).
pub async fn refresh_subject(
    pool: &PgPool,
    subject_type: &str,
    subject_id: Uuid,
    opts: &ProfileOpts,
) -> anyhow::Result<ProfileStats> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(PROFILE_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let stats = fold_subject_events(&mut tx, subject_type, Some(subject_id), opts).await?;
    tx.commit().await?;
    Ok(stats)
}

/// Read one profile (None when the identity has no accumulated history yet).
pub async fn get_profile(
    pool: &PgPool,
    subject_type: &str,
    subject_id: Uuid,
) -> anyhow::Result<Option<ProfileRow>> {
    let row = sqlx::query(
        "SELECT profile_text, visit_count, first_seen_unix_nanos, last_seen_unix_nanos \
         FROM entity_profiles WHERE subject_type = $1 AND subject_id = $2",
    )
    .bind(subject_type)
    .bind(subject_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| ProfileRow {
        profile_text: r.get("profile_text"),
        visit_count: r.get("visit_count"),
        first_seen_unix_nanos: r.get("first_seen_unix_nanos"),
        last_seen_unix_nanos: r.get("last_seen_unix_nanos"),
    }))
}

/// Fold the LOSER identity's profile into the SURVIVOR's on a merge — call inside the merge
/// transaction, BEFORE the loser row is deleted from its catalog. Watermark = GREATEST
/// (deliberate, documented loss: loser events not yet consumed at merge time stay orphaned —
/// merges don't repoint `events.subject_id` today; taking LEAST would re-consume the
/// survivor's events and duplicate lines).
pub async fn merge_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    subject_type: &str,
    loser: Uuid,
    survivor: Uuid,
) -> Result<(), sqlx::Error> {
    let loser_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM entity_profiles WHERE subject_type = $1 AND subject_id = $2)",
    )
    .bind(subject_type)
    .bind(loser)
    .fetch_one(&mut **tx)
    .await?;
    if !loser_exists {
        return Ok(()); // loser never accumulated anything
    }
    let survivor_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM entity_profiles WHERE subject_type = $1 AND subject_id = $2)",
    )
    .bind(subject_type)
    .bind(survivor)
    .fetch_one(&mut **tx)
    .await?;
    if !survivor_exists {
        sqlx::query(
            "UPDATE entity_profiles SET subject_id = $3, updated_at = now() \
             WHERE subject_type = $1 AND subject_id = $2",
        )
        .bind(subject_type)
        .bind(loser)
        .bind(survivor)
        .execute(&mut **tx)
        .await?;
        return Ok(());
    }
    // Fold entirely in SQL (no timestamptz decode — this crate has no chrono).
    sqlx::query(
        "UPDATE entity_profiles p SET \
           profile_text = left(p.profile_text || E'\\n--- merged duplicate identity ---\\n' || l.profile_text, 60000), \
           visit_count = p.visit_count + l.visit_count, \
           first_seen_unix_nanos = LEAST(COALESCE(p.first_seen_unix_nanos, l.first_seen_unix_nanos), \
                                         COALESCE(l.first_seen_unix_nanos, p.first_seen_unix_nanos)), \
           last_seen_unix_nanos = GREATEST(COALESCE(p.last_seen_unix_nanos, l.last_seen_unix_nanos), \
                                           COALESCE(l.last_seen_unix_nanos, p.last_seen_unix_nanos)), \
           last_event_at = GREATEST(p.last_event_at, l.last_event_at), \
           updated_at = now() \
         FROM entity_profiles l \
         WHERE p.subject_type = $1 AND p.subject_id = $2 \
           AND l.subject_type = $1 AND l.subject_id = $3",
    )
    .bind(subject_type)
    .bind(survivor)
    .bind(loser)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM entity_profiles WHERE subject_type = $1 AND subject_id = $2")
        .bind(subject_type)
        .bind(loser)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Record the identification moment ("[date] identified as <name>") on a rename — the beat
/// where an anonymous identity's accumulated picture attaches to a name. No-op profile-wise
/// beyond the line (profiles are keyed by id, so nothing moves). `now_unix_nanos` is passed
/// in so callers (and tests) control the clock.
pub async fn note_identified_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    subject_type: &str,
    subject_id: Uuid,
    name: &str,
    now_unix_nanos: i64,
    tz_offset_secs: i64,
) -> Result<(), sqlx::Error> {
    let line = format!(
        "[{}] identified as {}.",
        civil_stamp(now_unix_nanos, tz_offset_secs),
        name.trim()
    );
    sqlx::query(
        "INSERT INTO entity_profiles (subject_type, subject_id, profile_text) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (subject_type, subject_id) DO UPDATE SET \
           profile_text = entity_profiles.profile_text || E'\\n' || $3, \
           updated_at = now()",
    )
    .bind(subject_type)
    .bind(subject_id)
    .bind(&line)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Folding core
// ---------------------------------------------------------------------------------------------

/// Consume settled, unconsumed events for `subject_type` (optionally one subject), coalesce
/// into visits/conversations, render lines, and upsert the profiles. Runs inside the caller's
/// locked transaction.
async fn fold_subject_events(
    tx: &mut Transaction<'_, Postgres>,
    subject_type: &str,
    only_subject: Option<Uuid>,
    opts: &ProfileOpts,
) -> anyhow::Result<ProfileStats> {
    let gap_secs = if subject_type == "person" { opts.visit_gap_secs } else { opts.convo_gap_secs };
    let gap_nanos = gap_secs.max(1) * NANOS_PER_SEC;
    // "now" for the in-progress guard: DB clock, so injected fixtures with pinned capture
    // times still settle by wall time.
    let now_ns: i64 = sqlx::query_scalar("SELECT (extract(epoch from now()) * 1e9)::bigint")
        .fetch_one(&mut **tx)
        .await?;

    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT e.subject_id, e.device_id, e.subject_label, e.start_unix_nanos, e.end_unix_nanos, \
                (extract(epoch from e.updated_at) * 1e6)::bigint AS updated_micros \
         FROM events e \
         LEFT JOIN entity_profiles p ON p.subject_type = ",
    );
    qb.push_bind(subject_type)
        .push(" AND p.subject_id = e.subject_id WHERE e.subject_type = ")
        .push_bind(subject_type)
        .push(" AND e.subject_id IS NOT NULL AND e.event_type = ANY(")
        .push_bind(if subject_type == "person" {
            vec!["known_person".to_string(), "unknown_person".to_string()]
        } else {
            vec!["speech".to_string()]
        })
        .push(") AND e.updated_at > COALESCE(p.last_event_at, to_timestamp(0))")
        .push(" AND e.updated_at < now() - make_interval(secs => ")
        .push_bind(opts.grace_secs.max(0) as f64)
        .push(") AND e.end_unix_nanos <= ")
        .push_bind(now_ns.saturating_sub(gap_nanos));
    if let Some(sid) = only_subject {
        qb.push(" AND e.subject_id = ").push_bind(sid);
    }
    qb.push(" ORDER BY e.updated_at ASC LIMIT ")
        .push_bind(opts.max_events_per_pass.max(1));
    let rows = qb.build().fetch_all(&mut **tx).await?;
    if rows.is_empty() {
        return Ok(ProfileStats::default());
    }
    let events: Vec<Ev> = rows
        .into_iter()
        .map(|r| {
            Ok::<_, sqlx::Error>(Ev {
                subject_id: r.try_get("subject_id")?,
                device_id: r.try_get::<Option<String>, _>("device_id")?.unwrap_or_default(),
                subject_label: r.try_get("subject_label")?,
                start_unix_nanos: r.try_get("start_unix_nanos")?,
                end_unix_nanos: r.try_get("end_unix_nanos")?,
                updated_at_micros: r.try_get("updated_micros")?,
            })
        })
        .collect::<Result<_, _>>()?;
    let consumed = events.len() as u64;

    // Per-subject folding. BTreeMap for a deterministic subject order.
    let mut by_subject: std::collections::BTreeMap<Uuid, Vec<&Ev>> = Default::default();
    for e in &events {
        by_subject.entry(e.subject_id).or_default().push(e);
    }

    // Device display names for the line prefixes (one small query).
    let device_names = device_name_map(tx).await?;

    let mut touched = 0u64;
    for (subject_id, evs) in &by_subject {
        let visits = coalesce_events_to_visits(evs, gap_nanos);
        if visits.is_empty() {
            continue;
        }
        let mut lines: Vec<String> = Vec::with_capacity(visits.len());
        for v in &visits {
            let others = co_present(&events, *subject_id, v, gap_nanos);
            let line = if subject_type == "person" {
                render_visit_line(v, &others, &device_names, opts.tz_offset_secs)
            } else {
                let snippet = topic_snippet(tx, &v.device_id, v.start_unix_nanos, v.end_unix_nanos).await?;
                render_convo_line(v, &others, snippet.as_deref(), &device_names, opts.tz_offset_secs)
            };
            lines.push(line);
        }
        let watermark_micros = evs.iter().map(|e| e.updated_at_micros).max().unwrap_or(0);
        let first = visits.first().map(|v| v.start_unix_nanos);
        let last = visits.last().map(|v| v.end_unix_nanos);
        upsert_profile(
            tx,
            subject_type,
            *subject_id,
            &lines,
            visits.len() as i64,
            first,
            last,
            watermark_micros,
            opts.max_chars,
        )
        .await?;
        touched += 1;
    }
    Ok(ProfileStats { events_consumed: consumed, profiles_touched: touched })
}

/// Append the new lines to a profile (creating it if absent), compacting past the char cap.
#[allow(clippy::too_many_arguments)]
async fn upsert_profile(
    tx: &mut Transaction<'_, Postgres>,
    subject_type: &str,
    subject_id: Uuid,
    new_lines: &[String],
    added_visits: i64,
    first_ns: Option<i64>,
    last_ns: Option<i64>,
    watermark_micros: i64,
    max_chars: usize,
) -> anyhow::Result<()> {
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT profile_text FROM entity_profiles \
         WHERE subject_type = $1 AND subject_id = $2 FOR UPDATE",
    )
    .bind(subject_type)
    .bind(subject_id)
    .fetch_optional(&mut **tx)
    .await?;
    let mut text = existing.unwrap_or_default();
    for l in new_lines {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(l);
    }
    let text = compact_profile_text(&text, max_chars);
    sqlx::query(
        "INSERT INTO entity_profiles (subject_type, subject_id, profile_text, visit_count, \
             first_seen_unix_nanos, last_seen_unix_nanos, last_event_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, to_timestamp($7::double precision / 1e6), now()) \
         ON CONFLICT (subject_type, subject_id) DO UPDATE SET \
           profile_text = $3, \
           visit_count = entity_profiles.visit_count + $4, \
           first_seen_unix_nanos = LEAST(COALESCE(entity_profiles.first_seen_unix_nanos, $5), COALESCE($5, entity_profiles.first_seen_unix_nanos)), \
           last_seen_unix_nanos = GREATEST(COALESCE(entity_profiles.last_seen_unix_nanos, $6), COALESCE($6, entity_profiles.last_seen_unix_nanos)), \
           last_event_at = GREATEST(entity_profiles.last_event_at, to_timestamp($7::double precision / 1e6)), \
           updated_at = now()",
    )
    .bind(subject_type)
    .bind(subject_id)
    .bind(&text)
    .bind(added_visits)
    .bind(first_ns)
    .bind(last_ns)
    .bind(watermark_micros)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Everyone ELSE (distinct labels) whose event overlaps this visit on the same device — the
/// "also present" clause. Looks only within the current batch: co-presence is contemporaneous
/// by definition, so the overlapping events settle in the same passes; a rare batch split
/// costs one mention, never correctness.
fn co_present(all: &[Ev], subject: Uuid, v: &Visit, slack_nanos: i64) -> Vec<String> {
    let mut named: std::collections::BTreeSet<String> = Default::default();
    let mut unnamed: std::collections::BTreeSet<Uuid> = Default::default();
    for e in all {
        if e.subject_id == subject || e.device_id != v.device_id {
            continue;
        }
        let overlaps = e.start_unix_nanos < v.end_unix_nanos + slack_nanos
            && e.end_unix_nanos > v.start_unix_nanos - slack_nanos;
        if !overlaps {
            continue;
        }
        match &e.subject_label {
            Some(n) if !n.trim().is_empty() => {
                named.insert(n.trim().to_string());
            }
            _ => {
                unnamed.insert(e.subject_id);
            }
        }
    }
    let mut out: Vec<String> = named.into_iter().collect();
    match unnamed.len() {
        0 => {}
        1 => out.push("one unidentified person".to_string()),
        n => out.push(format!("{n} unidentified people")),
    }
    out
}

/// First words actually said in the conversation span (topic hint for the line).
async fn topic_snippet(
    tx: &mut Transaction<'_, Postgres>,
    device_id: &str,
    start_ns: i64,
    end_ns: i64,
) -> anyhow::Result<Option<String>> {
    let rows = sqlx::query_scalar::<_, Option<String>>(
        "SELECT text FROM transcript_sentences \
         WHERE device_id = $1 AND start_unix_nanos >= $2 AND start_unix_nanos < $3 \
           AND text IS NOT NULL \
         ORDER BY start_unix_nanos ASC LIMIT 2",
    )
    .bind(device_id)
    .bind(start_ns)
    .bind(end_ns.max(start_ns) + NANOS_PER_SEC)
    .fetch_all(&mut **tx)
    .await?;
    let joined = rows.into_iter().flatten().collect::<Vec<_>>().join(" ");
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let short: String = trimmed.chars().take(90).collect();
    Ok(Some(if short.len() < trimmed.len() { format!("{short}…") } else { short }))
}

async fn device_name_map(
    tx: &mut Transaction<'_, Postgres>,
) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let rows = sqlx::query("SELECT device_id, display_name FROM devices WHERE display_name IS NOT NULL")
        .fetch_all(&mut **tx)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<String, _>("device_id"), r.get::<String, _>("display_name")))
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Pure cores (unit-tested; no DB)
// ---------------------------------------------------------------------------------------------

/// Coalesce ONE subject's events into visits: sort by start, merge when the gap between one
/// event's end and the next's start is within `gap_nanos` (the same rule as the RAG side's
/// `presence::coalesce_visits`, but interval-aware since events already carry spans).
pub fn coalesce_events_to_visits(events: &[&Ev], gap_nanos: i64) -> Vec<Visit> {
    let mut sorted: Vec<&Ev> = events.to_vec();
    sorted.sort_by_key(|e| e.start_unix_nanos);
    let mut visits: Vec<Visit> = Vec::new();
    for e in sorted {
        match visits.last_mut() {
            Some(v)
                if v.device_id == e.device_id
                    && e.start_unix_nanos - v.end_unix_nanos <= gap_nanos =>
            {
                v.end_unix_nanos = v.end_unix_nanos.max(e.end_unix_nanos);
            }
            _ => visits.push(Visit {
                device_id: e.device_id.clone(),
                start_unix_nanos: e.start_unix_nanos,
                end_unix_nanos: e.end_unix_nanos,
            }),
        }
    }
    visits
}

fn device_label<'a>(
    device_id: &'a str,
    names: &'a std::collections::HashMap<String, String>,
) -> &'a str {
    names.get(device_id).map(String::as_str).unwrap_or(device_id)
}

fn dur_phrase(nanos: i64) -> String {
    let mins = (nanos.max(0) + 30 * NANOS_PER_SEC) / (60 * NANOS_PER_SEC);
    if mins < 1 {
        "under a minute".to_string()
    } else {
        format!("{mins} min")
    }
}

/// `[2026-06-12 08:12] front-door: visit, 7 min; also present: Bob, one unidentified person.`
pub fn render_visit_line(
    v: &Visit,
    others: &[String],
    device_names: &std::collections::HashMap<String, String>,
    tz_offset_secs: i64,
) -> String {
    let mut line = format!(
        "[{}] {}: visit, {}",
        civil_stamp(v.start_unix_nanos, tz_offset_secs),
        device_label(&v.device_id, device_names),
        dur_phrase(v.end_unix_nanos - v.start_unix_nanos)
    );
    if !others.is_empty() {
        line.push_str(&format!("; also present: {}", others.join(", ")));
    }
    line.push('.');
    line
}

/// `[2026-06-12 14:05] kitchen: conversation, 17 min, with Alice; started: "the delivery…"`
pub fn render_convo_line(
    v: &Visit,
    others: &[String],
    snippet: Option<&str>,
    device_names: &std::collections::HashMap<String, String>,
    tz_offset_secs: i64,
) -> String {
    let mut line = format!(
        "[{}] {}: conversation, {}",
        civil_stamp(v.start_unix_nanos, tz_offset_secs),
        device_label(&v.device_id, device_names),
        dur_phrase(v.end_unix_nanos - v.start_unix_nanos)
    );
    if !others.is_empty() {
        line.push_str(&format!(", with {}", others.join(", ")));
    }
    if let Some(s) = snippet {
        line.push_str(&format!("; started: \"{s}\""));
    }
    line.push('.');
    line
}

/// Keep the newest lines under `max_chars`, folding everything older into ONE rollup line —
/// deterministic (no LLM): `…N earlier entries between 2026-05-01 and 2026-06-01 (compacted).`
pub fn compact_profile_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    // Walk from the newest line backwards, keeping while under budget (reserve ~80 chars for
    // the rollup line itself).
    let budget = max_chars.saturating_sub(80);
    let mut kept_rev: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for l in lines.iter().rev() {
        let add = l.chars().count() + 1;
        if used + add > budget && !kept_rev.is_empty() {
            break;
        }
        used += add;
        kept_rev.push(l);
    }
    let dropped = lines.len() - kept_rev.len();
    if dropped == 0 {
        return text.to_string();
    }
    let date_of = |l: &str| -> Option<String> {
        let inner = l.strip_prefix('[')?;
        let end = inner.find(']')?;
        Some(inner[..end.min(10)].to_string())
    };
    let first_date = lines.first().and_then(|l| date_of(l)).unwrap_or_default();
    let last_dropped = lines.get(dropped - 1).and_then(|l| date_of(l)).unwrap_or_default();
    let mut out = format!(
        "…{dropped} earlier entr{} between {first_date} and {last_dropped} (compacted).",
        if dropped == 1 { "y" } else { "ies" }
    );
    for l in kept_rev.iter().rev() {
        out.push('\n');
        out.push_str(l);
    }
    out
}

/// `YYYY-MM-DD HH:MM` in local civil time from UTC nanos + fixed offset. Pure integer civil
/// arithmetic (Howard Hinnant's civil_from_days) — no chrono dep in this crate.
pub fn civil_stamp(unix_nanos: i64, tz_offset_secs: i64) -> String {
    let secs = unix_nanos.div_euclid(NANOS_PER_SEC) + tz_offset_secs;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", tod / 3600, (tod % 3600) / 60)
}

/// Days-since-epoch → (year, month, day). Standard civil-calendar algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day-of-era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // year-of-era
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day-of-year [0, 365]
    let mp = (5 * doy + 2) / 153; // month index [0, 11], March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: i64 = NANOS_PER_SEC;
    // 2026-06-12 08:12:00 UTC.
    const T0: i64 = 1_781_251_920_000_000_000;

    fn ev(subject: Uuid, device: &str, label: Option<&str>, start: i64, end: i64) -> Ev {
        Ev {
            subject_id: subject,
            device_id: device.to_string(),
            subject_label: label.map(str::to_string),
            start_unix_nanos: start,
            end_unix_nanos: end,
            updated_at_micros: 0,
        }
    }

    #[test]
    fn civil_stamp_matches_known_dates() {
        assert_eq!(civil_stamp(0, 0), "1970-01-01 00:00");
        assert_eq!(civil_stamp(T0, 0), "2026-06-12 08:12");
        // Offset shifts across a midnight boundary.
        assert_eq!(civil_stamp(T0, -9 * 3600), "2026-06-11 23:12");
    }

    #[test]
    fn events_coalesce_into_visits_per_device() {
        let s = Uuid::now_v7();
        let evs = vec![
            ev(s, "cam-a", None, T0, T0 + 30 * SEC),
            ev(s, "cam-a", None, T0 + 40 * SEC, T0 + 70 * SEC), // 10s gap — same visit
            ev(s, "cam-a", None, T0 + 3600 * SEC, T0 + 3630 * SEC), // an hour later — new visit
            ev(s, "cam-b", None, T0 + 50 * SEC, T0 + 80 * SEC), // other device — its own visit
        ];
        let refs: Vec<&Ev> = evs.iter().collect();
        let visits = coalesce_events_to_visits(&refs, 120 * SEC);
        assert_eq!(visits.len(), 3);
        assert_eq!(visits[0].device_id, "cam-a");
        assert_eq!(visits[0].end_unix_nanos, T0 + 70 * SEC);
    }

    #[test]
    fn visit_line_reads_naturally() {
        let v = Visit { device_id: "cam-a".into(), start_unix_nanos: T0, end_unix_nanos: T0 + 7 * 60 * SEC };
        let mut names = std::collections::HashMap::new();
        names.insert("cam-a".to_string(), "front-door".to_string());
        let line = render_visit_line(&v, &["Bob".into(), "one unidentified person".into()], &names, 0);
        assert_eq!(
            line,
            "[2026-06-12 08:12] front-door: visit, 7 min; also present: Bob, one unidentified person."
        );
        // No co-present, unnamed device, sub-minute visit.
        let v = Visit { device_id: "cam-x".into(), start_unix_nanos: T0, end_unix_nanos: T0 + 20 * SEC };
        assert_eq!(render_visit_line(&v, &[], &names, 0), "[2026-06-12 08:12] cam-x: visit, under a minute.");
    }

    #[test]
    fn convo_line_carries_participants_and_snippet() {
        let v = Visit { device_id: "cam-a".into(), start_unix_nanos: T0, end_unix_nanos: T0 + 17 * 60 * SEC };
        let names = std::collections::HashMap::new();
        let line = render_convo_line(&v, &["Alice".into()], Some("the delivery truck came back"), &names, 0);
        assert_eq!(
            line,
            "[2026-06-12 08:12] cam-a: conversation, 17 min, with Alice; started: \"the delivery truck came back\"."
        );
    }

    #[test]
    fn co_present_names_and_counts_unnamed() {
        let me = Uuid::now_v7();
        let bob = Uuid::now_v7();
        let anon1 = Uuid::now_v7();
        let anon2 = Uuid::now_v7();
        let all = vec![
            ev(me, "cam-a", None, T0, T0 + 60 * SEC),
            ev(bob, "cam-a", Some("Bob"), T0 + 10 * SEC, T0 + 50 * SEC),
            ev(anon1, "cam-a", None, T0 + 20 * SEC, T0 + 40 * SEC),
            ev(anon2, "cam-a", None, T0 + 20 * SEC, T0 + 40 * SEC),
            ev(bob, "cam-b", Some("Bob"), T0, T0 + 60 * SEC), // other device — not co-present
        ];
        let v = Visit { device_id: "cam-a".into(), start_unix_nanos: T0, end_unix_nanos: T0 + 60 * SEC };
        let others = co_present(&all, me, &v, 0);
        assert_eq!(others, vec!["Bob".to_string(), "2 unidentified people".to_string()]);
    }

    #[test]
    fn compaction_folds_oldest_lines_and_is_stable_under_cap() {
        let lines: Vec<String> = (0..100)
            .map(|i| format!("[2026-06-{:02} 08:00] cam-a: visit, 5 min.", (i % 28) + 1))
            .collect();
        let text = lines.join("\n");
        let out = compact_profile_text(&text, 1000);
        assert!(out.chars().count() <= 1000, "stays under cap: {}", out.len());
        assert!(out.starts_with('…'), "rollup line first: {}", &out[..60.min(out.len())]);
        assert!(out.contains("earlier entries between 2026-06-01 and"), "{}", &out[..90]);
        // Under the cap it's a no-op.
        assert_eq!(compact_profile_text("short", 1000), "short");
    }
}
