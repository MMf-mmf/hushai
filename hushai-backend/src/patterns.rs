//! Gotham baselines + anomaly emission (Wave 2 / Pillar G2) — the "patterns" producer (§1.6).
//!
//! The pure math lives in [`crate::graph`] (histogram fold, dwell percentiles, the anomaly
//! predicates, `EntityBaseline`, dedup keys); this file is the SQL, called from inside the single
//! advisory-locked `graph_pass` transaction. For every subject TOUCHED this pass it:
//!   1. recomputes the subject's `entity_baselines` row over the trailing window (capture-anchored:
//!      the window ends at the subject's latest event, so pinned-capture fixtures are deterministic),
//!   2. judges each window visit for `off_schedule_presence` AS-OF — against the histogram of the
//!      subject's STRICTLY-EARLIER visits (the spec's incremental "new visit vs the prior baseline"
//!      model), so first appearances (incl. the enrollment clip) never fire; only a later visit that
//!      violates an established rhythm does. Deterministic + identical for rebuild and the worker,
//!   3. emits deduped `pattern_anomaly` events ON THE TRANSACTION (`anom:<kind>:<subject>:<day>`).
//!
//! The event_ids emitted this pass are returned so the WORKER driver can alert-evaluate them
//! post-commit: the alert evaluator lives in the worker crate and the backend cannot reach it, so
//! the pass only PRODUCES the events + reports their ids (Gotham.md §1.6 / Phase D).
//!
//! WAVE-2 SCOPE: `off_schedule_presence` + the baseline recompute that feeds it — the Phase-D
//! exemplar ("an off-schedule visit fires an alert rule end-to-end"). The other three predicates
//! (`first_time_pairing`, `unknown_person_cluster`, `new_vehicle_for_person`) have pure cores in
//! [`crate::graph`] already; wiring them is a documented follow-up.
//!
//! This file ALSO owns the G2 **daily digest** producer ([`build_and_upsert_digest`], Phase E): a
//! deterministic structured summary of one civil day's activity (new entities, top visitors,
//! anomalies, conversations, first-time pairings, journeys) rendered by a template — NO LLM at write
//! time (the `hushai-rag::analytics::render_digest` discipline; the RAG/G3 layer narrates at READ
//! time). Materialized on demand for a pinned date (`graph_pass::generate_digest`, the eval + admin
//! path) or by the worker-0 wall-clock driver (`graph_pass::maybe_generate_daily_digest`).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use crate::graph::{
    self, AnomalyKind, BaselineVisit, CompanionTally, EntityBaseline, GraphCfg, NodeType,
    EVENT_TYPE_PATTERN_ANOMALY,
};

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Recompute baselines + emit off-schedule anomalies for every touched subject, inside the
/// `graph_pass` transaction. Returns the anomaly event_ids emitted this pass (for the worker driver
/// to alert-evaluate post-commit). `touched` is the `(subject_type, subject_id)` set drained this
/// pass; `tz_offset_secs` buckets local civil time; `cfg_hash` stamps each baseline row.
pub async fn recompute_and_flag(
    tx: &mut Transaction<'_, Postgres>,
    cfg: &GraphCfg,
    tz_offset_secs: i64,
    cfg_hash: &str,
    touched: &BTreeSet<(String, Uuid)>,
) -> anyhow::Result<Vec<Uuid>> {
    let mut anomaly_ids: Vec<Uuid> = Vec::new();
    let window_nanos = cfg.baseline_window_days.max(1) * 86_400 * NANOS_PER_SEC;
    let gap_nanos = cfg.copresence_slack_secs.max(1) * NANOS_PER_SEC;

    for (stype, sid) in touched {
        let node_type = match stype.as_str() {
            "person" => NodeType::Person,
            "speaker" => NodeType::Speaker,
            "plate" => NodeType::Plate,
            _ => continue, // devices carry no baseline
        };

        // The subject's events; trailing window ends at its latest event (capture-anchored, so a
        // pinned-capture fixture settles deterministically — never wall clock).
        let rows = sqlx::query(
            "SELECT device_id, start_unix_nanos, end_unix_nanos FROM events \
             WHERE subject_type = $1 AND subject_id = $2 AND device_id IS NOT NULL \
               AND event_type NOT IN ('pattern_anomaly', 'gotham_briefing') \
             ORDER BY start_unix_nanos ASC",
        )
        .bind(stype)
        .bind(sid)
        .fetch_all(&mut **tx)
        .await?;
        if rows.is_empty() {
            continue;
        }
        let max_end = rows.iter().map(|r| r.get::<i64, _>("end_unix_nanos")).max().unwrap_or(0);
        let win_lo = max_end - window_nanos;
        let raw: Vec<(String, i64, i64)> = rows
            .iter()
            .filter_map(|r| {
                let end: i64 = r.get("end_unix_nanos");
                if end < win_lo {
                    return None;
                }
                Some((r.get::<String, _>("device_id"), r.get::<i64, _>("start_unix_nanos"), end))
            })
            .collect();
        if raw.is_empty() {
            continue;
        }
        let visits = coalesce_visits(&raw, gap_nanos);

        let companions = load_companions(tx, node_type, *sid).await?;
        let baseline = graph::build_baseline(&visits, &companions, tz_offset_secs);
        upsert_baseline(tx, stype, *sid, cfg.baseline_window_days, &baseline, cfg_hash).await?;

        let label = resolve_label(tx, node_type, *sid).await?;
        let subject_key = format!("{stype}:{sid}");
        // AS-OF off_schedule: judge each visit, in capture order, against the histogram of the
        // subject's STRICTLY-EARLIER visits (the spec's incremental "new visit vs the prior
        // baseline" model). `visits` is start-sorted by `coalesce_visits`. A subject's first
        // appearances — including the enrollment clip an hour before the case — never fire (their
        // prior baseline is immature); only a later visit that violates an ESTABLISHED rhythm does.
        // This is deterministic and identical for a whole-scenario rebuild and the incremental
        // worker (re-judging old visits is idempotent via the civil-day dedup key).
        let mut prior_hist = vec![0i32; 168];
        for v in &visits {
            let bucket = graph::hour_of_week(v.start_unix_nanos, tz_offset_secs);
            if graph::is_off_schedule(&prior_hist, bucket, cfg) {
                let day = graph::civil_day(v.start_unix_nanos, tz_offset_secs);
                let dedup = graph::anom_dedup_key(
                    AnomalyKind::OffSchedulePresence,
                    &subject_key,
                    &day.to_string(),
                );
                let meta = json!({
                    "kind": AnomalyKind::OffSchedulePresence.as_str(),
                    "hour_bucket": bucket as i64,
                });
                if let Some(id) = emit_anomaly(
                    tx,
                    stype,
                    *sid,
                    label.as_deref(),
                    &v.device_id,
                    v.start_unix_nanos,
                    v.end_unix_nanos,
                    &dedup,
                    &meta,
                )
                .await?
                {
                    anomaly_ids.push(id);
                }
            }
            // Fold this visit into the running prior only AFTER judging it (as-of).
            if bucket < prior_hist.len() {
                prior_hist[bucket] += 1;
            }
        }
    }
    Ok(anomaly_ids)
}

