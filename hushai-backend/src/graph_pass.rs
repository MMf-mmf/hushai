//! Gotham entity-graph DB orchestrator (spec: Gotham.md §1.5, Part 1).
//!
//! The `profiles.rs` sibling: a watermark-drained, advisory-locked, worker-0-driven pass that
//! folds the already-sessionized `events` (0014) + CLOSED `conversations` (0025) into the
//! materialized `entity_edges` (0028). The pure determinism lives in [`crate::graph`]; this file
//! is the SQL: idempotent upserts, merge/delete reconciliation, the owner-binding seed, and the
//! explicit rebuild.
//!
//! WAVE 1 SCOPE (Pillar G1): the five edge types — `visits_place`, `co_present`,
//! `arrived_with_vehicle`, `conversed_with`, and the `same_identity_candidate` binding trials.
//! Baselines/anomalies (0029) and journeys (0030) ship their schema now but are STITCHED in
//! Waves 2/4 (`patterns.rs` / the journey stitcher) — this pass leaves those tables untouched.
//!
//! Determinism: watermarks are WALL clock (updated_at), "now" is the DB clock (so pinned-capture
//! fixtures still settle), evidence deduped by event_id, confidences rounded to 4 decimals in the
//! pure core. Same-config re-folds are stable.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::graph::{
    self, BindCounters, EdgeKind, EvidenceSample, GraphCfg, GraphVisit, NodeRef, NodeType,
};

/// Graph-writer advisory lock key ("hgrph"). Distinct from PROFILE_LOCK_KEY (0x6870_726f_66) and
/// the identity locks: graph folding never mutates catalogs, so no deadlock-ordering interaction.
pub const GRAPH_LOCK_KEY: i64 = 0x68_67_72_70_68;

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Pass options: the hashed [`GraphCfg`] knobs plus the operational (non-hashed) budget/offset.
#[derive(Debug, Clone)]
pub struct GraphOpts {
    pub cfg: GraphCfg,
    /// Per-pass event/conversation budget (backfill converges over passes; not hashed).
    pub max_events_per_pass: i64,
    /// Fixed civil-time offset (reserved for baseline bucketing in Wave 2).
    pub tz_offset_secs: i64,
}

impl Default for GraphOpts {
    fn default() -> Self {
        Self { cfg: GraphCfg::default(), max_events_per_pass: 2000, tz_offset_secs: 0 }
    }
}

