//! Gotham entity/link graph — pure deterministic cores (spec: Gotham.md §1.5, Part 1).
//!
//! Everything here is DB-free, clock-free, RNG-free: pair generation from visit lists, canonical
//! edge ordering, voice↔face binding scoring, anomaly predicates, cross-camera journey stitching,
//! evidence-sample merging, and [`GraphCfg`] + [`config_hash`]. The DB orchestrator
//! ([`crate::graph_pass`]) calls these; the RAG service narrates the result at chat time. Same
//! doctrine as `profiles.rs` / `threading.rs`: compute deterministically here, narrate at answer
//! time — which is exactly what makes the layer evaluable with byte-stable baselines.
//!
//! Determinism rules (the `profiles.rs` idioms): integer/ratio math only, confidences rounded to
//! 4 decimals before storage, total tie-breaks on every sort, `BTreeMap`/`BTreeSet` iteration.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

pub const NANOS_PER_SEC: i64 = 1_000_000_000;

// ---------------------------------------------------------------------------------------------
// Node + edge vocabulary (mirrors the migration 0028 CHECK constraints)
// ---------------------------------------------------------------------------------------------

/// A graph node's catalog kind. Ordering is by the stored text form so canonicalization matches
/// the `entity_edges_identity_idx` unique index exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeType {
    Person,
    Speaker,
    Plate,
    Device,
}

impl NodeType {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeType::Person => "person",
            NodeType::Speaker => "speaker",
            NodeType::Plate => "plate",
            NodeType::Device => "device",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "person" => Some(NodeType::Person),
            "speaker" => Some(NodeType::Speaker),
            "plate" => Some(NodeType::Plate),
            "device" => Some(NodeType::Device),
            _ => None,
        }
    }
}

/// The five materialized relationship kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    CoPresent,
    ConversedWith,
    ArrivedWithVehicle,
    SameIdentityCandidate,
    VisitsPlace,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::CoPresent => "co_present",
            EdgeKind::ConversedWith => "conversed_with",
            EdgeKind::ArrivedWithVehicle => "arrived_with_vehicle",
            EdgeKind::SameIdentityCandidate => "same_identity_candidate",
            EdgeKind::VisitsPlace => "visits_place",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "co_present" => Some(EdgeKind::CoPresent),
            "conversed_with" => Some(EdgeKind::ConversedWith),
            "arrived_with_vehicle" => Some(EdgeKind::ArrivedWithVehicle),
            "same_identity_candidate" => Some(EdgeKind::SameIdentityCandidate),
            "visits_place" => Some(EdgeKind::VisitsPlace),
            _ => None,
        }
    }

    /// Undirected types canonicalize their endpoints (smaller stored as `src`); directed types
    /// keep natural direction (`arrived_with_vehicle` person→plate, `visits_place` entity→device).
    pub fn is_undirected(self) -> bool {
        matches!(
            self,
            EdgeKind::CoPresent | EdgeKind::ConversedWith | EdgeKind::SameIdentityCandidate
        )
    }
}

/// An edge endpoint: `(node_type, node_id)`. `id` is the canonical lowercase hyphenated uuid
/// string for catalog nodes, or the `device_id` verbatim for devices (0028: text endpoints, no FK).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeRef {
    pub node_type: NodeType,
    pub id: String,
}

impl NodeRef {
    pub fn new(node_type: NodeType, id: impl Into<String>) -> Self {
        Self { node_type, id: id.into() }
    }

    /// The tuple canonical ordering + the unique index both compare on.
    fn sort_key(&self) -> (&'static str, &str) {
        (self.node_type.as_str(), self.id.as_str())
    }
}

/// Canonicalize an undirected pair: the lexicographically smaller `(type, id)` endpoint becomes
/// `src`. Producer-enforced so the unique index dedups with no mirror-row problem (§1.2). Self
/// pairs are returned unchanged (callers drop them).
pub fn canonical_pair(a: NodeRef, b: NodeRef) -> (NodeRef, NodeRef) {
    if a.sort_key() <= b.sort_key() {
        (a, b)
    } else {
        (b, a)
    }
}

// ---------------------------------------------------------------------------------------------
// Co-presence pair generation
// ---------------------------------------------------------------------------------------------

/// One subject's presence interval on one device (a coalesced visit, from `events`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphVisit {
    pub node: NodeRef,
    pub device_id: String,
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
}

/// Two intervals overlap within `slack_nanos` of touching (the `profiles::co_present` rule).
pub fn visits_overlap(a: &GraphVisit, b: &GraphVisit, slack_nanos: i64) -> bool {
    a.device_id == b.device_id
        && a.start_unix_nanos < b.end_unix_nanos + slack_nanos
        && a.end_unix_nanos > b.start_unix_nanos - slack_nanos
}