/// Coalesce `(device, start, end)` tuples into per-device visits (gap-merged, same rule as the edge
/// visit coalescer in `graph_pass`). Deterministic: sort by start then device.
fn coalesce_visits(raw: &[(String, i64, i64)], gap_nanos: i64) -> Vec<BaselineVisit> {
    let mut sorted = raw.to_vec();
    sorted.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let mut out: Vec<BaselineVisit> = Vec::new();
    for (dev, start, end) in sorted {
        match out.last_mut() {
            Some(v) if v.device_id == dev && start - v.end_unix_nanos <= gap_nanos => {
                v.end_unix_nanos = v.end_unix_nanos.max(end);
            }
            _ => out.push(BaselineVisit {
                device_id: dev,
                start_unix_nanos: start,
                end_unix_nanos: end,
            }),
        }
    }
    out
}

/// Companion tallies from the subject's `co_present` edges (the other endpoint + observation_count).
async fn load_companions(
    tx: &mut Transaction<'_, Postgres>,
    node_type: NodeType,
    id: Uuid,
) -> anyhow::Result<Vec<CompanionTally>> {
    let st = node_type.as_str();
    let sid = id.to_string();
    let rows = sqlx::query(
        "SELECT CASE WHEN src_type = $1 AND src_id = $2 THEN dst_type ELSE src_type END AS other_type, \
                CASE WHEN src_type = $1 AND src_id = $2 THEN dst_id ELSE src_id END AS other_id, \
                observation_count \
         FROM entity_edges \
         WHERE edge_type = 'co_present' \
           AND ((src_type = $1 AND src_id = $2) OR (dst_type = $1 AND dst_id = $2))",
    )
    .bind(st)
    .bind(&sid)
    .fetch_all(&mut **tx)
    .await?;
    let mut out = Vec::new();
    for r in &rows {
        let ot: String = r.get("other_type");
        let Some(nt) = NodeType::parse(&ot) else { continue };
        out.push(CompanionTally {
            node_type: nt,
            node_id: r.get("other_id"),
            observations: r.get("observation_count"),
        });
    }
    Ok(out)
}

