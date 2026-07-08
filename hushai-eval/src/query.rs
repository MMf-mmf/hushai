//! Query observed pipeline results, scoped to the fixture's device + pinned time window.
//! DB-direct (we already hold the pool) — deterministic and doesn't require the read APIs to be up.

use crate::ctx::Ctx;
use anyhow::Result;
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Sentence {
    pub text: String,
    pub start_ns: i64,
    pub end_ns: i64,
    pub sentiment: Option<String>,
    pub speaker_id: Option<Uuid>,
    /// Threaded conversation (migration 0025). NULL = unthreaded (pre-feature history / the
    /// threader's lagging tail) — the scorer must treat NULL as "no evidence", never an error.
    pub conversation_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct ObjectDet {
    pub label: String,
    pub start_ns: i64,
    pub end_ns: i64,
}

#[derive(Debug, Clone)]
pub struct PlateCat {
    pub plate_text: String,
    pub plate_text_norm: String,
    pub display_name: Option<String>,
    pub n_samples: i64,
}

#[derive(Debug, Clone)]
pub struct EventObs {
    pub event_type: String,
    pub severity: String,
    pub subject_type: Option<String>,
    pub subject_label: Option<String>,
    pub start_ns: i64,
}

/// One materialized `entity_edges` row (Gotham G1). Endpoints are `(type, id)` text pairs with no FK
/// (the 0024/0028 contract). `id` is a stringified catalog UUID for person/speaker/plate, or the
/// literal `device_id` for device endpoints.
#[derive(Debug, Clone)]
pub struct EdgeObs {
    pub edge_type: String,
    pub src_type: String,
    pub src_id: String,
    pub dst_type: String,
    pub dst_id: String,
    pub observation_count: i64,
    pub confidence: Option<f32>,
    pub status: Option<String>,
}

/// Name→id resolution for graph assertions (assignment-invariance): the enrolled `display_name` of a
/// person/speaker/plate maps to its catalog id. Device endpoints resolve to their literal id, so they
/// need no map. Empty when the `graph` modality isn't scored.
#[derive(Debug, Clone, Default)]
pub struct EntityIds {
    pub person: HashMap<String, String>, // display_name -> person_id::text
    pub speaker: HashMap<String, String>,
    pub plate: HashMap<String, String>, // display_name OR plate_text_norm -> plate_id::text
}

#[derive(Debug, Clone, Default)]
pub struct Observed {
    pub window: (i64, i64),
    pub sentences: Vec<Sentence>,
    pub speaker_names: HashMap<Uuid, Option<String>>,
    pub distinct_persons: i64,
    pub persons_named: Vec<(String, i64)>, // (display_name, n_samples)
    pub objects: Vec<ObjectDet>,
    pub plates: Vec<PlateCat>,
    pub plate_reads: HashMap<String, i64>, // plate_text_norm -> reads in window
    pub events: Vec<EventObs>,
    /// All materialized graph edges (Gotham G1). Unwindowed: edges are already folded from the
    /// window's events/conversations, and the whole point of the graph is cross-time relationships.
    pub graph_edges: Vec<EdgeObs>,
    /// Enrolled-name → catalog-id resolution for graph assertions.
    pub entity_ids: EntityIds,
}

const SLACK_NS: i64 = 5_000_000_000;

pub async fn observe(
    ctx: &Ctx,
    devices: &[String],
    win_lo: i64,
    win_hi: i64,
    modalities: &[String],
) -> Result<Observed> {
    let lo = win_lo - SLACK_NS;
    let hi = win_hi + SLACK_NS;
    let has = |m: &str| modalities.iter().any(|x| x == m);
    let mut o = Observed { window: (lo, hi), ..Default::default() };

    if has("transcript") || has("speakers") || has("sentiment") || has("conversations") {
        // NB: transcript_sentences.speaker_id is TEXT (a stringified UUID), not a uuid column —
        // read it as String and parse to Uuid to match the speakers catalog (which IS uuid).
        // conversation_id (0025) IS a real uuid column, so it decodes as Option<Uuid> directly.
        let rows: Vec<(String, i64, i64, Option<String>, Option<String>, Option<Uuid>)> = sqlx::query_as(
            "SELECT text, start_unix_nanos, end_unix_nanos, sentiment, speaker_id, conversation_id
             FROM transcript_sentences
             WHERE device_id = ANY($1) AND start_unix_nanos >= $2 AND start_unix_nanos < $3
             ORDER BY start_unix_nanos",
        )
        .bind(devices)
        .bind(lo)
        .bind(hi)
        .fetch_all(&ctx.pool)
        .await?;
        o.sentences = rows
            .into_iter()
            .map(|(text, start_ns, end_ns, sentiment, spk, conversation_id)| Sentence {
                text,
                start_ns,
                end_ns,
                sentiment,
                speaker_id: spk.and_then(|s| Uuid::parse_str(&s).ok()),
                conversation_id,
            })
            .collect();

        if has("speakers") {
            let rows: Vec<(Uuid, Option<String>)> =
                sqlx::query_as("SELECT speaker_id, display_name FROM speakers")
                    .fetch_all(&ctx.pool)
                    .await?;
            o.speaker_names = rows.into_iter().collect();
        }
    }

    if has("persons") || has("faces") {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT count(DISTINCT person_id) FROM person_segments
             WHERE device_id = ANY($1) AND start_unix_nanos >= $2 AND start_unix_nanos < $3
               AND person_id IS NOT NULL",
        )
        .bind(devices)
        .bind(lo)
        .bind(hi)
        .fetch_one(&ctx.pool)
        .await?;
        o.distinct_persons = n;
        // Window+device-scoped named-attribution: count IN-WINDOW sightings on THIS device per named
        // person, NOT the global `persons` catalog. Enrollment mints+names a person on a `-ref` device
        // before the case window, so the old global `SELECT display_name,n_samples FROM persons` passed
        // `persons.named` on enrollment alone — a broken re-identification (person never re-seen in the
        // clip) still scored 1.0. This ties the metric to actual in-window attribution (person_segments).
        o.persons_named = sqlx::query_as(
            "SELECT p.display_name, count(*)::bigint
             FROM person_segments ps
             JOIN persons p ON p.person_id = ps.person_id
             WHERE ps.device_id = ANY($1) AND ps.start_unix_nanos >= $2 AND ps.start_unix_nanos < $3
               AND p.display_name IS NOT NULL
             GROUP BY p.display_name",
        )
        .bind(devices)
        .bind(lo)
        .bind(hi)
        .fetch_all(&ctx.pool)
        .await?
        .into_iter()
        .map(|(name, n): (String, i64)| (name, n))
        .collect();
    }

    if has("objects") {
        let rows: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT object_label, start_unix_nanos, end_unix_nanos FROM scene_objects
             WHERE device_id = ANY($1) AND start_unix_nanos >= $2 AND start_unix_nanos < $3
               AND object_label <> '__frame__'",
        )
        .bind(devices)
        .bind(lo)
        .bind(hi)
        .fetch_all(&ctx.pool)
        .await?;
        o.objects = rows
            .into_iter()
            .map(|(label, start_ns, end_ns)| ObjectDet { label, start_ns, end_ns })
            .collect();
    }

    if has("plates") {
        o.plates = sqlx::query_as(
            "SELECT plate_text, plate_text_norm, display_name, n_samples FROM license_plates",
        )
        .fetch_all(&ctx.pool)
        .await?
        .into_iter()
        .map(|(plate_text, plate_text_norm, display_name, n_samples): (String, String, Option<String>, i64)| {
            PlateCat { plate_text, plate_text_norm, display_name, n_samples }
        })
        .collect();
        let reads: Vec<(String, i64)> = sqlx::query_as(
            "SELECT lp.plate_text_norm, count(*)::bigint
             FROM plate_detections pd JOIN license_plates lp ON pd.plate_id = lp.plate_id
             WHERE pd.device_id = ANY($1) AND pd.start_unix_nanos >= $2 AND pd.start_unix_nanos < $3
             GROUP BY lp.plate_text_norm",
        )
        .bind(devices)
        .bind(lo)
        .bind(hi)
        .fetch_all(&ctx.pool)
        .await?;
        o.plate_reads = reads.into_iter().collect();
    }

    if has("events") {
        let rows: Vec<(String, String, Option<String>, Option<String>, i64)> = sqlx::query_as(
            "SELECT event_type, severity, subject_type, subject_label, start_unix_nanos
             FROM events
             WHERE device_id = ANY($1) AND start_unix_nanos >= $2 AND start_unix_nanos < $3
             ORDER BY start_unix_nanos",
        )
        .bind(devices)
        .bind(lo)
        .bind(hi)
        .fetch_all(&ctx.pool)
        .await?;
        o.events = rows
            .into_iter()
            .map(|(event_type, severity, subject_type, subject_label, start_ns)| EventObs {
                event_type,
                severity,
                subject_type,
                subject_label,
                start_ns,
            })
            .collect();
    }

    if has("graph") {
        // The materialized edges (Gotham G1). Unwindowed — the fold already applied the window's
        // events/conversations, and relationships are inherently cross-time. `src_id`/`dst_id` are
        // TEXT (stringified UUIDs, or a device_id literal), so read them as String.
        o.graph_edges = sqlx::query_as(
            "SELECT edge_type, src_type, src_id, dst_type, dst_id, observation_count, confidence, status
             FROM entity_edges",
        )
        .fetch_all(&ctx.pool)
        .await?
        .into_iter()
        .map(
            |(edge_type, src_type, src_id, dst_type, dst_id, observation_count, confidence, status): (
                String, String, String, String, String, i64, Option<f32>, Option<String>,
            )| EdgeObs {
                edge_type, src_type, src_id, dst_type, dst_id, observation_count, confidence, status
            },
        )
        .collect();

        // Name→id resolution (assignment-invariance): assertions name entities by enrolled
        // display_name; the graph stores catalog ids. Only NAMED rows resolve — an unnamed cluster
        // can't be asserted by name (and shouldn't be: it's the counter-assertion's job).
        for (name, id) in sqlx::query_as::<_, (String, Uuid)>(
            "SELECT display_name, person_id FROM persons WHERE display_name IS NOT NULL",
        )
        .fetch_all(&ctx.pool)
        .await?
        {
            o.entity_ids.person.insert(name, id.to_string());
        }
        for (name, id) in sqlx::query_as::<_, (String, Uuid)>(
            "SELECT display_name, speaker_id FROM speakers WHERE display_name IS NOT NULL",
        )
        .fetch_all(&ctx.pool)
        .await?
        {
            o.entity_ids.speaker.insert(name, id.to_string());
        }
        // Plates resolve by display_name AND by normalized text (a fixture may name a plate by its
        // string when no human label was assigned).
        for (name, norm, id) in sqlx::query_as::<_, (Option<String>, String, Uuid)>(
            "SELECT display_name, plate_text_norm, plate_id FROM license_plates",
        )
        .fetch_all(&ctx.pool)
        .await?
        {
            if let Some(n) = name {
                o.entity_ids.plate.insert(n, id.to_string());
            }
            // The normalized-text key is a FALLBACK: never clobber an explicit display_name binding
            // (guards a future multi-plate fixture where plate A's display_name equals plate B's norm).
            o.entity_ids.plate.entry(norm).or_insert_with(|| id.to_string());
        }
    }

    Ok(o)
}