/// Distinct canonical co-presence pairs among `visits` (assumed one device / one window). Two
/// visits by DIFFERENT subjects overlapping within `slack_nanos` yield a canonical pair.
///
/// N²-guard (determinism-relevant → hashed): if more than `max_subjects` distinct subjects appear,
/// only the first `max_subjects` in canonical order are considered — a bounded, deterministic
/// subset (graph_pass logs the drop). Returns pairs sorted for a stable upsert order.
pub fn co_present_pairs(
    visits: &[GraphVisit],
    slack_nanos: i64,
    max_subjects: usize,
) -> Vec<(NodeRef, NodeRef)> {
    // Deterministic subject cap.
    let mut subjects: BTreeSet<NodeRef> = BTreeSet::new();
    for v in visits {
        subjects.insert(v.node.clone());
    }
    let kept: BTreeSet<NodeRef> = subjects.into_iter().take(max_subjects.max(1)).collect();

    let mut pairs: BTreeSet<(NodeRef, NodeRef)> = BTreeSet::new();
    for i in 0..visits.len() {
        if !kept.contains(&visits[i].node) {
            continue;
        }
        for j in (i + 1)..visits.len() {
            if visits[i].node == visits[j].node || !kept.contains(&visits[j].node) {
                continue;
            }
            if visits_overlap(&visits[i], &visits[j], slack_nanos) {
                pairs.insert(canonical_pair(visits[i].node.clone(), visits[j].node.clone()));
            }
        }
    }
    pairs.into_iter().collect()
}

// ---------------------------------------------------------------------------------------------
// Voice↔face binding scoring (§1.4)
// ---------------------------------------------------------------------------------------------

/// Persisted binding counters (live in the edge's `evidence` jsonb; the score is recomputable).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BindCounters {
    pub together: i64,
    pub speaker_only: i64,
    pub person_only: i64,
}

/// Session-level Jaccard: `together / (together + speaker_only + person_only)`, rounded to 4
/// decimals. Zero denominator → 0.0.
pub fn binding_confidence(c: &BindCounters) -> f32 {
    let denom = c.together + c.speaker_only + c.person_only;
    if denom <= 0 {
        return 0.0;
    }
    round4(c.together as f32 / denom as f32)
}

/// Surfacing gate (§1.4): an edge enters the review queue only when ALL hold — enough joint
/// sessions, confidence over the floor, AND a clear margin over the speaker's runner-up person
/// candidate (defeats the always-together-couple confound). `runner_up_confidence` is 0.0 when
/// there is no competitor.
pub fn binding_should_surface(
    c: &BindCounters,
    runner_up_confidence: f32,
    cfg: &GraphCfg,
) -> bool {
    let conf = binding_confidence(c);
    c.together >= cfg.bind_min_sessions
        && conf >= cfg.bind_min_confidence
        && (conf - runner_up_confidence) >= cfg.bind_margin
}

// ---------------------------------------------------------------------------------------------
// Evidence samples (newest-N provenance, capped at GRAPH_EDGE_SAMPLE_CAP)
// ---------------------------------------------------------------------------------------------

/// One provenance sample kept on an edge (`evidence` jsonb array element).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EvidenceSample {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segment_id: Option<String>,
    pub t: i64,
}

/// Merge `incoming` into `existing`, keeping the newest `cap` samples by time `t` (ties broken by
/// event_id/segment_id for determinism). The persisted provenance window (§1.2).
pub fn merge_evidence_samples(
    existing: &[EvidenceSample],
    incoming: &[EvidenceSample],
    cap: usize,
) -> Vec<EvidenceSample> {
    let mut all: Vec<EvidenceSample> = existing.iter().chain(incoming.iter()).cloned().collect();
    // Newest first; total tie-break so equal-t samples order deterministically.
    all.sort_by(|a, b| {
        b.t.cmp(&a.t)
            .then_with(|| a.event_id.cmp(&b.event_id))
            .then_with(|| a.segment_id.cmp(&b.segment_id))
    });
    all.dedup_by(|a, b| a.event_id == b.event_id && a.segment_id == b.segment_id && a.t == b.t);
    all.truncate(cap.max(1));
    all
}

// ---------------------------------------------------------------------------------------------
// Time bucketing (baselines / anomalies)
// ---------------------------------------------------------------------------------------------

/// Hour-of-week bucket `[0, 168)` in local civil time. Convention: Sunday = 0 (epoch day 0 was a
/// Thursday, so `(days + 4) mod 7` gives 0 = Sunday). Bucket = `weekday * 24 + hour_of_day`.
pub fn hour_of_week(unix_nanos: i64, tz_offset_secs: i64) -> usize {
    let secs = unix_nanos.div_euclid(NANOS_PER_SEC) + tz_offset_secs;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let weekday = (days + 4).rem_euclid(7);
    (weekday * 24 + tod / 3600) as usize
}

// ---------------------------------------------------------------------------------------------
// Anomaly predicates (§1.6) — pure; Wave 2's patterns.rs applies them against baselines
// ---------------------------------------------------------------------------------------------