async fn upsert_baseline(
    tx: &mut Transaction<'_, Postgres>,
    subject_type: &str,
    subject_id: Uuid,
    window_days: i64,
    b: &EntityBaseline,
    cfg_hash: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO entity_baselines \
           (subject_type, subject_id, window_days, hour_histogram, visits_in_window, \
            dwell_p50_secs, dwell_p90_secs, device_stats, companion_stats, config_hash, computed_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10, now()) \
         ON CONFLICT (subject_type, subject_id) DO UPDATE SET \
           window_days = $3, hour_histogram = $4, visits_in_window = $5, \
           dwell_p50_secs = $6, dwell_p90_secs = $7, device_stats = $8, \
           companion_stats = $9, config_hash = $10, computed_at = now()",
    )
    .bind(subject_type)
    .bind(subject_id)
    .bind(window_days as i32)
    .bind(&b.hour_histogram)
    .bind(b.visits_in_window as i32)
    .bind(b.dwell_p50_secs)
    .bind(b.dwell_p90_secs)
    .bind(&b.device_stats)
    .bind(&b.companion_stats)
    .bind(cfg_hash)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The subject's human display name for the anomaly event's `subject_label` (best-effort; the eval
/// resolves assertions by enrolled name → subject_id, but the label aids the events modality + UI).
async fn resolve_label(
    tx: &mut Transaction<'_, Postgres>,
    node_type: NodeType,
    id: Uuid,
) -> anyhow::Result<Option<String>> {
    let sql = match node_type {
        NodeType::Person => "SELECT display_name FROM persons WHERE person_id = $1",
        NodeType::Speaker => "SELECT display_name FROM speakers WHERE speaker_id = $1",
        NodeType::Plate => {
            "SELECT COALESCE(display_name, plate_text) FROM license_plates WHERE plate_id = $1"
        }
        NodeType::Device => return Ok(None),
    };
    let label: Option<Option<String>> =
        sqlx::query_scalar(sql).bind(id).fetch_optional(&mut **tx).await?;
    Ok(label.flatten())
}

/// Emit one `pattern_anomaly` event on the transaction, idempotent by `dedup_key` (the 0014
/// partial unique index). Returns `Some(event_id)` ONLY when a NEW row is inserted; `None` when the
/// anomaly already existed for this subject/day (`ON CONFLICT ... DO NOTHING` returns no row). This
/// is what makes `anomalies_emitted` count genuinely-fresh anomalies and the worker alert-evaluate
/// each one exactly once — re-judging an already-fired historical visit on a later pass is a no-op.
/// severity is always `warning`.
#[allow(clippy::too_many_arguments)]
async fn emit_anomaly(
    tx: &mut Transaction<'_, Postgres>,
    subject_type: &str,
    subject_id: Uuid,
    subject_label: Option<&str>,
    device_id: &str,
    start_ns: i64,
    end_ns: i64,
    dedup_key: &str,
    metadata: &serde_json::Value,
) -> anyhow::Result<Option<Uuid>> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO events \
           (event_id, device_id, event_type, severity, subject_type, subject_id, subject_label, \
            start_unix_nanos, end_unix_nanos, metadata, dedup_key, created_at, updated_at) \
         VALUES ($1,$2,$3,'warning',$4,$5,$6,$7,$8,$9,$10, now(), now()) \
         ON CONFLICT (dedup_key) WHERE dedup_key IS NOT NULL \
         DO NOTHING \
         RETURNING event_id",
    )
    .bind(Uuid::now_v7())
    .bind(device_id)
    .bind(EVENT_TYPE_PATTERN_ANOMALY)
    .bind(subject_type)
    .bind(subject_id)
    .bind(subject_label)
    .bind(start_ns)
    .bind(end_ns)
    .bind(metadata)
    .bind(dedup_key)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(id)
}

