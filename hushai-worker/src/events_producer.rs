//! Roadmap A3 — the worker EVENT PRODUCER: turn each segment's just-committed detections into
//! sessionized `events` (the proactive VSaaS layer), then evaluate alert rules against each.
//!
//! Two entry points, one per processing lane, called from the post-commit hook in each lane:
//!   * [`derive_vision_events`] — from `process_vision_segment` after its write tx commits
//!     (faces → known_person/unknown_person, plates → plate_of_interest/plate_seen, objects →
//!     object_seen).
//!   * [`derive_audio_events`] — from `process_segment` after `write_transcript` commits
//!     (transcribed speech → `speech`, severity escalated on negative sentiment).
//!
//! Design (see `docs/feature-parity-roadmap.md` + AGENTS.md "Events & alerts"):
//!   * SESSIONIZATION via a coarse time-bucket baked into the `dedup_key`, so re-sightings of the
//!     same subject within `EVENTS_SESSION_BUCKET_SECS` coalesce into ONE event (the backend's
//!     `record_event` UPSERT extends end-time / keeps best score). One event per *distinct subject*
//!     per bucket, not one per raw detection.
//!   * KNOWN vs UNKNOWN is decided by `display_name IS NULL` (a freshly-minted, still-anonymous
//!     identity is "unknown"), NOT a match-vs-mint flag — correct under reprocessing.
//!   * RESILIENCE: a per-event failure is logged and skipped; the whole call is wrapped by the
//!     caller so an event/alert failure can NEVER fail the segment's core processing.
//!   * tz math for alert windows lives in Postgres (the worker has no chrono-tz) — see `alerts.rs`.

use std::collections::{HashMap, HashSet};

use hushai_backend::events::{self, NewEvent};
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::chunk::Sentence;
use crate::media::SegmentRow;
use crate::vision::face_match::FaceWrite;
use crate::vision::plates::plate_match::PlateWrite;
use crate::vision::write::ObjectWrite;

/// Producer tunables (parsed from `EVENTS_*` env in `config::WorkerConfig::from_env`).
#[derive(Debug, Clone)]
pub struct EventsConfig {
    /// Master switch for the event producer. When false, no events are emitted (nor alerts).
    pub enabled: bool,
    /// When true, each emitted event is run through the alert evaluator (`alerts::evaluate`).
    pub alerts_enabled: bool,
    /// Sessionization window (seconds) folded into the dedup_key so re-sightings coalesce.
    pub session_bucket_secs: i64,
    /// det_score floor for `object_seen` events (separate from the detector's keep floor so event
    /// noise tunes independently).
    pub object_min_score: f32,
    /// Drop detector `object_label='person'` from `object_seen` so it doesn't double-count the face
    /// lane's person identity.
    pub object_suppress_person: bool,
    /// mean_conf floor to emit a subject-less `plate_seen` for an uncatalogued read (no plate_id).
    pub plate_seen_min_conf: f32,
    /// When true, a `speech` event with sentiment='negative' is `warning` instead of `info`.
    pub negative_sentiment_warns: bool,
    /// Severity for anonymous-person sightings (operators can downgrade to `info`).
    pub unknown_person_severity: String,
}

/// The coarse session bucket index for a start time. Re-sightings in the same bucket share a
/// dedup_key → one coalesced event. `div_euclid` so negative (pre-1970, never in practice) is sane.
fn bucket(start_unix_nanos: i64, width_secs: i64) -> i64 {
    let w = width_secs.max(1).saturating_mul(1_000_000_000);
    start_unix_nanos.div_euclid(w)
}