/// A baseline is "mature" enough to judge deviations against (`visits_in_window ≥` gate).
pub fn baseline_mature(visits_in_window: i64, cfg: &GraphCfg) -> bool {
    visits_in_window >= cfg.anomaly_min_visits
}

/// `off_schedule_presence`: a visit lands in an hour-of-week bucket holding less than
/// `GRAPH_ANOMALY_HOUR_MIN_FRAC` of the subject's histogram mass. Gated on baseline maturity.
pub fn is_off_schedule(histogram: &[i32], hour_bucket: usize, cfg: &GraphCfg) -> bool {
    let total: i64 = histogram.iter().map(|&h| h as i64).sum();
    if total <= 0 {
        return false;
    }
    if !baseline_mature(total, cfg) {
        return false;
    }
    let bucket_mass = histogram.get(hour_bucket).copied().unwrap_or(0) as f32;
    (bucket_mass / total as f32) < cfg.anomaly_hour_min_frac
}

/// `unknown_person_cluster`: at least `GRAPH_ANOMALY_UNKNOWN_CLUSTER_MIN` distinct unknown-person
/// subjects co-present in one window on one device.
pub fn is_unknown_cluster(distinct_unknown: usize, cfg: &GraphCfg) -> bool {
    distinct_unknown as i64 >= cfg.anomaly_unknown_cluster_min
}

/// `first_time_pairing`: a co_present edge transitions 0→1 observations AND both entities are
/// established regulars (mature baselines).
pub fn is_first_time_pairing(prev_observation_count: i64, a_mature: bool, b_mature: bool) -> bool {
    prev_observation_count == 0 && a_mature && b_mature
}

/// `new_vehicle_for_person`: a newly created `arrived_with_vehicle` edge for a person who already
/// has an established edge to a DIFFERENT vehicle.
pub fn is_new_vehicle_for_person(edge_is_new: bool, has_established_other_vehicle: bool) -> bool {
    edge_is_new && has_established_other_vehicle
}

// ---------------------------------------------------------------------------------------------
// Cross-camera journey stitching (§1.3, Pillar G5)
// ---------------------------------------------------------------------------------------------

/// One subject visit fed to the stitcher (already coalesced per device).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JourneyVisit {
    pub device_id: String,
    pub arrive_ns: i64,
    pub depart_ns: i64,
    pub event_id: Option<String>,
}

/// One hop of a stitched journey.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JourneyHop {
    pub device_id: String,
    pub arrive_ns: i64,
    pub depart_ns: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
}

/// A stitched multi-device journey.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StitchedJourney {
    pub hops: Vec<JourneyHop>,
    pub started_at_unix_nanos: i64,
    pub ended_at_unix_nanos: i64,
}

/// Stitch a subject's visits into cross-camera journeys: a visit on device B starting within
/// `gap_nanos` of the running chain's last departure extends it; a larger gap starts a new chain.
/// Consecutive same-device visits collapse into one hop. Only chains spanning ≥ 2 distinct
/// devices are emitted (single-device visits are events/profiles territory — §1.3).
pub fn stitch_journeys(visits: &[JourneyVisit], gap_nanos: i64) -> Vec<StitchedJourney> {
    let mut sorted: Vec<&JourneyVisit> = visits.iter().collect();
    sorted.sort_by(|a, b| {
        a.arrive_ns
            .cmp(&b.arrive_ns)
            .then_with(|| a.depart_ns.cmp(&b.depart_ns))
            .then_with(|| a.device_id.cmp(&b.device_id))
    });

    let mut journeys: Vec<StitchedJourney> = Vec::new();
    let mut chain: Vec<JourneyHop> = Vec::new();

    let flush = |chain: &mut Vec<JourneyHop>, out: &mut Vec<StitchedJourney>| {
        let distinct: BTreeSet<&str> = chain.iter().map(|h| h.device_id.as_str()).collect();
        if distinct.len() >= 2 {
            let start = chain.first().map(|h| h.arrive_ns).unwrap_or(0);
            let end = chain.iter().map(|h| h.depart_ns).max().unwrap_or(start);
            out.push(StitchedJourney {
                hops: chain.clone(),
                started_at_unix_nanos: start,
                ended_at_unix_nanos: end,
            });
        }
        chain.clear();
    };

    for v in sorted {
        match chain.last_mut() {
            Some(last) if v.arrive_ns - last.depart_ns <= gap_nanos => {
                if last.device_id == v.device_id {
                    // Same device continues the current hop.
                    last.depart_ns = last.depart_ns.max(v.depart_ns);
                } else {
                    chain.push(JourneyHop {
                        device_id: v.device_id.clone(),
                        arrive_ns: v.arrive_ns,
                        depart_ns: v.depart_ns,
                        event_id: v.event_id.clone(),
                    });
                }
            }
            _ => {
                flush(&mut chain, &mut journeys);
                chain.push(JourneyHop {
                    device_id: v.device_id.clone(),
                    arrive_ns: v.arrive_ns,
                    depart_ns: v.depart_ns,
                    event_id: v.event_id.clone(),
                });
            }
        }
    }
    flush(&mut chain, &mut journeys);
    journeys
}