// ---------------------------------------------------------------------------------------------
// Daily digest (§1.6 / migration 0029) — deterministic structured facts + template render, no LLM
// ---------------------------------------------------------------------------------------------

/// One subject reference in a digest section (resolved label is best-effort).
struct SubjectRow {
    subject_type: String,
    subject_id: Uuid,
    label: Option<String>,
}
/// One top-visitor tally.
struct Visitor {
    subject_type: String,
    subject_id: Uuid,
    label: Option<String>,
    visits: i64,
}
/// One anomaly line (from the pinned day's `pattern_anomaly` events).
struct AnomRow {
    kind: Option<String>,
    subject_type: Option<String>,
    subject_id: Option<String>,
    label: Option<String>,
    hour_bucket: Option<i64>,
}
/// One first-time co-presence pairing (a `co_present` edge first observed on the day).
struct PairRow {
    a_type: String,
    a_id: String,
    a_label: Option<String>,
    b_type: String,
    b_id: String,
    b_label: Option<String>,
}
/// One journey row (Wave 4 — the query is forward-compatible; empty until the stitcher ships).
struct JourneyRow {
    subject_type: String,
    subject_id: String,
    label: Option<String>,
    hop_count: i64,
}

/// Per-subject `(device, start, end)` event tuples for one civil day (digest visit-coalescing input).
type DayVisits = BTreeMap<(String, Uuid), Vec<(String, i64, i64)>>;