impl GraphOpts {
    /// Build from the `GRAPH_*` environment (identical knob names + defaults to the worker's
    /// `WorkerConfig::graph_opts`). The admin `POST /v1/graph/rebuild` handler uses this so an
    /// explicit rebuild folds under the OPERATOR'S configured knobs (grace/slack/thresholds), not a
    /// hard-coded `default()`. Correctness (a tuned graph rebuilds under its own config) AND the
    /// enabler for deterministic eval: with `GRAPH_GRACE_SECS=0` a rebuild folds freshly-injected
    /// events immediately (the default 90s grace would exclude events younger than 90s wall-clock).
    pub fn from_env() -> Self {
        fn p<T: std::str::FromStr>(k: &str, d: T) -> T {
            std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
        }
        Self {
            cfg: GraphCfg {
                grace_secs: p("GRAPH_GRACE_SECS", 90),
                copresence_slack_secs: p("GRAPH_COPRESENCE_SLACK_SECS", 120),
                copresence_max_subjects: p("GRAPH_COPRESENCE_MAX_SUBJECTS", 12),
                vehicle_corr_window_secs: p("GRAPH_VEHICLE_CORR_WINDOW_SECS", 180),
                edge_sample_cap: p("GRAPH_EDGE_SAMPLE_CAP", 16),
                bind_min_sessions: p("GRAPH_BIND_MIN_SESSIONS", 3),
                bind_min_confidence: p("GRAPH_BIND_MIN_CONFIDENCE", 0.6),
                bind_margin: p("GRAPH_BIND_MARGIN", 0.2),
                baseline_window_days: p("GRAPH_BASELINE_WINDOW_DAYS", 30),
                anomaly_min_visits: p("GRAPH_ANOMALY_MIN_VISITS", 5),
                anomaly_hour_min_frac: p("GRAPH_ANOMALY_HOUR_MIN_FRAC", 0.05),
                anomaly_unknown_cluster_min: p("GRAPH_ANOMALY_UNKNOWN_CLUSTER_MIN", 3),
                journey_gap_secs: p("GRAPH_JOURNEY_GAP_SECS", 600),
            },
            max_events_per_pass: p("GRAPH_MAX_EVENTS_PER_PASS", 2000),
            tz_offset_secs: 0,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct GraphStats {
    pub events_consumed: u64,
    pub conversations_consumed: u64,
    pub edges_upserted: u64,
    pub bindings_surfaced: u64,
    /// Subjects whose `entity_baselines` row was recomputed this pass (Wave 2).
    pub baselines_recomputed: u64,
    /// `pattern_anomaly` events emitted this pass (Wave 2).
    pub anomalies_emitted: u64,
    /// Cross-camera journey rows inserted-or-updated this pass (Wave 4 / G5).
    pub journeys_upserted: u64,
    /// The emitted anomaly event_ids — the worker driver alert-evaluates these post-commit (the
    /// backend can't reach the worker's `alerts::evaluate`). Empty for a no-anomaly pass.
    pub anomaly_event_ids: Vec<Uuid>,
}

/// The 0→1 edge transitions + unknown-person clusters a single `drain_events` pass observed — the
/// raw material for the Wave-2 EDGE anomalies (`first_time_pairing`, `new_vehicle_for_person`,
/// `unknown_person_cluster`). Collected at drain time (the 0→1 signal lives here) but EMITTED in
/// [`crate::patterns::flag_edge_anomalies`] after baselines are recomputed, so maturity is available
/// for both the whole-scenario rebuild AND the incremental worker (§1.6). Batch-local, same as
/// `co_present` (a rare cross-pass split costs one anomaly, never correctness).
#[derive(Debug, Default)]
pub struct EdgeTransitions {
    /// `(a, b, device_id, t)` — a `co_present` edge that went 0→1 this pass (canonical endpoints).
    pub new_copresent: Vec<(NodeRef, NodeRef, String, i64)>,
    /// `(person, plate, device_id, t)` — an `arrived_with_vehicle` edge that went 0→1 this pass.
    pub new_vehicle: Vec<(NodeRef, NodeRef, String, i64)>,
    /// `(device_id, t, distinct_unknown_count)` — a window on one device with ≥ the cluster minimum
    /// distinct UNKNOWN persons (display_name NULL) co-present.
    pub unknown_clusters: Vec<(String, i64, i64)>,
}

/// One identity-carrying event, minimal fields for folding.
#[derive(Debug, Clone)]
struct EvRow {
    subject_type: String,
    subject_id: Uuid,
    device_id: String,
    event_id: Uuid,
    segment_id: Option<Uuid>,
    start_unix_nanos: i64,
    end_unix_nanos: i64,
    updated_micros: i64,
}

// ---------------------------------------------------------------------------------------------
// Public entry: the pass
// ---------------------------------------------------------------------------------------------

/// One bounded graph pass (the worker-0 driver's per-interval call). Advisory-locked, single
/// transaction, both watermarks advanced on commit.
pub async fn graph_pass(pool: &PgPool, opts: &GraphOpts) -> anyhow::Result<GraphStats> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(GRAPH_LOCK_KEY)
        .execute(&mut *tx)
        .await?;

    let cfg_hash = graph::config_hash(&opts.cfg);
    let state = load_state(&mut tx).await?;
    if let Some(stored) = &state.config_hash
        && stored != &cfg_hash
    {
        // Config drift: keep accumulating (rebuild is explicit only — the
        // THREADER_BACKFILL_ON_START idiom). A stale-lineage warning + metric only.
        tracing::warn!(stored = %stored, current = %cfg_hash, "graph config_hash drift — accumulating; run rebuild to refold");
        crate::observe::counter("hushai_graph_config_drift_total", &[]);
    }

    let mut stats = GraphStats::default();
    let (ev_wm, touched_ev, transitions) =
        drain_events(&mut tx, opts, state.events_watermark_micros, &cfg_hash, &mut stats).await?;
    let (cv_wm, touched_cv) =
        drain_conversations(&mut tx, opts, state.conversations_watermark_micros, &cfg_hash, &mut stats)
            .await?;

    // Step 4 (§1.5 / §1.6): recompute baselines for the subjects touched this pass and emit the
    // pattern anomalies. off_schedule is judged AS-OF (each visit vs the subject's strictly-earlier
    // visits) in recompute_and_flag; the three EDGE anomalies (first_time_pairing / new_vehicle /
    // unknown_cluster) are judged in flag_edge_anomalies over the 0→1 transitions this pass observed,
    // AFTER baselines are recomputed so endpoint maturity is available. Both run inside this pass tx.
    let mut touched = touched_ev;
    touched.extend(touched_cv);
    if !touched.is_empty() {
        let ids = crate::patterns::recompute_and_flag(
            &mut tx,
            &opts.cfg,
            opts.tz_offset_secs,
            &cfg_hash,
            &touched,
        )
        .await?;
        stats.baselines_recomputed = touched.len() as u64;
        stats.anomaly_event_ids = ids;
        // Step 4b (§1.3 / G5): re-stitch cross-camera journeys for the touched person/plate subjects.
        // Independent of baselines; runs in the same pass tx over the same capture-anchored window.
        stats.journeys_upserted =
            crate::patterns::stitch_and_upsert_journeys(&mut tx, &opts.cfg, &cfg_hash, &touched)
                .await?;
    }
    let edge_ids = crate::patterns::flag_edge_anomalies(
        &mut tx,
        &opts.cfg,
        opts.tz_offset_secs,
        &transitions,
    )
    .await?;
    stats.anomaly_event_ids.extend(edge_ids);
    stats.anomalies_emitted = stats.anomaly_event_ids.len() as u64;

    // Advance watermarks (max seen this pass; unchanged when nothing drained) + stamp config_hash.
    sqlx::query(
        "UPDATE graph_state SET \
           events_watermark = GREATEST(events_watermark, to_timestamp($1::double precision / 1e6)), \
           conversations_watermark = GREATEST(conversations_watermark, to_timestamp($2::double precision / 1e6)), \
           config_hash = $3, updated_at = now() WHERE id = 1",
    )
    .bind(ev_wm)
    .bind(cv_wm)
    .bind(&cfg_hash)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(stats)
}

struct GraphStateRow {
    events_watermark_micros: i64,
    conversations_watermark_micros: i64,
    config_hash: Option<String>,
}

async fn load_state(tx: &mut Transaction<'_, Postgres>) -> anyhow::Result<GraphStateRow> {
    let row = sqlx::query(
        "SELECT (extract(epoch from events_watermark) * 1e6)::bigint AS ew, \
                (extract(epoch from conversations_watermark) * 1e6)::bigint AS cw, \
                config_hash FROM graph_state WHERE id = 1",
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(GraphStateRow {
        events_watermark_micros: row.get("ew"),
        conversations_watermark_micros: row.get("cw"),
        config_hash: row.try_get("config_hash").ok().flatten(),
    })
}

// ---------------------------------------------------------------------------------------------
// Events drain: visits_place, co_present, arrived_with_vehicle
// ---------------------------------------------------------------------------------------------

/// Returns the new events watermark (max updated_micros drained, or the prior one if nothing).
async fn drain_events(
    tx: &mut Transaction<'_, Postgres>,
    opts: &GraphOpts,
    prior_wm_micros: i64,
    cfg_hash: &str,
    stats: &mut GraphStats,
) -> anyhow::Result<(i64, BTreeSet<(String, Uuid)>, EdgeTransitions)> {
    let mut transitions = EdgeTransitions::default();
    let slack_nanos = opts.cfg.copresence_slack_secs.max(1) * NANOS_PER_SEC;
    // DB-clock "now" for the in-progress guard (fixtures with pinned capture still settle).
    let now_ns: i64 = sqlx::query_scalar("SELECT (extract(epoch from now()) * 1e9)::bigint")
        .fetch_one(&mut **tx)
        .await?;

    let rows = sqlx::query(
        "SELECT e.event_id, e.subject_type, e.subject_id, e.device_id, e.segment_id, \
                e.start_unix_nanos, e.end_unix_nanos, \
                (extract(epoch from e.updated_at) * 1e6)::bigint AS updated_micros \
         FROM events e \
         WHERE e.subject_id IS NOT NULL \
           AND e.subject_type = ANY(ARRAY['person','speaker','plate']) \
           AND e.device_id IS NOT NULL \
           AND e.event_type NOT IN ('pattern_anomaly', 'gotham_briefing') \
           AND e.updated_at > to_timestamp($1::double precision / 1e6) \
           AND e.updated_at < now() - make_interval(secs => $2) \
           AND e.end_unix_nanos <= $3 \
         ORDER BY e.updated_at ASC LIMIT $4",
    )
    .bind(prior_wm_micros)
    .bind(opts.cfg.grace_secs.max(0) as f64)
    .bind(now_ns.saturating_sub(slack_nanos))
    .bind(opts.max_events_per_pass.max(1))
    .fetch_all(&mut **tx)
    .await?;

    if rows.is_empty() {
        return Ok((prior_wm_micros, BTreeSet::new(), transitions));
    }
    let events: Vec<EvRow> = rows
        .into_iter()
        .map(|r| {
            Ok::<_, sqlx::Error>(EvRow {
                subject_type: r.try_get("subject_type")?,
                subject_id: r.try_get("subject_id")?,
                device_id: r.try_get("device_id")?,
                event_id: r.try_get("event_id")?,
                segment_id: r.try_get("segment_id")?,
                start_unix_nanos: r.try_get("start_unix_nanos")?,
                end_unix_nanos: r.try_get("end_unix_nanos")?,
                updated_micros: r.try_get("updated_micros")?,
            })
        })
        .collect::<Result<_, _>>()?;
    stats.events_consumed = events.len() as u64;
    let new_wm = events.iter().map(|e| e.updated_micros).max().unwrap_or(prior_wm_micros);

    // Coalesce each subject's events into visits (BTreeMap → deterministic subject order).
    let mut by_subject: BTreeMap<(String, Uuid), Vec<&EvRow>> = BTreeMap::new();
    for e in &events {
        by_subject.entry((e.subject_type.clone(), e.subject_id)).or_default().push(e);
    }

    // Build coalesced visits + a device→visits index for co_present / vehicle correlation.
    let mut all_visits: Vec<(GraphVisit, EvidenceSample)> = Vec::new();
    for ((stype, sid), evs) in &by_subject {
        let node_type = match stype.as_str() {
            "person" => NodeType::Person,
            "speaker" => NodeType::Speaker,
            "plate" => NodeType::Plate,
            _ => continue,
        };
        let node = NodeRef::new(node_type, sid.to_string());
        let visits = coalesce(evs, slack_nanos);
        for v in &visits {
            // visits_place: subject → device.
            let dev = NodeRef::new(NodeType::Device, v.device_id.clone());
            let sample = v.evidence.clone();
            upsert_edge(
                tx,
                EdgeKind::VisitsPlace,
                node.clone(),
                dev,
                1,
                v.start_unix_nanos,
                v.end_unix_nanos,
                None,
                std::slice::from_ref(&sample),
                None,
                None,
                cfg_hash,
                opts.cfg.edge_sample_cap,
            )
            .await?;
            stats.edges_upserted += 1;
            all_visits.push((
                GraphVisit {
                    node: node.clone(),
                    device_id: v.device_id.clone(),
                    start_unix_nanos: v.start_unix_nanos,
                    end_unix_nanos: v.end_unix_nanos,
                },
                sample,
            ));
        }
    }

    // co_present: person↔speaker overlapping visits, per device (batch-local — the
    // profiles::co_present precedent; a rare batch split costs one observation, never
    // correctness, and converges as more events drain).
    let mut by_device: BTreeMap<String, Vec<GraphVisit>> = BTreeMap::new();
    for (v, _) in &all_visits {
        if matches!(v.node.node_type, NodeType::Person | NodeType::Speaker) {
            by_device.entry(v.device_id.clone()).or_default().push(v.clone());
        }
    }
    for (device, visits) in &by_device {
        let pairs = graph::co_present_pairs(visits, slack_nanos, opts.cfg.copresence_max_subjects);
        if visits.iter().map(|v| &v.node).collect::<std::collections::BTreeSet<_>>().len()
            > opts.cfg.copresence_max_subjects
        {
            crate::observe::counter("hushai_graph_copresence_capped_total", &[]);
        }
        for (a, b) in pairs {
            // The actual co-presence moments — max of each overlapping visit PAIR's starts — NOT the
            // batch-global max visit start. In a whole-history rebuild a and b may each keep visiting
            // solo AFTER first meeting; the global max would misdate the edge's first_seen and the
            // first_time_pairing anomaly's civil-day. `first_t` = the FIRST time they were together.
            let mut first_co: Option<i64> = None;
            let mut last_co: Option<i64> = None;
            for va in visits.iter().filter(|v| v.node == a) {
                for vb in visits.iter().filter(|v| v.node == b) {
                    if graph::visits_overlap(va, vb, slack_nanos) {
                        let moment = va.start_unix_nanos.max(vb.start_unix_nanos);
                        first_co = Some(first_co.map_or(moment, |m| m.min(moment)));
                        last_co = Some(last_co.map_or(moment, |m| m.max(moment)));
                    }
                }
            }
            // The pair came from co_present_pairs (an overlap exists), so these are Some; defensive fallback.
            let first_t = first_co.unwrap_or(0);
            let last_t = last_co.unwrap_or(first_t);
            let prior = upsert_edge(
                tx,
                EdgeKind::CoPresent,
                a.clone(),
                b.clone(),
                1,
                first_t,
                last_t,
                None,
                &[EvidenceSample { event_id: None, segment_id: None, t: first_t }],
                None,
                None,
                cfg_hash,
                opts.cfg.edge_sample_cap,
            )
            .await?;
            stats.edges_upserted += 1;
            // 0→1 this pass ⇒ a brand-new pairing → first_time_pairing candidate (maturity decided
            // later in patterns::flag_edge_anomalies). A repeat (prior≥1, or a second device this
            // pass) has prior≥1 and is not re-recorded. The anomaly is dated to first_t (first meeting).
            if prior == 0 {
                transitions.new_copresent.push((a, b, device.clone(), first_t));
            }
        }
    }

    // unknown_person_cluster (§1.6): a window on one device with ≥ GRAPH_ANOMALY_UNKNOWN_CLUSTER_MIN
    // distinct UNKNOWN persons (display_name IS NULL) co-present. Batch-local like co_present. The
    // "unknown" set is resolved once for this pass's person subjects (unknown = no display_name, the
    // events_producer known/unknown rule).
    let person_ids: Vec<String> = by_subject
        .keys()
        .filter(|(st, _)| st == "person")
        .map(|(_, id)| id.to_string())
        .collect();
    if !person_ids.is_empty() && opts.cfg.anomaly_unknown_cluster_min >= 1 {
        let unknown_ids: BTreeSet<String> = sqlx::query_scalar::<_, Uuid>(
            "SELECT person_id FROM persons WHERE person_id = ANY($1::uuid[]) AND display_name IS NULL",
        )
        .bind(person_ids.iter().filter_map(|s| Uuid::parse_str(s).ok()).collect::<Vec<_>>())
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .map(|u| u.to_string())
        .collect();
        for (device, visits) in &by_device {
            // Only unknown-person visits, start-sorted (deterministic scan order).
            let mut uv: Vec<&GraphVisit> = visits
                .iter()
                .filter(|v| v.node.node_type == NodeType::Person && unknown_ids.contains(&v.node.id))
                .collect();
            uv.sort_by(|a, b| a.start_unix_nanos.cmp(&b.start_unix_nanos).then_with(|| a.node.id.cmp(&b.node.id)));
            let mut fired_days: BTreeSet<i64> = BTreeSet::new();
            for anchor in &uv {
                // Distinct unknown persons overlapping the anchor visit (includes the anchor).
                let distinct: BTreeSet<&str> = uv
                    .iter()
                    .filter(|o| graph::visits_overlap(anchor, o, slack_nanos))
                    .map(|o| o.node.id.as_str())
                    .collect();
                if graph::is_unknown_cluster(distinct.len(), &opts.cfg) {
                    // One candidate per (device, civil day) — the emit dedups too, but recording once
                    // keeps the reported count stable (the earliest-anchor window wins).
                    let day = graph::civil_day(anchor.start_unix_nanos, opts.tz_offset_secs);
                    if fired_days.insert(day) {
                        transitions.unknown_clusters.push((
                            device.clone(),
                            anchor.start_unix_nanos,
                            distinct.len() as i64,
                        ));
                    }
                }
            }
        }
    }

    // arrived_with_vehicle: a plate visit on the same device within vehicle_corr_window of a
    // person visit start → person → plate.
    let corr_nanos = opts.cfg.vehicle_corr_window_secs.max(1) * NANOS_PER_SEC;
    let persons: Vec<&(GraphVisit, EvidenceSample)> =
        all_visits.iter().filter(|(v, _)| v.node.node_type == NodeType::Person).collect();
    let plates: Vec<&(GraphVisit, EvidenceSample)> =
        all_visits.iter().filter(|(v, _)| v.node.node_type == NodeType::Plate).collect();
    for (pv, _) in &persons {
        for (plv, plsample) in &plates {
            if pv.device_id != plv.device_id {
                continue;
            }
            if (plv.start_unix_nanos - pv.start_unix_nanos).abs() <= corr_nanos {
                let t = pv.start_unix_nanos.min(plv.start_unix_nanos);
                let prior = upsert_edge(
                    tx,
                    EdgeKind::ArrivedWithVehicle,
                    pv.node.clone(),
                    plv.node.clone(),
                    1,
                    t,
                    pv.end_unix_nanos.max(plv.end_unix_nanos),
                    None,
                    &[(*plsample).clone()],
                    None,
                    None,
                    cfg_hash,
                    opts.cfg.edge_sample_cap,
                )
                .await?;
                stats.edges_upserted += 1;
                // 0→1 ⇒ a new person↔vehicle association → new_vehicle_for_person candidate (the
                // "established OTHER vehicle" test is applied in patterns::flag_edge_anomalies).
                if prior == 0 {
                    transitions.new_vehicle.push((
                        pv.node.clone(),
                        plv.node.clone(),
                        pv.device_id.clone(),
                        t,
                    ));
                }
            }
        }
    }

    // Subjects touched this pass (drives the Wave-2 baseline recompute + anomaly judging).
    let touched: BTreeSet<(String, Uuid)> = by_subject.keys().cloned().collect();
    Ok((new_wm, touched, transitions))
}

/// Coalesce one subject's events into visits (interval-aware, same rule as
/// `profiles::coalesce_events_to_visits`), carrying a newest evidence sample per visit.
fn coalesce(evs: &[&EvRow], gap_nanos: i64) -> Vec<CoalescedVisit> {
    let mut sorted: Vec<&EvRow> = evs.to_vec();
    sorted.sort_by_key(|e| e.start_unix_nanos);
    let mut out: Vec<CoalescedVisit> = Vec::new();
    for e in sorted {
        let sample = EvidenceSample {
            event_id: Some(e.event_id.to_string()),
            segment_id: e.segment_id.map(|s| s.to_string()),
            t: e.start_unix_nanos,
        };
        match out.last_mut() {
            Some(v) if v.device_id == e.device_id && e.start_unix_nanos - v.end_unix_nanos <= gap_nanos => {
                v.end_unix_nanos = v.end_unix_nanos.max(e.end_unix_nanos);
                v.evidence = sample; // newest
            }
            _ => out.push(CoalescedVisit {
                device_id: e.device_id.clone(),
                start_unix_nanos: e.start_unix_nanos,
                end_unix_nanos: e.end_unix_nanos,
                evidence: sample,
            }),
        }
    }
    out
}

struct CoalescedVisit {
    device_id: String,
    start_unix_nanos: i64,
    end_unix_nanos: i64,
    evidence: EvidenceSample,
}

// ---------------------------------------------------------------------------------------------
// Conversations drain: conversed_with + voice↔face binding trials
// ---------------------------------------------------------------------------------------------

async fn drain_conversations(
    tx: &mut Transaction<'_, Postgres>,
    opts: &GraphOpts,
    prior_wm_micros: i64,
    cfg_hash: &str,
    stats: &mut GraphStats,
) -> anyhow::Result<(i64, BTreeSet<(String, Uuid)>)> {
    let slack_nanos = opts.cfg.copresence_slack_secs.max(1) * NANOS_PER_SEC;
    let rows = sqlx::query(
        "SELECT conversation_id, primary_device_id, started_at_unix_nanos, ended_at_unix_nanos, \
                speaker_ids, (extract(epoch from updated_at) * 1e6)::bigint AS updated_micros \
         FROM conversations \
         WHERE status = 'closed' \
           AND updated_at > to_timestamp($1::double precision / 1e6) \
           AND updated_at < now() - make_interval(secs => $2) \
         ORDER BY updated_at ASC LIMIT $3",
    )
    .bind(prior_wm_micros)
    .bind(opts.cfg.grace_secs.max(0) as f64)
    .bind(opts.max_events_per_pass.max(1))
    .fetch_all(&mut **tx)
    .await?;
    if rows.is_empty() {
        return Ok((prior_wm_micros, BTreeSet::new()));
    }

    let mut new_wm = prior_wm_micros;
    let mut consumed = 0u64;
    let mut touched: BTreeSet<(String, Uuid)> = BTreeSet::new();
    for r in &rows {
        consumed += 1;
        new_wm = new_wm.max(r.get::<i64, _>("updated_micros"));
        let device_id: Option<String> = r.try_get("primary_device_id")?;
        let t0: i64 = r.get("started_at_unix_nanos");
        let t1: i64 = r.get("ended_at_unix_nanos");
        let mut speakers: Vec<Uuid> = r.try_get("speaker_ids")?;
        speakers.sort();
        speakers.dedup();
        for sp in &speakers {
            touched.insert(("speaker".to_string(), *sp));
        }

        // conversed_with: every distinct speaker pair in the conversation.
        for i in 0..speakers.len() {
            for j in (i + 1)..speakers.len() {
                let a = NodeRef::new(NodeType::Speaker, speakers[i].to_string());
                let b = NodeRef::new(NodeType::Speaker, speakers[j].to_string());
                let (src, dst) = graph::canonical_pair(a, b);
                upsert_edge(
                    tx,
                    EdgeKind::ConversedWith,
                    src,
                    dst,
                    1,
                    t0,
                    t1,
                    None,
                    &[EvidenceSample { event_id: None, segment_id: None, t: t0 }],
                    None,
                    None,
                    cfg_hash,
                    opts.cfg.edge_sample_cap,
                )
                .await?;
                stats.edges_upserted += 1;
            }
        }

        // Binding trials (§1.4): persons present on this device overlapping [t0,t1] ± slack.
        let Some(dev) = device_id else { continue };
        let persons_present: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT subject_id FROM events \
             WHERE subject_type = 'person' AND subject_id IS NOT NULL AND device_id = $1 \
               AND event_type NOT IN ('pattern_anomaly', 'gotham_briefing') \
               AND start_unix_nanos < $2 AND end_unix_nanos > $3",
        )
        .bind(&dev)
        .bind(t1 + slack_nanos)
        .bind(t0 - slack_nanos)
        .fetch_all(&mut **tx)
        .await?;

        for sp in &speakers {
            if persons_present.is_empty() {
                // speaker_only: this speaker talked with no face visible — lowers Jaccard on all
                // that speaker's existing binding candidates.
                bump_binding_speaker_only(tx, *sp, opts, cfg_hash).await?;
                continue;
            }
            for person in &persons_present {
                let surfaced = upsert_binding(tx, *sp, *person, t0, opts, cfg_hash).await?;
                stats.edges_upserted += 1;
                if surfaced {
                    stats.bindings_surfaced += 1;
                }
            }
        }
    }
    stats.conversations_consumed = consumed;
    Ok((new_wm, touched))
}

// ---------------------------------------------------------------------------------------------
// Edge upsert (read-modify-write so evidence keeps its newest-N window deterministically)
// ---------------------------------------------------------------------------------------------

/// Upsert an edge (read-modify-write on evidence for a deterministic newest-N window). Returns the
/// edge's PRIOR `observation_count` (0 when the row is new) — the 0→1 transition signal the Wave-2
/// edge anomalies (`first_time_pairing`, `new_vehicle_for_person`) key on (§1.6). Callers that don't
/// care simply drop the value.
#[allow(clippy::too_many_arguments)]
async fn upsert_edge(
    tx: &mut Transaction<'_, Postgres>,
    kind: EdgeKind,
    src: NodeRef,
    dst: NodeRef,
    add_obs: i64,
    first_ns: i64,
    last_ns: i64,
    confidence: Option<f32>,
    new_samples: &[EvidenceSample],
    status: Option<&str>,
    metadata: Option<Value>,
    cfg_hash: &str,
    sample_cap: usize,
) -> anyhow::Result<i64> {
    // Read the existing evidence (FOR UPDATE) so the newest-N merge is race-free.
    let existing: Option<(Value, i64)> = sqlx::query_as(
        "SELECT evidence, observation_count FROM entity_edges \
         WHERE edge_type = $1 AND src_type = $2 AND src_id = $3 AND dst_type = $4 AND dst_id = $5 \
         FOR UPDATE",
    )
    .bind(kind.as_str())
    .bind(src.node_type.as_str())
    .bind(&src.id)
    .bind(dst.node_type.as_str())
    .bind(&dst.id)
    .fetch_optional(&mut **tx)
    .await?;

    let prior_count: i64 = existing.as_ref().map(|(_, c)| *c).unwrap_or(0);
    let prior_samples: Vec<EvidenceSample> = existing
        .as_ref()
        .and_then(|(v, _)| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let merged = graph::merge_evidence_samples(&prior_samples, new_samples, sample_cap);
    let evidence_json = serde_json::to_value(&merged).unwrap_or_else(|_| json!([]));
    let metadata_json = metadata.unwrap_or_else(|| json!({}));

    sqlx::query(
        "INSERT INTO entity_edges \
           (edge_id, edge_type, src_type, src_id, dst_type, dst_id, observation_count, \
            first_seen_unix_nanos, last_seen_unix_nanos, confidence, evidence, metadata, status, \
            config_hash, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14, now(), now()) \
         ON CONFLICT (edge_type, src_type, src_id, dst_type, dst_id) DO UPDATE SET \
           observation_count = entity_edges.observation_count + $7, \
           first_seen_unix_nanos = LEAST(COALESCE(entity_edges.first_seen_unix_nanos, $8), $8), \
           last_seen_unix_nanos = GREATEST(COALESCE(entity_edges.last_seen_unix_nanos, $9), $9), \
           confidence = COALESCE($10, entity_edges.confidence), \
           evidence = $11, \
           metadata = entity_edges.metadata || $12, \
           status = $13, \
           config_hash = $14, updated_at = now()",
    )
    .bind(Uuid::now_v7())
    .bind(kind.as_str())
    .bind(src.node_type.as_str())
    .bind(&src.id)
    .bind(dst.node_type.as_str())
    .bind(&dst.id)
    .bind(add_obs)
    .bind(first_ns)
    .bind(last_ns)
    .bind(confidence)
    .bind(&evidence_json)
    .bind(&metadata_json)
    .bind(status)
    .bind(cfg_hash)
    .execute(&mut **tx)
    .await?;
    Ok(prior_count)
}

/// Upsert a binding candidate (`same_identity_candidate`), bumping the `together` counter and
/// re-deciding whether it surfaces into the review queue. Returns true when it newly surfaces.
/// Endpoints canonicalized (speaker/person). Never auto-confirms; rejection is sticky.
async fn upsert_binding(
    tx: &mut Transaction<'_, Postgres>,
    speaker: Uuid,
    person: Uuid,
    t: i64,
    opts: &GraphOpts,
    cfg_hash: &str,
) -> anyhow::Result<bool> {
    let (src, dst) = graph::canonical_pair(
        NodeRef::new(NodeType::Speaker, speaker.to_string()),
        NodeRef::new(NodeType::Person, person.to_string()),
    );
    let row: Option<(Value, Option<String>)> = sqlx::query_as(
        "SELECT metadata, status FROM entity_edges \
         WHERE edge_type = 'same_identity_candidate' AND src_type = $1 AND src_id = $2 \
           AND dst_type = $3 AND dst_id = $4 FOR UPDATE",
    )
    .bind(src.node_type.as_str())
    .bind(&src.id)
    .bind(dst.node_type.as_str())
    .bind(&dst.id)
    .fetch_optional(&mut **tx)
    .await?;

    let mut counters: BindCounters = row
        .as_ref()
        .and_then(|(m, _)| serde_json::from_value::<BindCounters>(m.get("counters").cloned().unwrap_or(json!({}))).ok())
        .unwrap_or_default();
    let sticky_rejected = matches!(row.as_ref().and_then(|(_, s)| s.clone()).as_deref(), Some("rejected"));
    let already_confirmed = matches!(row.as_ref().and_then(|(_, s)| s.clone()).as_deref(), Some("confirmed"));
    counters.together += 1;
    let conf = graph::binding_confidence(&counters);

    // Runner-up: the speaker's best OTHER person candidate confidence (margin gate defeats the
    // always-together confound). Read after the counter bump so this trial is comparable.
    let runner_up = speaker_runner_up_confidence(tx, speaker, person).await?;
    let should_surface = graph::binding_should_surface(&counters, runner_up, &opts.cfg);

    // Status transitions: sticky 'rejected' and existing 'confirmed' never auto-flip; otherwise a
    // qualifying trial promotes NULL/candidate → 'candidate'.
    let new_status: Option<&str> = if sticky_rejected {
        Some("rejected")
    } else if already_confirmed {
        Some("confirmed")
    } else if should_surface {
        Some("candidate")
    } else {
        row.as_ref().and_then(|(_, s)| s.as_deref())
    };
    let newly_surfaced = !sticky_rejected && !already_confirmed && should_surface
        && !matches!(row.as_ref().and_then(|(_, s)| s.as_deref()), Some("candidate"));

    let metadata = json!({ "counters": counters });
    upsert_edge(
        tx,
        EdgeKind::SameIdentityCandidate,
        src,
        dst,
        1,
        t,
        t,
        Some(conf),
        &[EvidenceSample { event_id: None, segment_id: None, t }],
        new_status,
        Some(metadata),
        cfg_hash,
        opts.cfg.edge_sample_cap,
    )
    .await?;
    // upsert_edge merges metadata with `||`, so counters overwrite cleanly (same key).
    Ok(newly_surfaced)
}

/// Bump `speaker_only` on every existing binding candidate of `speaker` (talked, no face seen).
async fn bump_binding_speaker_only(
    tx: &mut Transaction<'_, Postgres>,
    speaker: Uuid,
    opts: &GraphOpts,
    cfg_hash: &str,
) -> anyhow::Result<()> {
    let sp = speaker.to_string();
    let ids: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT src_type, src_id, dst_type, dst_id FROM entity_edges \
         WHERE edge_type = 'same_identity_candidate' \
           AND ((src_type = 'speaker' AND src_id = $1) OR (dst_type = 'speaker' AND dst_id = $1)) \
           AND COALESCE(status,'') <> 'rejected'",
    )
    .bind(&sp)
    .fetch_all(&mut **tx)
    .await?;
    for (st, si, dt, di) in ids {
        let row: Option<(Value,)> = sqlx::query_as(
            "SELECT metadata FROM entity_edges WHERE edge_type='same_identity_candidate' \
             AND src_type=$1 AND src_id=$2 AND dst_type=$3 AND dst_id=$4 FOR UPDATE",
        )
        .bind(&st).bind(&si).bind(&dt).bind(&di)
        .fetch_optional(&mut **tx)
        .await?;
        let mut counters: BindCounters = row
            .and_then(|(m,)| serde_json::from_value(m.get("counters").cloned().unwrap_or(json!({}))).ok())
            .unwrap_or_default();
        counters.speaker_only += 1;
        let conf = graph::binding_confidence(&counters);
        sqlx::query(
            "UPDATE entity_edges SET metadata = metadata || $5, confidence = $6, config_hash = $7, \
                updated_at = now() \
             WHERE edge_type='same_identity_candidate' AND src_type=$1 AND src_id=$2 AND dst_type=$3 AND dst_id=$4",
        )
        .bind(&st).bind(&si).bind(&dt).bind(&di)
        .bind(json!({"counters": counters}))
        .bind(conf)
        .bind(cfg_hash)
        .execute(&mut **tx)
        .await?;
    }
    let _ = opts;
    Ok(())
}

/// The speaker's best confidence to a DIFFERENT person candidate (for the binding margin gate).
async fn speaker_runner_up_confidence(
    tx: &mut Transaction<'_, Postgres>,
    speaker: Uuid,
    exclude_person: Uuid,
) -> anyhow::Result<f32> {
    let sp = speaker.to_string();
    let ex = exclude_person.to_string();
    let best: Option<f32> = sqlx::query_scalar(
        "SELECT max(confidence) FROM entity_edges \
         WHERE edge_type = 'same_identity_candidate' \
           AND ((src_type='speaker' AND src_id=$1) OR (dst_type='speaker' AND dst_id=$1)) \
           AND NOT ((src_type='person' AND src_id=$2) OR (dst_type='person' AND dst_id=$2))",
    )
    .bind(&sp)
    .bind(&ex)
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    Ok(best.unwrap_or(0.0))
}

// ---------------------------------------------------------------------------------------------
// Merge / delete reconciliation (called inside the catalog merge/delete transactions)
// ---------------------------------------------------------------------------------------------

/// Repoint the loser identity's edges onto the survivor inside a merge transaction (call beside
/// `profiles::merge_in_tx`). Folds resulting duplicates (sum counts, LEAST/GREATEST seen, merged
/// evidence, summed binding counters, recomputed confidence), drops self-edges. `node_type` is
/// 'person'|'speaker'|'plate' (device nodes never merge). Documented accepted loss (== profiles):
/// loser events not yet drained at merge time stay orphaned.
pub async fn merge_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    node_type: &str,
    loser: Uuid,
    survivor: Uuid,
) -> Result<(), sqlx::Error> {
    // A blind `UPDATE src_id = survivor` would violate the unique index the instant it creates a
    // duplicate (Postgres enforces it per-row, not deferred) AND can leave a non-canonical mirror
    // row for a same-type undirected edge. So: load the loser's edges, delete them, then re-fold
    // each onto the survivor through the canonicalizing upsert — the only collision-safe order.
    let l = loser.to_string();
    let s = survivor.to_string();
    let rows = sqlx::query(
        "SELECT edge_type, src_type, src_id, dst_type, dst_id, observation_count, \
                first_seen_unix_nanos, last_seen_unix_nanos, evidence, metadata, status \
         FROM entity_edges WHERE (src_type = $1 AND src_id = $2) OR (dst_type = $1 AND dst_id = $2)",
    )
    .bind(node_type).bind(&l)
    .fetch_all(&mut **tx)
    .await?;
    sqlx::query(
        "DELETE FROM entity_edges WHERE (src_type = $1 AND src_id = $2) OR (dst_type = $1 AND dst_id = $2)",
    )
    .bind(node_type).bind(&l)
    .execute(&mut **tx)
    .await?;

    for r in &rows {
        let edge_type: String = r.get("edge_type");
        let mut st: String = r.get("src_type");
        let mut si: String = r.get("src_id");
        let mut dt: String = r.get("dst_type");
        let mut di: String = r.get("dst_id");
        // Repoint the loser endpoint(s) onto the survivor.
        if st == node_type && si == l {
            si = s.clone();
        }
        if dt == node_type && di == l {
            di = s.clone();
        }
        if st == dt && si == di {
            continue; // self-edge (was loser↔survivor) — drop
        }
        // Re-canonicalize undirected edges (a repoint can flip the ordering for same-type pairs).
        if matches!(edge_type.as_str(), "co_present" | "conversed_with" | "same_identity_candidate")
            && (st.as_str(), si.as_str()) > (dt.as_str(), di.as_str())
        {
            std::mem::swap(&mut st, &mut dt);
            std::mem::swap(&mut si, &mut di);
        }

        let obs: i64 = r.get("observation_count");
        let first: Option<i64> = r.try_get("first_seen_unix_nanos").ok().flatten();
        let last: Option<i64> = r.try_get("last_seen_unix_nanos").ok().flatten();
        let ev: Value = r.try_get("evidence").unwrap_or_else(|_| json!([]));
        let meta: Value = r.try_get("metadata").unwrap_or_else(|_| json!({}));
        let status: Option<String> = r.try_get("status").ok().flatten();

        // Read the current target (survivor-side row, possibly created by a prior loop iteration).
        let cur = sqlx::query(
            "SELECT observation_count, first_seen_unix_nanos, last_seen_unix_nanos, evidence, \
                    metadata, status FROM entity_edges \
             WHERE edge_type = $1 AND src_type = $2 AND src_id = $3 AND dst_type = $4 AND dst_id = $5 \
             FOR UPDATE",
        )
        .bind(&edge_type).bind(&st).bind(&si).bind(&dt).bind(&di)
        .fetch_optional(&mut **tx)
        .await?;
        let (cobs, cfirst, clast, cev, cmeta, cstatus): (i64, Option<i64>, Option<i64>, Value, Value, Option<String>) =
            match &cur {
                Some(c) => (
                    c.get("observation_count"),
                    c.try_get("first_seen_unix_nanos").ok().flatten(),
                    c.try_get("last_seen_unix_nanos").ok().flatten(),
                    c.try_get("evidence").unwrap_or_else(|_| json!([])),
                    c.try_get("metadata").unwrap_or_else(|_| json!({})),
                    c.try_get("status").ok().flatten(),
                ),
                None => (0, None, None, json!([]), json!({}), None),
            };

        let new_obs = cobs + obs;
        let new_first = min_opt(cfirst, first);
        let new_last = max_opt(clast, last);
        let prior: Vec<EvidenceSample> = serde_json::from_value(cev).unwrap_or_default();
        let incoming: Vec<EvidenceSample> = serde_json::from_value(ev).unwrap_or_default();
        let merged_ev = graph::merge_evidence_samples(&prior, &incoming, MERGE_SAMPLE_CAP);
        let ev_json = serde_json::to_value(&merged_ev).unwrap_or_else(|_| json!([]));

        // Binding edges sum their counters, recompute confidence, and take the strongest status.
        let (new_meta, new_conf, new_status): (Value, Option<f32>, Option<String>) =
            if edge_type == "same_identity_candidate" {
                let lc = counters_of(&meta);
                let cc = counters_of(&cmeta);
                let sum = BindCounters {
                    together: lc.together + cc.together,
                    speaker_only: lc.speaker_only + cc.speaker_only,
                    person_only: lc.person_only + cc.person_only,
                };
                let owner_seed = meta.get("owner_seed").and_then(|v| v.as_bool()).unwrap_or(false)
                    || cmeta.get("owner_seed").and_then(|v| v.as_bool()).unwrap_or(false);
                let m = json!({ "counters": sum, "owner_seed": owner_seed });
                (m, Some(graph::binding_confidence(&sum)), merge_status(cstatus, status))
            } else {
                (cmeta, None, None)
            };

        sqlx::query(
            "INSERT INTO entity_edges \
               (edge_id, edge_type, src_type, src_id, dst_type, dst_id, observation_count, \
                first_seen_unix_nanos, last_seen_unix_nanos, confidence, evidence, metadata, status, \
                created_at, updated_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13, now(), now()) \
             ON CONFLICT (edge_type, src_type, src_id, dst_type, dst_id) DO UPDATE SET \
               observation_count = $7, first_seen_unix_nanos = $8, last_seen_unix_nanos = $9, \
               confidence = COALESCE($10, entity_edges.confidence), evidence = $11, \
               metadata = $12, status = $13, updated_at = now()",
        )
        .bind(Uuid::now_v7())
        .bind(&edge_type).bind(&st).bind(&si).bind(&dt).bind(&di)
        .bind(new_obs).bind(new_first).bind(new_last).bind(new_conf)
        .bind(&ev_json).bind(&new_meta).bind(&new_status)
        .execute(&mut **tx)
        .await?;
    }
    // Journeys (0030) are per-subject DERIVED rows; drop the loser's (the survivor re-stitches when
    // next drained, folding in the repointed events). Vision lanes only — speaker/device carry none.
    if node_type == "person" || node_type == "plate" {
        sqlx::query("DELETE FROM entity_journeys WHERE subject_type = $1 AND subject_id = $2")
            .bind(node_type)
            .bind(loser)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Evidence sample cap used by merge folding (no `GraphCfg` in scope at a catalog-merge callsite;
/// matches the default `GRAPH_EDGE_SAMPLE_CAP`).
const MERGE_SAMPLE_CAP: usize = 16;

fn min_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (x, None) => x,
        (None, y) => y,
    }
}
fn max_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (x, None) => x,
        (None, y) => y,
    }
}
fn counters_of(meta: &Value) -> BindCounters {
    serde_json::from_value(meta.get("counters").cloned().unwrap_or_else(|| json!({}))).unwrap_or_default()
}
/// Merge two binding statuses taking the strongest: a human `confirmed` wins; then sticky
/// `rejected`; then `candidate`; else NULL.
fn merge_status(a: Option<String>, b: Option<String>) -> Option<String> {
    let rank = |s: &Option<String>| match s.as_deref() {
        Some("confirmed") => 3,
        Some("rejected") => 2,
        Some("candidate") => 1,
        _ => 0,
    };
    if rank(&a) >= rank(&b) { a } else { b }
}