/// Emit a batch: UPSERT each event, then (if enabled) evaluate alert rules against the resulting
/// event_id. Per-event failures are logged and skipped — never propagated — so one bad event can't
/// drop the rest or fail the segment.
async fn emit_all(pool: &PgPool, evs: Vec<NewEvent>, cfg: &EventsConfig) {
    for ev in &evs {
        match events::record_event(pool, ev).await {
            Ok(event_id) => {
                hushai_backend::observe::counter(
                    "hushai_events_produced_total",
                    &[("type", &ev.event_type)],
                );
                if cfg.alerts_enabled {
                    match crate::alerts::evaluate(pool, event_id).await {
                        Ok(n) if n > 0 => {
                            hushai_backend::observe::counter_by("hushai_alerts_fired_total", &[], n);
                            tracing::debug!(%event_id, deliveries = n, event_type = %ev.event_type, "alerts fired");
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, %event_id, "alert evaluation failed"),
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, event_type = %ev.event_type, "record_event failed"),
        }
    }
}

/// Batched `id -> display_name` lookup for a catalog table. The (table,id_col) pair comes only from
/// our own call sites (never user input); the match keeps the SQL a `&'static str` (sqlx 0.9 forbids
/// `&String` in `query`). Returns a map; a missing id simply won't be present.
async fn load_names(
    pool: &PgPool,
    table: &str,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Option<String>>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql: &'static str = match table {
        "persons" => "SELECT person_id AS id, display_name FROM persons WHERE person_id = ANY($1)",
        "license_plates" => {
            "SELECT plate_id AS id, display_name FROM license_plates WHERE plate_id = ANY($1)"
        }
        "speakers" => {
            "SELECT speaker_id AS id, display_name FROM speakers WHERE speaker_id = ANY($1)"
        }
        _ => return Ok(HashMap::new()),
    };
    let rows = sqlx::query(sql).bind(ids).fetch_all(pool).await?;
    let mut m = HashMap::with_capacity(rows.len());
    for r in &rows {
        m.insert(r.get::<Uuid, _>("id"), r.get::<Option<String>, _>("display_name"));
    }
    Ok(m)
}

/// `load_names` that degrades to an empty map on error (logged) instead of aborting the whole
/// segment's event batch — a name-lookup hiccup must not drop unrelated `object_seen`/`speech`
/// events. A missing name just means the subject is treated as unnamed (→ unknown_person), which is
/// the safe direction (alert as a stranger rather than not at all).
async fn names_or_empty(pool: &PgPool, table: &str, ids: &[Uuid]) -> HashMap<Uuid, Option<String>> {
    match load_names(pool, table, ids).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, table, "event producer: name lookup failed; treating subjects as unnamed");
            HashMap::new()
        }
    }
}

/// Per-subject rolling aggregate while coalescing detections into one event.
struct Agg {
    start: i64,
    end: i64,
    score: f32,
    count: i64,
}
impl Agg {
    fn seed(start: i64, end: i64, score: f32) -> Self {
        Agg { start, end, score, count: 1 }
    }
    fn fold(&mut self, start: i64, end: i64, score: f32) {
        self.start = self.start.min(start);
        self.end = self.end.max(end);
        self.score = self.score.max(score);
        self.count += 1;
    }
}