/// Build + upsert the deterministic daily digest for `civil_day` (§1.6 / migration 0029). NO LLM:
/// `sections` is structured integer/label facts and `rendered_text` is a template render (the
/// `hushai-rag::analytics::render_digest` discipline). Idempotent by the `digest_date` PK. Returns
/// the `sections` jsonb for the caller (the admin endpoint / eval / tests).
///
/// The digest for civil day D covers activity whose START falls in D's local-civil window
/// `[D*86400 - tz, (D+1)*86400 - tz)` seconds — capture-anchored, never wall clock — reading the
/// same sessionized sources as the graph: `events` (EXCLUDING the graph's own `pattern_anomaly`/
/// `gotham_briefing` output — the no-self-fold rule), closed `conversations`, materialized
/// `co_present` edges, and `entity_journeys`. Deterministic throughout: `BTreeMap` subject order,
/// total tie-breaks on every sort.
pub async fn build_and_upsert_digest(
    tx: &mut Transaction<'_, Postgres>,
    cfg: &GraphCfg,
    tz_offset_secs: i64,
    cfg_hash: &str,
    civil_day: i64,
) -> anyhow::Result<Value> {
    let day_lo = (civil_day * 86_400 - tz_offset_secs) * NANOS_PER_SEC;
    let day_hi = ((civil_day + 1) * 86_400 - tz_offset_secs) * NANOS_PER_SEC;
    let gap_nanos = cfg.copresence_slack_secs.max(1) * NANOS_PER_SEC;

    // The civil date as an ISO string — Postgres does the day-count → date conversion (robust vs
    // hand-rolled calendar math). It is the `digest_date` PK and is echoed in `sections.date`.
    let date_iso: String = sqlx::query_scalar("SELECT (DATE '1970-01-01' + ($1::int))::text")
        .bind(civil_day as i32)
        .fetch_one(&mut **tx)
        .await?;

    // 1. Perception events on the day → per-subject coalesced visits (top_visitors).
    let ev_rows = sqlx::query(
        "SELECT subject_type, subject_id, device_id, start_unix_nanos, end_unix_nanos \
         FROM events \
         WHERE subject_id IS NOT NULL AND device_id IS NOT NULL \
           AND subject_type = ANY(ARRAY['person','speaker','plate']) \
           AND event_type NOT IN ('pattern_anomaly', 'gotham_briefing') \
           AND start_unix_nanos >= $1 AND start_unix_nanos < $2 \
         ORDER BY subject_type, subject_id, device_id, start_unix_nanos",
    )
    .bind(day_lo)
    .bind(day_hi)
    .fetch_all(&mut **tx)
    .await?;
    let mut per_subject: DayVisits = DayVisits::new();
    for r in &ev_rows {
        per_subject
            .entry((r.get::<String, _>("subject_type"), r.get::<Uuid, _>("subject_id")))
            .or_default()
            .push((
                r.get::<String, _>("device_id"),
                r.get::<i64, _>("start_unix_nanos"),
                r.get::<i64, _>("end_unix_nanos"),
            ));
    }
    let mut visitors: Vec<Visitor> = Vec::new();
    for ((stype, sid), raw) in &per_subject {
        let Some(nt) = NodeType::parse(stype) else { continue };
        let visits = coalesce_visits(raw, gap_nanos).len() as i64;
        let label = resolve_label(tx, nt, *sid).await?;
        visitors.push(Visitor { subject_type: stype.clone(), subject_id: *sid, label, visits });
    }
    // top_visitors: visits desc, then type, then id (total order).
    visitors.sort_by(|a, b| {
        b.visits
            .cmp(&a.visits)
            .then_with(|| a.subject_type.cmp(&b.subject_type))
            .then_with(|| a.subject_id.cmp(&b.subject_id))
    });
    let total_visitors = visitors.len();
    visitors.truncate(graph::DIGEST_TOP_VISITORS);

    // 2. new_entities: subjects whose FIRST-EVER perception event (any device/day) lands on the day.
    let new_rows = sqlx::query(
        "SELECT subject_type, subject_id FROM ( \
           SELECT subject_type, subject_id, MIN(start_unix_nanos) AS first_start \
           FROM events \
           WHERE subject_id IS NOT NULL \
             AND subject_type = ANY(ARRAY['person','speaker','plate']) \
             AND event_type NOT IN ('pattern_anomaly', 'gotham_briefing') \
           GROUP BY subject_type, subject_id \
         ) f \
         WHERE f.first_start >= $1 AND f.first_start < $2 \
         ORDER BY subject_type, subject_id",
    )
    .bind(day_lo)
    .bind(day_hi)
    .fetch_all(&mut **tx)
    .await?;
    let mut new_entities: Vec<SubjectRow> = Vec::new();
    for r in &new_rows {
        let stype: String = r.get("subject_type");
        let sid: Uuid = r.get("subject_id");
        let Some(nt) = NodeType::parse(&stype) else { continue };
        let label = resolve_label(tx, nt, sid).await?;
        new_entities.push(SubjectRow { subject_type: stype, subject_id: sid, label });
    }

    // 3. anomalies on the day (the graph's OWN output — labelled at emit time; read directly).
    let anom_rows = sqlx::query(
        "SELECT subject_type, subject_id, subject_label, metadata->>'kind' AS kind, \
                metadata->>'hour_bucket' AS hour_bucket \
         FROM events \
         WHERE event_type = 'pattern_anomaly' \
           AND start_unix_nanos >= $1 AND start_unix_nanos < $2 \
         ORDER BY metadata->>'kind', subject_type, subject_id",
    )
    .bind(day_lo)
    .bind(day_hi)
    .fetch_all(&mut **tx)
    .await?;
    let anomalies: Vec<AnomRow> = anom_rows
        .iter()
        .map(|r| AnomRow {
            kind: r.get::<Option<String>, _>("kind"),
            subject_type: r.get::<Option<String>, _>("subject_type"),
            subject_id: r.get::<Option<Uuid>, _>("subject_id").map(|u| u.to_string()),
            label: r.get::<Option<String>, _>("subject_label"),
            hour_bucket: r.get::<Option<String>, _>("hour_bucket").and_then(|s| s.parse().ok()),
        })
        .collect();

    // 4. conversations closed on the day: count + distinct participants.
    let conv_count: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM conversations \
         WHERE status = 'closed' AND started_at_unix_nanos >= $1 AND started_at_unix_nanos < $2",
    )
    .bind(day_lo)
    .bind(day_hi)
    .fetch_one(&mut **tx)
    .await?;
    let conv_participants: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT s)::bigint \
         FROM conversations c, unnest(c.speaker_ids) AS s \
         WHERE c.status = 'closed' AND c.started_at_unix_nanos >= $1 AND c.started_at_unix_nanos < $2",
    )
    .bind(day_lo)
    .bind(day_hi)
    .fetch_one(&mut **tx)
    .await?;

    // 5. first_time_pairings: `co_present` edges whose first_seen lands on the day (0→1 transition).
    let pair_rows = sqlx::query(
        "SELECT src_type, src_id, dst_type, dst_id FROM entity_edges \
         WHERE edge_type = 'co_present' \
           AND first_seen_unix_nanos >= $1 AND first_seen_unix_nanos < $2 \
         ORDER BY src_type, src_id, dst_type, dst_id",
    )
    .bind(day_lo)
    .bind(day_hi)
    .fetch_all(&mut **tx)
    .await?;
    let mut pairings: Vec<PairRow> = Vec::new();
    for r in &pair_rows {
        let (a_type, a_id): (String, String) = (r.get("src_type"), r.get("src_id"));
        let (b_type, b_id): (String, String) = (r.get("dst_type"), r.get("dst_id"));
        let a_label = endpoint_label(tx, &a_type, &a_id).await?;
        let b_label = endpoint_label(tx, &b_type, &b_id).await?;
        pairings.push(PairRow { a_type, a_id, a_label, b_type, b_id, b_label });
    }

    // 6. journeys started on the day (Wave 4 — empty until the stitcher ships; query is ready).
    let journey_rows = sqlx::query(
        "SELECT subject_type, subject_id, hop_count FROM entity_journeys \
         WHERE started_at_unix_nanos >= $1 AND started_at_unix_nanos < $2 \
         ORDER BY subject_type, subject_id, started_at_unix_nanos",
    )
    .bind(day_lo)
    .bind(day_hi)
    .fetch_all(&mut **tx)
    .await?;
    let mut journeys: Vec<JourneyRow> = Vec::new();
    for r in &journey_rows {
        let stype: String = r.get("subject_type");
        let sid: Uuid = r.get("subject_id");
        let label = match NodeType::parse(&stype) {
            Some(nt) => resolve_label(tx, nt, sid).await?,
            None => None,
        };
        journeys.push(JourneyRow {
            subject_type: stype,
            subject_id: sid.to_string(),
            label,
            hop_count: r.get::<i32, _>("hop_count") as i64,
        });
    }

    // Assemble deterministic `sections` jsonb + a template `rendered_text` (no LLM).
    let sections = json!({
        "date": date_iso,
        "new_entities": new_entities.iter().map(subject_json).collect::<Vec<_>>(),
        "anomalies": anomalies.iter().map(anom_json).collect::<Vec<_>>(),
        "top_visitors": visitors.iter().map(visitor_json).collect::<Vec<_>>(),
        "conversations": { "count": conv_count, "participants": conv_participants },
        "first_time_pairings": pairings.iter().map(pair_json).collect::<Vec<_>>(),
        "journeys": journeys.iter().map(journey_json).collect::<Vec<_>>(),
        "counts": {
            "new_entities": new_entities.len(),
            "anomalies": anomalies.len(),
            "top_visitors": total_visitors,       // distinct visitors (pre-truncation)
            "conversations": conv_count,
            "first_time_pairings": pairings.len(),
            "journeys": journeys.len(),
        },
    });
    let rendered_text = render_digest_text(
        &date_iso,
        &new_entities,
        &visitors,
        total_visitors,
        &anomalies,
        conv_count,
        conv_participants,
        &pairings,
        &journeys,
    );

    sqlx::query(
        "INSERT INTO daily_digests \
           (digest_date, tz_offset_secs, sections, rendered_text, config_hash, created_at, updated_at) \
         VALUES ($1::date, $2, $3, $4, $5, now(), now()) \
         ON CONFLICT (digest_date) DO UPDATE SET \
           tz_offset_secs = $2, sections = $3, rendered_text = $4, config_hash = $5, updated_at = now()",
    )
    .bind(&date_iso)
    .bind(tz_offset_secs as i32)
    .bind(&sections)
    .bind(&rendered_text)
    .bind(cfg_hash)
    .execute(&mut **tx)
    .await?;

    Ok(sections)
}