// ---------------------------------------------------------------------------------------------
// Config + fingerprint
// ---------------------------------------------------------------------------------------------

/// The determinism-relevant (★) GRAPH_* knobs. Every field participates in [`config_hash`];
/// changing any starts a new eval lineage (the `ThreaderCfg` precedent). Operational knobs
/// (enabled/interval/max_events_per_pass/rebuild) do NOT affect the final edge set and are held
/// on the worker `GraphOpts`, not here.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphCfg {
    /// Settle window before an event is eligible to fold (≥ 2× event session bucket).
    pub grace_secs: i64,
    /// Overlap slack for co_present / binding trials.
    pub copresence_slack_secs: i64,
    /// Pairwise subject cap per window (N² guard).
    pub copresence_max_subjects: usize,
    /// person↔plate correlation window.
    pub vehicle_corr_window_secs: i64,
    /// Evidence samples kept per edge.
    pub edge_sample_cap: usize,
    /// Binding: min `together` before a candidate surfaces.
    pub bind_min_sessions: i64,
    /// Binding: min Jaccard confidence.
    pub bind_min_confidence: f32,
    /// Binding: top-1 vs top-2 margin.
    pub bind_margin: f32,
    /// Baseline trailing window.
    pub baseline_window_days: i64,
    /// Baseline maturity gate for anomalies.
    pub anomaly_min_visits: i64,
    /// Off-schedule threshold (fraction of histogram mass).
    pub anomaly_hour_min_frac: f32,
    /// Unknown-person cluster size.
    pub anomaly_unknown_cluster_min: i64,
    /// Cross-camera hop stitch gap.
    pub journey_gap_secs: i64,
}

impl Default for GraphCfg {
    fn default() -> Self {
        Self {
            grace_secs: 90,
            copresence_slack_secs: 120,
            copresence_max_subjects: 12,
            vehicle_corr_window_secs: 180,
            edge_sample_cap: 16,
            bind_min_sessions: 3,
            bind_min_confidence: 0.6,
            bind_margin: 0.2,
            baseline_window_days: 30,
            anomaly_min_visits: 5,
            anomaly_hour_min_frac: 0.05,
            anomaly_unknown_cluster_min: 3,
            journey_gap_secs: 600,
        }
    }
}

/// Stable fingerprint of the ★ knob set (hex, 16 chars — the `threading::config_hash` shape).
/// Stored on `graph_state` and every edge/baseline/journey row; the eval manifest folds the same
/// GRAPH_* knobs, so a knob change shows up as a new lineage on both sides.
pub fn config_hash(cfg: &GraphCfg) -> String {
    let canonical = format!(
        "grace={};copslack={};copmax={};vehwin={};sample={};bmin={};bconf={:.4};bmargin={:.4};bwin={};amin={};ahour={:.4};acluster={};jgap={}",
        cfg.grace_secs,
        cfg.copresence_slack_secs,
        cfg.copresence_max_subjects,
        cfg.vehicle_corr_window_secs,
        cfg.edge_sample_cap,
        cfg.bind_min_sessions,
        cfg.bind_min_confidence,
        cfg.bind_margin,
        cfg.baseline_window_days,
        cfg.anomaly_min_visits,
        cfg.anomaly_hour_min_frac,
        cfg.anomaly_unknown_cluster_min,
        cfg.journey_gap_secs,
    );
    let digest = Sha256::digest(canonical.as_bytes());
    hex::encode(&digest[..8])
}

/// Round to 4 decimals (all stored confidences — determinism, §1.3).
pub fn round4(x: f32) -> f32 {
    (x * 10_000.0).round() / 10_000.0
}

// ---------------------------------------------------------------------------------------------
// Baselines (§1.6 / migration 0029) — pure recompute over a subject's trailing-window visits
// ---------------------------------------------------------------------------------------------

/// Companion tally cap kept in `entity_baselines.companion_stats` (top-K, deterministic order).
/// A code constant, NOT a ★ knob — it does not fold into [`config_hash`] (which tracks the GRAPH_*
/// ENV knobs). NOTE: changing it silently alters stored `companion_stats` with no config-hash
/// change; the eval only notices via a fixture that asserts companion_stats (none do today), so
/// treat a change as baseline-affecting and re-freeze deliberately.
pub const COMPANION_TOP_K: usize = 8;

/// Cap on the daily digest's `top_visitors` list (§1.6 / migration 0029). Like [`COMPANION_TOP_K`]
/// a code constant, NOT a ★ knob — it does not fold into [`config_hash`]. It bounds a *rendered
/// report* rather than a stored derivation, so a change re-orders a digest's tail but never a graph
/// baseline; no fixture asserts a full top-K list, so re-freeze deliberately if you change it.
pub const DIGEST_TOP_VISITORS: usize = 10;

