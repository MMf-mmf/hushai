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

    Ok(o)
}