/// Derive events from a VISION segment's just-committed detections. `assigned`/`plates_assigned`
/// are positionally aligned to `face_writes`/`plate_writes` (per the face/plate matchers).
#[allow(clippy::too_many_arguments)]
pub async fn derive_vision_events(
    pool: &PgPool,
    seg: &SegmentRow,
    segment_id: Uuid,
    face_writes: &[FaceWrite],
    assigned: &[Option<Uuid>],
    object_writes: &[ObjectWrite],
    plate_writes: &[PlateWrite],
    plates_assigned: &[Option<Uuid>],
    cfg: &EventsConfig,
) -> Result<(), sqlx::Error> {
    if !cfg.enabled {
        return Ok(());
    }
    let device_id = &seg.device_id;
    let mut evs: Vec<NewEvent> = Vec::new();

    // ---- faces: one event per distinct attributed person; known vs unknown by display_name ----
    let person_ids: Vec<Uuid> = assigned
        .iter()
        .flatten()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if !person_ids.is_empty() {
        let names = names_or_empty(pool, "persons", &person_ids).await;
        let mut agg: HashMap<Uuid, (Agg, Option<[f32; 4]>)> = HashMap::new();
        for (fw, a) in face_writes.iter().zip(assigned.iter()) {
            if let Some(pid) = a {
                agg.entry(*pid)
                    .and_modify(|(g, _)| g.fold(fw.start_unix_nanos, fw.end_unix_nanos, fw.det_score))
                    .or_insert_with(|| {
                        (Agg::seed(fw.start_unix_nanos, fw.end_unix_nanos, fw.det_score), Some(fw.bbox))
                    });
            }
        }
        for (pid, (g, bbox)) in agg {
            let name = names.get(&pid).cloned().flatten();
            let (event_type, severity) = match &name {
                Some(_) => ("known_person", "info".to_string()),
                None => ("unknown_person", cfg.unknown_person_severity.clone()),
            };
            evs.push(NewEvent {
                device_id: Some(device_id.clone()),
                event_type: event_type.to_string(),
                severity,
                subject_type: Some("person".to_string()),
                subject_id: Some(pid),
                subject_label: name,
                segment_id: Some(segment_id),
                start_unix_nanos: g.start,
                end_unix_nanos: g.end,
                score: Some(g.score),
                metadata: json!({ "faces": g.count, "bbox": bbox }),
                dedup_key: Some(format!(
                    "seen:person:{pid}:{device_id}:{}",
                    bucket(g.start, cfg.session_bucket_secs)
                )),
            });
        }
    }

    // ---- plates: attributed (by plate_id) → of-interest/seen; uncatalogued reads → plate_seen ----
    let plate_ids: Vec<Uuid> = plates_assigned
        .iter()
        .flatten()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let plate_names = names_or_empty(pool, "license_plates", &plate_ids).await;
    let mut plate_agg: HashMap<Uuid, (Agg, String)> = HashMap::new();
    for (pw, a) in plate_writes.iter().zip(plates_assigned.iter()) {
        match a {
            Some(pid) => {
                plate_agg
                    .entry(*pid)
                    .and_modify(|(g, _)| g.fold(pw.start_unix_nanos, pw.end_unix_nanos, pw.det_score))
                    .or_insert_with(|| {
                        (Agg::seed(pw.start_unix_nanos, pw.end_unix_nanos, pw.det_score), pw.ocr_text.clone())
                    });
            }
            None => {
                // Read below the catalog gate: emit a subject-less plate_seen if confident enough.
                if pw.mean_conf >= cfg.plate_seen_min_conf {
                    evs.push(NewEvent {
                        device_id: Some(device_id.clone()),
                        event_type: "plate_seen".to_string(),
                        severity: "info".to_string(),
                        subject_type: Some("plate".to_string()),
                        subject_id: None,
                        subject_label: Some(pw.ocr_text.clone()),
                        segment_id: Some(segment_id),
                        start_unix_nanos: pw.start_unix_nanos,
                        end_unix_nanos: pw.end_unix_nanos,
                        score: Some(pw.det_score),
                        metadata: json!({ "plate_text": pw.ocr_text, "uncatalogued": true }),
                        dedup_key: Some(format!(
                            "platetext:{}:{device_id}:{}",
                            pw.ocr_text_norm,
                            bucket(pw.start_unix_nanos, cfg.session_bucket_secs)
                        )),
                    });
                }
            }
        }
    }
    for (pid, (g, ocr)) in plate_agg {
        let name = plate_names.get(&pid).cloned().flatten();
        let (event_type, severity, label) = match &name {
            Some(n) => ("plate_of_interest", "warning".to_string(), Some(n.clone())),
            None => ("plate_seen", "info".to_string(), Some(ocr.clone())),
        };
        evs.push(NewEvent {
            device_id: Some(device_id.clone()),
            event_type: event_type.to_string(),
            severity,
            subject_type: Some("plate".to_string()),
            subject_id: Some(pid),
            subject_label: label,
            segment_id: Some(segment_id),
            start_unix_nanos: g.start,
            end_unix_nanos: g.end,
            score: Some(g.score),
            metadata: json!({ "plate_text": ocr }),
            dedup_key: Some(format!(
                "plate:{pid}:{device_id}:{}",
                bucket(g.start, cfg.session_bucket_secs)
            )),
        });
    }

    // ---- objects: one event per distinct label (excludes whole-frame + optionally 'person') ----
    let mut obj_agg: HashMap<String, Agg> = HashMap::new();
    for o in object_writes {
        if o.object_label == "__frame__" {
            continue;
        }
        if cfg.object_suppress_person && o.object_label == "person" {
            continue;
        }
        let score = o.det_score.unwrap_or(0.0);
        if score < cfg.object_min_score {
            continue;
        }
        obj_agg
            .entry(o.object_label.clone())
            .and_modify(|g| g.fold(o.start_unix_nanos, o.end_unix_nanos, score))
            .or_insert_with(|| Agg::seed(o.start_unix_nanos, o.end_unix_nanos, score));
    }
    for (label, g) in obj_agg {
        evs.push(NewEvent {
            device_id: Some(device_id.clone()),
            event_type: "object_seen".to_string(),
            severity: "info".to_string(),
            subject_type: Some("object".to_string()),
            subject_id: None,
            subject_label: Some(label.clone()),
            segment_id: Some(segment_id),
            start_unix_nanos: g.start,
            end_unix_nanos: g.end,
            score: Some(g.score),
            metadata: json!({ "count": g.count }),
            dedup_key: Some(format!(
                "object:{label}:{device_id}:{}",
                bucket(g.start, cfg.session_bucket_secs)
            )),
        });
    }

    emit_all(pool, evs, cfg).await;
    Ok(())
}