/// One subject visit fed to the baseline folder (device + interval; dwell = end − start).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineVisit {
    pub device_id: String,
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
}

/// One companion tally (from the subject's `co_present` edges), fed to the baseline folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanionTally {
    pub node_type: NodeType,
    pub node_id: String,
    pub observations: i64,
}

/// A subject's recomputed baseline (the computed columns of the 0029 `entity_baselines` row).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EntityBaseline {
    /// 168 hour-of-week buckets (local civil time, fixed offset). `sum == visits_in_window`.
    pub hour_histogram: Vec<i32>,
    pub visits_in_window: i64,
    pub dwell_p50_secs: Option<i32>,
    pub dwell_p90_secs: Option<i32>,
    /// `{"<device_id>":{"visits":N,"last_seen_ns":..}}`
    pub device_stats: serde_json::Value,
    /// `[{"node_type":..,"node_id":..,"observations":N}]`, top-K, deterministic order.
    pub companion_stats: serde_json::Value,
}

/// Deterministic nearest-rank percentile (`q ∈ [0,1]`) over an unsorted slice; `None` if empty.
pub fn percentile(values: &[i64], q: f64) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let q = q.clamp(0.0, 1.0);
    let n = v.len();
    // Nearest-rank: rank = ceil(q · n), 1-indexed, clamped into [1, n].
    let rank = ((q * n as f64).ceil() as usize).clamp(1, n);
    Some(v[rank - 1])
}

/// Fold a subject's trailing-window visits + companion tallies into an [`EntityBaseline`].
/// Deterministic throughout: histogram by [`hour_of_week`], dwell percentiles nearest-rank,
/// `device_stats` in `BTreeMap` (device_id) order, companions top-K sorted observations-desc then
/// node type/id.
pub fn build_baseline(
    visits: &[BaselineVisit],
    companions: &[CompanionTally],
    tz_offset_secs: i64,
) -> EntityBaseline {
    let mut hist = vec![0i32; 168];
    let mut dwells: Vec<i64> = Vec::with_capacity(visits.len());
    // device_id → (visits, last_seen_ns)
    let mut dev: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for v in visits {
        let b = hour_of_week(v.start_unix_nanos, tz_offset_secs);
        if b < hist.len() {
            hist[b] += 1;
        }
        dwells.push(((v.end_unix_nanos - v.start_unix_nanos).max(0)) / NANOS_PER_SEC);
        let e = dev.entry(v.device_id.clone()).or_insert((0, i64::MIN));
        e.0 += 1;
        e.1 = e.1.max(v.end_unix_nanos);
    }
    let device_stats = serde_json::Value::Object(
        dev.into_iter()
            .map(|(k, (visits, last))| {
                (k, serde_json::json!({ "visits": visits, "last_seen_ns": last }))
            })
            .collect(),
    );

    let mut comps = companions.to_vec();
    comps.sort_by(|a, b| {
        b.observations
            .cmp(&a.observations)
            .then_with(|| a.node_type.as_str().cmp(b.node_type.as_str()))
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    comps.truncate(COMPANION_TOP_K);
    let companion_stats = serde_json::Value::Array(
        comps
            .iter()
            .map(|c| {
                serde_json::json!({
                    "node_type": c.node_type.as_str(),
                    "node_id": c.node_id,
                    "observations": c.observations,
                })
            })
            .collect(),
    );

    EntityBaseline {
        visits_in_window: visits.len() as i64,
        dwell_p50_secs: percentile(&dwells, 0.5).map(|x| x as i32),
        dwell_p90_secs: percentile(&dwells, 0.9).map(|x| x as i32),
        hour_histogram: hist,
        device_stats,
        companion_stats,
    }
}

// ---------------------------------------------------------------------------------------------
// Anomaly kinds + emission helpers (§1.6)
// ---------------------------------------------------------------------------------------------

/// The `events.event_type` all pattern anomalies carry (0014 free-text vocabulary, no migration).
pub const EVENT_TYPE_PATTERN_ANOMALY: &str = "pattern_anomaly";

/// The four §1.6 anomaly kinds (stored in `events.metadata.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnomalyKind {
    OffSchedulePresence,
    FirstTimePairing,
    UnknownPersonCluster,
    NewVehicleForPerson,
}

impl AnomalyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AnomalyKind::OffSchedulePresence => "off_schedule_presence",
            AnomalyKind::FirstTimePairing => "first_time_pairing",
            AnomalyKind::UnknownPersonCluster => "unknown_person_cluster",
            AnomalyKind::NewVehicleForPerson => "new_vehicle_for_person",
        }
    }
}

/// Local civil day index (days since epoch under the fixed offset) — the anomaly dedup bucket, so
/// an anomaly fires at most once per subject per day no matter how many passes re-drain it.
pub fn civil_day(unix_nanos: i64, tz_offset_secs: i64) -> i64 {
    (unix_nanos.div_euclid(NANOS_PER_SEC) + tz_offset_secs).div_euclid(86_400)
}

