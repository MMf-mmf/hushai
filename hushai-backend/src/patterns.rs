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

use std::collections::BTreeSet;

use serde_json::json;
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