/// Best-effort display label for a graph edge endpoint `(type, id-string)`. Device endpoints ARE
/// their id; person/speaker/plate resolve through the catalog (None when unnamed or the id is not a
/// parseable uuid).
async fn endpoint_label(
    tx: &mut Transaction<'_, Postgres>,
    node_type: &str,
    node_id: &str,
) -> anyhow::Result<Option<String>> {
    match NodeType::parse(node_type) {
        Some(NodeType::Device) | None => Ok(Some(node_id.to_string())),
        Some(nt) => match Uuid::parse_str(node_id) {
            Ok(id) => resolve_label(tx, nt, id).await,
            Err(_) => Ok(None),
        },
    }
}

fn label_or(l: &Option<String>) -> &str {
    l.as_deref().unwrap_or("unidentified")
}

fn subject_json(s: &SubjectRow) -> Value {
    json!({ "type": s.subject_type, "id": s.subject_id.to_string(), "label": s.label })
}
fn visitor_json(v: &Visitor) -> Value {
    json!({ "type": v.subject_type, "id": v.subject_id.to_string(), "label": v.label, "visits": v.visits })
}
fn anom_json(a: &AnomRow) -> Value {
    json!({ "kind": a.kind, "subject_type": a.subject_type, "subject_id": a.subject_id, "label": a.label, "hour_bucket": a.hour_bucket })
}
fn pair_json(p: &PairRow) -> Value {
    json!({
        "a": { "type": p.a_type, "id": p.a_id, "label": p.a_label },
        "b": { "type": p.b_type, "id": p.b_id, "label": p.b_label },
    })
}
fn journey_json(j: &JourneyRow) -> Value {
    json!({ "subject_type": j.subject_type, "subject_id": j.subject_id, "label": j.label, "hop_count": j.hop_count })
}