/// Idempotent anomaly `dedup_key = "anom:<kind>:<subject>:<bucket>"` (§1.6). `subject` is a stable
/// key ("person:<uuid>" / "device:<id>"); `bucket` is typically [`civil_day`] as a string.
///
/// off_schedule detection itself reuses [`is_off_schedule`]: `patterns::recompute_and_flag` judges
/// each visit (in capture order) against the histogram of the subject's STRICTLY-EARLIER visits —
/// the spec's incremental "new visit vs prior baseline" model — so a subject's first appearances
/// (incl. the enrollment clip) never fire, only a later violation of an established rhythm.
pub fn anom_dedup_key(kind: AnomalyKind, subject: &str, bucket: &str) -> String {
    format!("anom:{}:{}:{}", kind.as_str(), subject, bucket)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: i64 = NANOS_PER_SEC;
    // 2026-06-12 08:12:00 UTC — a Friday.
    const T0: i64 = 1_781_251_920_000_000_000;

    fn n(t: NodeType, id: &str) -> NodeRef {
        NodeRef::new(t, id)
    }

    fn vis(t: NodeType, id: &str, dev: &str, start: i64, end: i64) -> GraphVisit {
        GraphVisit { node: n(t, id), device_id: dev.into(), start_unix_nanos: start, end_unix_nanos: end }
    }

    #[test]
    fn canonical_pair_orders_by_stored_text_and_is_symmetric() {
        let a = n(NodeType::Person, "bbb");
        let b = n(NodeType::Speaker, "aaa");
        // "person" < "speaker" so person is always src regardless of arg order.
        assert_eq!(canonical_pair(a.clone(), b.clone()), (a.clone(), b.clone()));
        assert_eq!(canonical_pair(b.clone(), a.clone()), (a, b));
        // Same type falls back to id ordering.
        let p1 = n(NodeType::Person, "aaa");
        let p2 = n(NodeType::Person, "bbb");
        assert_eq!(canonical_pair(p2.clone(), p1.clone()), (p1, p2));
    }

    #[test]
    fn overlap_respects_device_and_slack() {
        let a = vis(NodeType::Person, "a", "cam-a", T0, T0 + 60 * SEC);
        let b = vis(NodeType::Speaker, "b", "cam-a", T0 + 70 * SEC, T0 + 90 * SEC); // 10s apart
        assert!(!visits_overlap(&a, &b, 0));
        assert!(visits_overlap(&a, &b, 30 * SEC)); // slack bridges the gap
        let other_dev = vis(NodeType::Speaker, "b", "cam-b", T0, T0 + 60 * SEC);
        assert!(!visits_overlap(&a, &other_dev, 10 * SEC)); // different device never overlaps
    }

    #[test]
    fn co_present_pairs_are_canonical_deduped_and_capped() {
        let visits = vec![
            vis(NodeType::Person, "p1", "cam-a", T0, T0 + 60 * SEC),
            vis(NodeType::Speaker, "s1", "cam-a", T0 + 10 * SEC, T0 + 50 * SEC),
            vis(NodeType::Person, "p1", "cam-a", T0 + 20 * SEC, T0 + 40 * SEC), // same subject again
        ];
        let pairs = co_present_pairs(&visits, 0, 12);
        assert_eq!(pairs.len(), 1, "one distinct pair despite the repeat");
        assert_eq!(pairs[0], (n(NodeType::Person, "p1"), n(NodeType::Speaker, "s1")));

        // Cap keeps only the first N subjects in canonical order → drops pairs with the rest.
        let many = vec![
            vis(NodeType::Person, "a", "cam-a", T0, T0 + 60 * SEC),
            vis(NodeType::Person, "b", "cam-a", T0, T0 + 60 * SEC),
            vis(NodeType::Person, "c", "cam-a", T0, T0 + 60 * SEC),
        ];
        let capped = co_present_pairs(&many, 0, 2);
        assert_eq!(capped, vec![(n(NodeType::Person, "a"), n(NodeType::Person, "b"))]);
    }

    #[test]
    fn binding_confidence_is_jaccard_rounded() {
        let c = BindCounters { together: 3, speaker_only: 1, person_only: 0 };
        assert_eq!(binding_confidence(&c), 0.75);
        assert_eq!(binding_confidence(&BindCounters::default()), 0.0);
        // 1/3 rounds to 4 decimals.
        let c = BindCounters { together: 1, speaker_only: 1, person_only: 1 };
        assert_eq!(binding_confidence(&c), 0.3333);
    }

    #[test]
    fn binding_surfaces_only_past_all_three_gates() {
        let cfg = GraphCfg::default(); // min_sessions 3, min_conf 0.6, margin 0.2
        let strong = BindCounters { together: 4, speaker_only: 1, person_only: 0 }; // conf 0.8
        assert!(binding_should_surface(&strong, 0.0, &cfg));
        // Too few sessions.
        let few = BindCounters { together: 2, speaker_only: 0, person_only: 0 }; // conf 1.0
        assert!(!binding_should_surface(&few, 0.0, &cfg));
        // Confidence fine but a close runner-up kills the margin (always-together confound).
        assert!(!binding_should_surface(&strong, 0.7, &cfg));
    }

    #[test]
    fn evidence_keeps_newest_capped() {
        let existing = vec![
            EvidenceSample { event_id: Some("e1".into()), segment_id: None, t: 100 },
            EvidenceSample { event_id: Some("e2".into()), segment_id: None, t: 200 },
        ];
        let incoming = vec![
            EvidenceSample { event_id: Some("e2".into()), segment_id: None, t: 200 }, // dup
            EvidenceSample { event_id: Some("e3".into()), segment_id: None, t: 300 },
        ];
        let merged = merge_evidence_samples(&existing, &incoming, 2);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].event_id.as_deref(), Some("e3")); // newest first
        assert_eq!(merged[1].event_id.as_deref(), Some("e2"));
    }

    #[test]
    fn hour_of_week_sunday_zero() {
        // Epoch (Thursday 00:00 UTC) → weekday 4, hour 0 → bucket 96.
        assert_eq!(hour_of_week(0, 0), 4 * 24);
        // T0 is Friday 08:12 UTC → weekday 5, hour 8 → 5*24+8 = 128.
        assert_eq!(hour_of_week(T0, 0), 128);
        // A -9h offset pushes T0 back to Friday 23:12? No — 08:12-09:00 = prev day 23:12 Thu.
        assert_eq!(hour_of_week(T0, -9 * 3600), 4 * 24 + 23);
    }

    #[test]
    fn off_schedule_needs_maturity_and_rare_bucket() {
        let cfg = GraphCfg::default(); // min_visits 5, hour_min_frac 0.05
        let mut hist = vec![0i32; 168];
        hist[100] = 100; // subject almost always at bucket 100
        hist[50] = 2; // rare bucket
        assert!(is_off_schedule(&hist, 50, &cfg)); // 2/102 < 0.05 → anomaly
        assert!(!is_off_schedule(&hist, 100, &cfg)); // the usual bucket
        // Immature baseline never fires.
        let mut sparse = vec![0i32; 168];
        sparse[10] = 1;
        sparse[20] = 1;
        assert!(!is_off_schedule(&sparse, 20, &cfg));
    }

    #[test]
    fn other_anomaly_predicates() {
        let cfg = GraphCfg::default();
        assert!(is_unknown_cluster(3, &cfg));
        assert!(!is_unknown_cluster(2, &cfg));
        assert!(is_first_time_pairing(0, true, true));
        assert!(!is_first_time_pairing(1, true, true));
        assert!(!is_first_time_pairing(0, true, false));
        assert!(is_new_vehicle_for_person(true, true));
        assert!(!is_new_vehicle_for_person(true, false));
    }

    #[test]
    fn journeys_span_multiple_devices_and_split_on_gap() {
        let gap = 600 * SEC;
        let visits = vec![
            JourneyVisit { device_id: "front".into(), arrive_ns: T0, depart_ns: T0 + 60 * SEC, event_id: Some("e1".into()) },
            JourneyVisit { device_id: "garage".into(), arrive_ns: T0 + 120 * SEC, depart_ns: T0 + 180 * SEC, event_id: Some("e2".into()) },
            // hours later — new journey, single device only → NOT emitted
            JourneyVisit { device_id: "front".into(), arrive_ns: T0 + 10_000 * SEC, depart_ns: T0 + 10_060 * SEC, event_id: None },
        ];
        let js = stitch_journeys(&visits, gap);
        assert_eq!(js.len(), 1);
        assert_eq!(js[0].hops.len(), 2);
        assert_eq!(js[0].hops[0].device_id, "front");
        assert_eq!(js[0].hops[1].device_id, "garage");
        assert_eq!(js[0].started_at_unix_nanos, T0);
        assert_eq!(js[0].ended_at_unix_nanos, T0 + 180 * SEC);
    }

    #[test]
    fn journey_collapses_consecutive_same_device() {
        let gap = 600 * SEC;
        let visits = vec![
            JourneyVisit { device_id: "front".into(), arrive_ns: T0, depart_ns: T0 + 60 * SEC, event_id: None },
            JourneyVisit { device_id: "front".into(), arrive_ns: T0 + 90 * SEC, depart_ns: T0 + 120 * SEC, event_id: None },
            JourneyVisit { device_id: "garage".into(), arrive_ns: T0 + 150 * SEC, depart_ns: T0 + 200 * SEC, event_id: None },
        ];
        let js = stitch_journeys(&visits, gap);
        assert_eq!(js.len(), 1);
        assert_eq!(js[0].hops.len(), 2, "two front visits collapse to one hop");
        assert_eq!(js[0].hops[0].depart_ns, T0 + 120 * SEC);
    }

    #[test]
    fn config_hash_stable_and_knob_sensitive() {
        let a = config_hash(&GraphCfg::default());
        assert_eq!(a.len(), 16);
        assert_eq!(a, config_hash(&GraphCfg::default())); // stable
        let b = config_hash(&GraphCfg { bind_margin: 0.25, ..GraphCfg::default() });
        assert_ne!(a, b); // any ★ knob shifts the lineage
    }

    fn bvis(dev: &str, start: i64, dwell_secs: i64) -> BaselineVisit {
        BaselineVisit { device_id: dev.into(), start_unix_nanos: start, end_unix_nanos: start + dwell_secs * SEC }
    }

    #[test]
    fn percentile_is_nearest_rank() {
        assert_eq!(percentile(&[], 0.5), None);
        let v = [10, 20, 30, 40, 50];
        assert_eq!(percentile(&v, 0.5), Some(30)); // ceil(0.5*5)=3 → v[2]
        assert_eq!(percentile(&v, 0.9), Some(50)); // ceil(0.9*5)=5 → v[4]
        assert_eq!(percentile(&v, 0.0), Some(10)); // clamped to rank 1
        assert_eq!(percentile(&[7], 0.9), Some(7));
        // Unsorted input sorts internally.
        assert_eq!(percentile(&[50, 10, 30], 0.5), Some(30));
    }

    #[test]
    fn build_baseline_folds_histogram_dwell_devices_companions() {
        // 5 visits at Friday 08:xx (bucket 128), 1 at a different bucket, across two devices.
        let visits = vec![
            bvis("front", T0, 60),
            bvis("front", T0 + 86_400 * SEC * 7, 120), // +1 week → same bucket 128
            bvis("front", T0 + 86_400 * SEC * 14, 30),
            bvis("garage", T0, 90),
            bvis("garage", T0 + 3600 * SEC, 90), // +1h → bucket 129
        ];
        let comps = vec![
            CompanionTally { node_type: NodeType::Speaker, node_id: "s1".into(), observations: 2 },
            CompanionTally { node_type: NodeType::Person, node_id: "p9".into(), observations: 9 },
        ];
        let b = build_baseline(&visits, &comps, 0);
        assert_eq!(b.visits_in_window, 5);
        assert_eq!(b.hour_histogram.len(), 168);
        assert_eq!(b.hour_histogram.iter().map(|&h| h as i64).sum::<i64>(), 5);
        assert_eq!(b.hour_histogram[128], 4);
        assert_eq!(b.hour_histogram[129], 1);
        // dwell secs sorted: [30,60,90,90,120]; p50 nearest-rank rank3 → 90, p90 rank5 → 120.
        assert_eq!(b.dwell_p50_secs, Some(90));
        assert_eq!(b.dwell_p90_secs, Some(120));
        // device_stats keyed + counted.
        assert_eq!(b.device_stats["front"]["visits"], serde_json::json!(3));
        assert_eq!(b.device_stats["garage"]["visits"], serde_json::json!(2));
        // companions sorted observations-desc → p9(9) before s1(2).
        assert_eq!(b.companion_stats[0]["node_id"], serde_json::json!("p9"));
        assert_eq!(b.companion_stats[1]["node_id"], serde_json::json!("s1"));
    }

    #[test]
    fn off_schedule_as_of_prior_histogram() {
        // As-of model (patterns.rs): judge a visit against the histogram of STRICTLY-EARLIER visits.
        let cfg = GraphCfg::default(); // min_visits 5, hour_min_frac 0.05
        // Prior = 5 visits all at bucket 128 (a mature, concentrated rhythm).
        let mut prior = vec![0i32; 168];
        prior[128] = 5;
        // A later visit at a novel bucket 30: 0/5 of prior mass < 0.05 → off-schedule.
        assert!(is_off_schedule(&prior, 30, &cfg));
        // A later visit back at bucket 128: 5/5 = 1.0 → NOT off-schedule.
        assert!(!is_off_schedule(&prior, 128, &cfg));
        // Immature prior (4 visits) → nothing fires yet (a subject's early visits, incl. enroll).
        let mut immature = vec![0i32; 168];
        immature[128] = 4;
        assert!(!is_off_schedule(&immature, 30, &cfg));
        // Empty prior (the very first appearance) → never fires.
        assert!(!is_off_schedule(&vec![0i32; 168], 30, &cfg));
    }

    #[test]
    fn anomaly_dedup_key_and_civil_day() {
        assert_eq!(civil_day(0, 0), 0);
        assert_eq!(civil_day(86_400 * SEC + 5 * SEC, 0), 1);
        // −9h offset pushes an early-morning ns back to the previous civil day.
        assert_eq!(civil_day(3600 * SEC, -9 * 3600), -1);
        assert_eq!(
            anom_dedup_key(AnomalyKind::OffSchedulePresence, "person:abc", "20123"),
            "anom:off_schedule_presence:person:abc:20123"
        );
    }
}