/// Derive a `speech` event from an AUDIO segment's just-committed transcript. One event per
/// segment's speech, attributed to the resolved speaker when known. No event for a silent /
/// non-speech segment (sentences empty).
pub async fn derive_audio_events(
    pool: &PgPool,
    seg: &SegmentRow,
    segment_id: Uuid,
    sentences: &[Sentence],
    sentiment: &Option<String>,
    speaker_id: Option<Uuid>,
    cfg: &EventsConfig,
) -> Result<(), sqlx::Error> {
    if !cfg.enabled || sentences.is_empty() {
        return Ok(());
    }
    let device_id = &seg.device_id;
    let start = sentences
        .first()
        .map(|s| s.start_unix_nanos)
        .unwrap_or(seg.capture_start_unix_nanos);
    let end = sentences
        .last()
        .map(|s| s.end_unix_nanos)
        .unwrap_or(seg.capture_start_unix_nanos + seg.duration_nanos);

    let label = match speaker_id {
        Some(sid) => names_or_empty(pool, "speakers", &[sid]).await.get(&sid).cloned().flatten(),
        None => None,
    };
    let negative = sentiment.as_deref() == Some("negative");
    let severity = if negative && cfg.negative_sentiment_warns { "warning" } else { "info" };
    let who = speaker_id.map(|s| s.to_string()).unwrap_or_else(|| "anon".to_string());

    let ev = NewEvent {
        device_id: Some(device_id.clone()),
        event_type: "speech".to_string(),
        severity: severity.to_string(),
        subject_type: Some("speaker".to_string()),
        subject_id: speaker_id,
        subject_label: label,
        segment_id: Some(segment_id),
        start_unix_nanos: start,
        end_unix_nanos: end,
        score: None,
        metadata: json!({ "sentences": sentences.len(), "sentiment": sentiment }),
        dedup_key: Some(format!(
            "speech:{who}:{device_id}:{}",
            bucket(start, cfg.session_bucket_secs)
        )),
    };
    emit_all(pool, vec![ev], cfg).await;
    Ok(())
}