/// Cascade-delete an entity's graph rows inside its catalog-delete transaction (§3 deletion
/// inheritance). Call from any HARD delete path of persons/speakers/plates.
pub async fn delete_entity_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    node_type: &str,
    id: Uuid,
) -> Result<(), sqlx::Error> {
    let sid = id.to_string();
    sqlx::query(
        "DELETE FROM entity_edges WHERE (src_type = $1 AND src_id = $2) OR (dst_type = $1 AND dst_id = $2)",
    )
    .bind(node_type).bind(&sid)
    .execute(&mut **tx)
    .await?;
    // Baselines/journeys (0029/0030) share the derived-data deletion contract.
    if node_type != "device" {
        sqlx::query("DELETE FROM entity_baselines WHERE subject_type = $1 AND subject_id = $2")
            .bind(node_type).bind(id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM entity_journeys WHERE subject_type = $1 AND subject_id = $2")
            .bind(node_type).bind(id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Owner binding seed + consumer helper + rebuild
// ---------------------------------------------------------------------------------------------

/// Idempotently maintain a `confirmed` binding between the owner speaker and owner person (both
/// `is_owner`); retire it if ownership moves. Called from the driver on startup / each pass.
pub async fn seed_owner_binding(pool: &PgPool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(GRAPH_LOCK_KEY).execute(&mut *tx).await?;
    let owner_speaker: Option<Uuid> =
        sqlx::query_scalar("SELECT speaker_id FROM speakers WHERE is_owner = true LIMIT 1")
            .fetch_optional(&mut *tx).await?;
    let owner_person: Option<Uuid> =
        sqlx::query_scalar("SELECT person_id FROM persons WHERE is_owner = true LIMIT 1")
            .fetch_optional(&mut *tx).await?;

    // Retire any prior auto-seeded owner edge that no longer matches (ownership moved).
    sqlx::query(
        "DELETE FROM entity_edges WHERE edge_type = 'same_identity_candidate' \
           AND metadata->>'owner_seed' = 'true' \
           AND NOT (src_id = $1 OR dst_id = $1 OR src_id = $2 OR dst_id = $2)",
    )
    .bind(owner_speaker.map(|u| u.to_string()).unwrap_or_default())
    .bind(owner_person.map(|u| u.to_string()).unwrap_or_default())
    .execute(&mut *tx)
    .await?;

    if let (Some(sp), Some(pe)) = (owner_speaker, owner_person) {
        let (src, dst) = graph::canonical_pair(
            NodeRef::new(NodeType::Speaker, sp.to_string()),
            NodeRef::new(NodeType::Person, pe.to_string()),
        );
        sqlx::query(
            "INSERT INTO entity_edges \
               (edge_id, edge_type, src_type, src_id, dst_type, dst_id, observation_count, \
                confidence, evidence, metadata, status, created_at, updated_at) \
             VALUES ($1,'same_identity_candidate',$2,$3,$4,$5,0,1.0,'[]'::jsonb, \
                     '{\"owner_seed\":true}'::jsonb,'confirmed', now(), now()) \
             ON CONFLICT (edge_type, src_type, src_id, dst_type, dst_id) DO UPDATE SET \
               status = 'confirmed', metadata = entity_edges.metadata || '{\"owner_seed\":true}'::jsonb, \
               updated_at = now()",
        )
        .bind(Uuid::now_v7())
        .bind(src.node_type.as_str()).bind(&src.id)
        .bind(dst.node_type.as_str()).bind(&dst.id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Consumer helper (exported for hushai-rag): the person id bound to a speaker via a CONFIRMED
/// edge, if any — the only edge state consumers may union voice/face history across (§1.4).
pub async fn bound_person_for_speaker(pool: &PgPool, speaker: Uuid) -> anyhow::Result<Option<Uuid>> {
    let sp = speaker.to_string();
    let pid: Option<String> = sqlx::query_scalar(
        "SELECT CASE WHEN src_type='person' THEN src_id ELSE dst_id END \
         FROM entity_edges \
         WHERE edge_type='same_identity_candidate' AND status='confirmed' \
           AND ((src_type='speaker' AND src_id=$1) OR (dst_type='speaker' AND dst_id=$1)) \
         ORDER BY confidence DESC NULLS LAST, edge_id LIMIT 1",
    )
    .bind(&sp)
    .fetch_optional(pool)
    .await?;
    Ok(pid.and_then(|s| Uuid::parse_str(&s).ok()))
}

// ---------------------------------------------------------------------------------------------
// Daily digest (§1.6 / Phase E) — the patterns producer wrapped in the pass's advisory lock
// ---------------------------------------------------------------------------------------------

/// Materialize + return the daily-digest `sections` for a pinned ISO civil date (`YYYY-MM-DD`). The
/// admin `POST /v1/graph/digests/{date}` + the eval force a pinned date HERE (the wall-clock driver
/// can't be used deterministically). Advisory-locked on `GRAPH_LOCK_KEY` so it never reads edges
/// mid-fold and serializes with a concurrent pass. Determinism = the underlying capture-anchored data.
pub async fn generate_digest_for_date(
    pool: &PgPool,
    opts: &GraphOpts,
    date_iso: &str,
) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(GRAPH_LOCK_KEY).execute(&mut *tx).await?;
    let civil_day: i32 = sqlx::query_scalar("SELECT ($1::date - DATE '1970-01-01')::int")
        .bind(date_iso)
        .fetch_one(&mut *tx)
        .await?;
    let cfg_hash = graph::config_hash(&opts.cfg);
    let sections = crate::patterns::build_and_upsert_digest(
        &mut tx,
        &opts.cfg,
        opts.tz_offset_secs,
        &cfg_hash,
        civil_day as i64,
    )
    .await?;
    tx.commit().await?;
    Ok(sections)
}

/// Worker-0 wall-clock driver (§1.6): once local wall-clock passes `digest_hour_local` AND no row
/// exists for YESTERDAY's local civil date, materialize it (idempotent by PK). Returns the civil-day
/// index generated, or `None` when it's too early today / already done. `GRAPH_DIGEST_HOUR_LOCAL` is
/// a NON-hashed knob (not in `GraphCfg`/`config_hash`) — this path is never eval-exercised (the eval
/// forces a pinned date), so it needs no byte-determinism guarantee, only idempotency.
pub async fn maybe_generate_daily_digest(
    pool: &PgPool,
    opts: &GraphOpts,
    digest_hour_local: i64,
) -> anyhow::Result<Option<i64>> {
    let now_secs: i64 =
        sqlx::query_scalar("SELECT (extract(epoch from now()))::bigint").fetch_one(pool).await?;
    let local_secs = now_secs + opts.tz_offset_secs;
    let local_hour = local_secs.rem_euclid(86_400) / 3_600;
    if local_hour < digest_hour_local {
        return Ok(None); // too early in the local day — yesterday's digest waits for the hour gate
    }
    let yesterday = local_secs.div_euclid(86_400) - 1;
    const EXISTS_SQL: &str =
        "SELECT 1 FROM daily_digests WHERE digest_date = (DATE '1970-01-01' + ($1::int))";
    let exists: Option<i32> =
        sqlx::query_scalar(EXISTS_SQL).bind(yesterday as i32).fetch_optional(pool).await?;
    if exists.is_some() {
        return Ok(None);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(GRAPH_LOCK_KEY).execute(&mut *tx).await?;
    // Re-check under the lock — a concurrent worker/pass may have just generated it.
    let exists2: Option<i32> =
        sqlx::query_scalar(EXISTS_SQL).bind(yesterday as i32).fetch_optional(&mut *tx).await?;
    if exists2.is_some() {
        tx.commit().await?;
        return Ok(None);
    }
    let cfg_hash = graph::config_hash(&opts.cfg);
    crate::patterns::build_and_upsert_digest(
        &mut tx,
        &opts.cfg,
        opts.tz_offset_secs,
        &cfg_hash,
        yesterday,
    )
    .await?;
    tx.commit().await?;
    Ok(Some(yesterday))
}

/// Explicit rebuild (admin / GRAPH_REBUILD_ON_START): truncate derived rows, reset watermarks,
/// then refold from scratch. Confirmed/rejected binding decisions are DERIVED too — a rebuild
/// re-runs trials but preserves nothing by design; the owner seed re-creates the owner edge.
pub async fn rebuild(pool: &PgPool, opts: &GraphOpts) -> anyhow::Result<GraphStats> {
    {
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(GRAPH_LOCK_KEY).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM entity_edges").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM entity_baselines").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM entity_journeys").execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE graph_state SET events_watermark = to_timestamp(0), \
               conversations_watermark = to_timestamp(0), config_hash = NULL, updated_at = now() WHERE id = 1",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    seed_owner_binding(pool).await?;
    // Drain to convergence (bounded loop: each pass consumes up to the budget).
    let mut total = GraphStats::default();
    loop {
        let s = graph_pass(pool, opts).await?;
        total.events_consumed += s.events_consumed;
        total.conversations_consumed += s.conversations_consumed;
        total.edges_upserted += s.edges_upserted;
        total.bindings_surfaced += s.bindings_surfaced;
        total.baselines_recomputed += s.baselines_recomputed;
        total.anomalies_emitted += s.anomalies_emitted;
        let stop = s.events_consumed == 0 && s.conversations_consumed == 0;
        total.anomaly_event_ids.extend(s.anomaly_event_ids);
        if stop {
            break;
        }
    }
    Ok(total)
}