/// Deterministic template render of the digest — the read-time-narration boundary (§1.6): the RAG/G3
/// layer turns this into prose; the store keeps only facts. One line per section; empty sections say
/// so explicitly (never silently dropped — the `render_digest` discipline).
#[allow(clippy::too_many_arguments)]
fn render_digest_text(
    date: &str,
    new_entities: &[SubjectRow],
    top_visitors: &[Visitor],
    total_visitors: usize,
    anomalies: &[AnomRow],
    conv_count: i64,
    conv_participants: i64,
    pairings: &[PairRow],
    journeys: &[JourneyRow],
) -> String {
    let plural = |n: i64| if n == 1 { "" } else { "s" };
    let mut out = format!("DAILY BRIEFING for {date}\n");

    if new_entities.is_empty() {
        out.push_str("NEW ENTITIES: none.\n");
    } else {
        let names: Vec<&str> = new_entities.iter().map(|e| label_or(&e.label)).collect();
        out.push_str(&format!("NEW ENTITIES: {} ({}).\n", new_entities.len(), names.join(", ")));
    }

    if total_visitors == 0 {
        out.push_str("VISITORS: none seen.\n");
    } else {
        let parts: Vec<String> = top_visitors
            .iter()
            .map(|v| format!("{} ({} visit{})", label_or(&v.label), v.visits, plural(v.visits)))
            .collect();
        let more = total_visitors.saturating_sub(top_visitors.len());
        let tail = if more > 0 { format!(" (+{more} more)") } else { String::new() };
        out.push_str(&format!(
            "VISITORS: {} subject{} seen — {}{}.\n",
            total_visitors,
            if total_visitors == 1 { "" } else { "s" },
            parts.join(", "),
            tail
        ));
    }

    out.push_str(&format!(
        "CONVERSATIONS: {conv_count} across {conv_participants} participant{}.\n",
        plural(conv_participants)
    ));

    if anomalies.is_empty() {
        out.push_str("ANOMALIES: none.\n");
    } else {
        let parts: Vec<String> = anomalies
            .iter()
            .map(|a| format!("{} ({})", a.kind.as_deref().unwrap_or("anomaly"), label_or(&a.label)))
            .collect();
        out.push_str(&format!("ANOMALIES: {} — {}.\n", anomalies.len(), parts.join(", ")));
    }

    if pairings.is_empty() {
        out.push_str("FIRST-TIME PAIRINGS: none.\n");
    } else {
        let parts: Vec<String> = pairings
            .iter()
            .map(|p| format!("{} & {}", label_or(&p.a_label), label_or(&p.b_label)))
            .collect();
        out.push_str(&format!("FIRST-TIME PAIRINGS: {} — {}.\n", pairings.len(), parts.join(", ")));
    }

    if journeys.is_empty() {
        out.push_str("JOURNEYS: none.\n");
    } else {
        out.push_str(&format!("JOURNEYS: {}.\n", journeys.len()));
    }
    out
}