/// Trigger a single authoritative graph rebuild via the backend admin API (`POST /v1/graph/rebuild`)
/// — the one non-DB call in this module. Needed because `graph_pass` correlates cross-subject edges
/// batch-locally; a whole-scenario rebuild folds every subject in ONE batch so the graph is
/// deterministic (see `poll::wait_graph_inputs_settled`). The eval already authenticates to the
/// backend with `device_token` on every injection (`inject.rs` POSTs `/v1/segments`), so the same
/// bearer works here. Returns `Ok(false)` when the graph surface is unreachable / unauthorized /
/// absent (an old backend without the endpoint) — the caller maps that to INCONCLUSIVE, never a scored
/// failure. `Ok(true)` = rebuilt (the handler folds to convergence synchronously before responding).
pub async fn trigger_graph_rebuild(ctx: &Ctx) -> Result<bool> {
    let url = format!("{}/v1/graph/rebuild", ctx.backend_url.trim_end_matches('/'));
    match ctx.http.post(&url).bearer_auth(&ctx.device_token).send().await {
        Ok(r) if r.status().is_success() => Ok(true),
        Ok(r) => {
            eprintln!("[graph] rebuild endpoint returned {} (backend without the graph API?)", r.status());
            Ok(false)
        }
        Err(e) => {
            eprintln!("[graph] rebuild request failed (backend unreachable?): {e}");
            Ok(false)
        }
    }
}
