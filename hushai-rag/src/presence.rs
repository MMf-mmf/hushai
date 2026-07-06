//! Deterministic presence aggregation (flaw F1) — the counting/timing/rhythm engine for
//! private-investigator questions ("how many times has X been here", "when did they first/last
//! show up", "what times do they usually come", "which came most often").
//!
//! WHY: the People/Objects/Plates agents return raw per-sighting rows (`retrieve::list_by_*`),
//! CAPPED at `top_k`, and leave the arithmetic to a small LLM — which miscounts and can't see past
//! the cap. This module computes the numbers in SQL/Rust so the model only NARRATES a figure it is
//! handed. It mirrors `analytics.rs` (the reflection digest) — same tz bucketing (`bucket`) and
//! peak-picking (`top_indices`) — but keyed on a person/plate id or an object label instead of a
//! speaker, and over ALL matching sightings (no cap).
//!
//! COUNTS ARE VISITS, NOT SEGMENTS: the pipeline re-detects a continuously-present subject in
//! every ~2s transport segment, so raw per-(subject, segment) rows are an artifact of clip
//! segmentation — a ten-minute visit is ~300 of them. `coalesce_visits` merges sighting times
//! whose gaps are within `gap_nanos` (config `PRESENCE_VISIT_GAP_SECS`) into continuous
//! appearances, so "you saw Bob 2 times" means two VISITS ("from 8:19 PM to 8:29 PM", then
//! later), never 62 re-detections. The citation list (`retrieve::list_by_*`) still shows
//! per-segment rows (each is a video deep-link), so the visit count is intentionally NOT the
//! citation count.

use crate::analytics::{bucket, top_indices};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};

/// Merge ascending-or-not sighting timestamps into continuous-visit intervals: consecutive
/// times closer than `gap_nanos` extend the current visit; a larger gap starts a new one.
/// Returns `[(first_sighting, last_sighting)]` per visit, chronological. A single detection
/// yields a zero-length interval.
pub fn coalesce_visits(times: &[i64], gap_nanos: i64) -> Vec<(i64, i64)> {
    let mut sorted: Vec<i64> = times.to_vec();
    sorted.sort_unstable();
    let mut visits: Vec<(i64, i64)> = Vec::new();
    for t in sorted {
        match visits.last_mut() {
            Some((_, end)) if t - *end <= gap_nanos.max(0) => *end = t,
            _ => visits.push((t, t)),
        }
    }
    visits
}

/// A deterministic rollup of one subject's sightings over a window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PresenceSummary {
    /// Continuous VISITS (gap-coalesced appearances) — what a person means by "seen N times".
    pub count: i64,
    /// Raw per-(subject, segment) detections behind those visits (debug/interest only).
    pub raw_sightings: i64,
    /// The coalesced `[start, end]` interval of each visit, chronological.
    pub visits: Vec<(i64, i64)>,
    /// Start of the first visit / start of the most recent visit.
    pub first_ns: Option<i64>,
    pub last_ns: Option<i64>,
    /// Local-civil-time histograms (tz offset applied), for "what times / days does X come".
    /// Bucketed on visit STARTS — arrivals, not dwell time.
    pub by_hour: [i64; 24],
    pub by_dow: [i64; 7],
}

impl PresenceSummary {
    /// Fold per-sighting timestamps into the visit rollup. `tz_offset_secs` shifts to local
    /// civil time before hour/day bucketing (identical to the reflection digest); `gap_nanos`
    /// is the visit-coalescing gap (`PRESENCE_VISIT_GAP_SECS`).
    pub fn from_sighting_times(times: &[i64], tz_offset_secs: i64, gap_nanos: i64) -> Self {
        let visits = coalesce_visits(times, gap_nanos);
        let mut s = PresenceSummary {
            count: visits.len() as i64,
            raw_sightings: times.len() as i64,
            first_ns: visits.first().map(|&(a, _)| a),
            last_ns: visits.last().map(|&(a, _)| a),
            ..Default::default()
        };
        for &(start, _) in &visits {
            let (h, dow, _) = bucket(start, tz_offset_secs);
            s.by_hour[h as usize] += 1;
            s.by_dow[dow as usize] += 1;
        }
        s.visits = visits;
        s
    }

    /// Up to two busiest hours-of-day (0..23), descending by count.
    pub fn peak_hours(&self) -> Vec<usize> {
        top_indices(&self.by_hour, 2)
    }
    /// Up to two busiest days-of-week (0=Mon), descending by count.
    pub fn peak_dows(&self) -> Vec<usize> {
        top_indices(&self.by_dow, 2)
    }
}

/// Distinct per-(person, segment) sighting timestamps for the given persons — the SAME dedup
/// `retrieve::list_by_person` applies, so a count built from this equals "every time I saw them".
/// UNCAPPED (unlike the top-k sighting list): this is the whole-corpus count.
pub async fn person_sighting_times(
    pool: &PgPool,
    person_ids: &[String],
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
) -> anyhow::Result<Vec<i64>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT t.start_unix_nanos FROM ( \
           SELECT DISTINCT ON (ps.person_id, ps.segment_id) ps.person_id, ps.segment_id, ps.start_unix_nanos \
           FROM person_segments ps WHERE ps.person_id = ANY(",
    );
    qb.push_bind(person_ids.to_vec())
        .push("::uuid[]) AND ps.start_unix_nanos IS NOT NULL");
    if let Some(d) = device_id {
        qb.push(" AND ps.device_id = ").push_bind(d.to_string());
    }
    if let Some(a) = after {
        qb.push(" AND ps.start_unix_nanos >= ").push_bind(a);
    }
    if let Some(b) = before {
        qb.push(" AND ps.start_unix_nanos < ").push_bind(b);
    }
    qb.push(" ORDER BY ps.person_id, ps.segment_id, ps.start_unix_nanos ASC ) t");
    let rows = qb.build().fetch_all(pool).await?;
    rows.iter().map(|r| r.try_get::<i64, _>("start_unix_nanos").map_err(Into::into)).collect()
}

/// Deterministic presence rollup for one or more person ids.
pub async fn person_presence(
    pool: &PgPool,
    person_ids: &[String],
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
    tz_offset_secs: i64,
    visit_gap_nanos: i64,
) -> anyhow::Result<PresenceSummary> {
    let times = person_sighting_times(pool, person_ids, device_id, after, before).await?;
    Ok(PresenceSummary::from_sighting_times(&times, tz_offset_secs, visit_gap_nanos))
}

/// Distinct per-(plate, segment) sighting timestamps — the dedup `retrieve::list_by_plate` uses.
pub async fn plate_sighting_times(
    pool: &PgPool,
    plate_ids: &[String],
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
) -> anyhow::Result<Vec<i64>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT t.start_unix_nanos FROM ( \
           SELECT DISTINCT ON (pd.plate_id, pd.segment_id) pd.plate_id, pd.segment_id, pd.start_unix_nanos \
           FROM plate_detections pd WHERE pd.plate_id = ANY(",
    );
    qb.push_bind(plate_ids.to_vec())
        .push("::uuid[]) AND pd.start_unix_nanos IS NOT NULL");
    if let Some(d) = device_id {
        qb.push(" AND pd.device_id = ").push_bind(d.to_string());
    }
    if let Some(a) = after {
        qb.push(" AND pd.start_unix_nanos >= ").push_bind(a);
    }
    if let Some(b) = before {
        qb.push(" AND pd.start_unix_nanos < ").push_bind(b);
    }
    qb.push(" ORDER BY pd.plate_id, pd.segment_id, pd.start_unix_nanos ASC ) t");
    let rows = qb.build().fetch_all(pool).await?;
    rows.iter().map(|r| r.try_get::<i64, _>("start_unix_nanos").map_err(Into::into)).collect()
}

/// Deterministic presence rollup for one or more plate ids.
pub async fn plate_presence(
    pool: &PgPool,
    plate_ids: &[String],
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
    tz_offset_secs: i64,
    visit_gap_nanos: i64,
) -> anyhow::Result<PresenceSummary> {
    let times = plate_sighting_times(pool, plate_ids, device_id, after, before).await?;
    Ok(PresenceSummary::from_sighting_times(&times, tz_offset_secs, visit_gap_nanos))
}

/// Distinct per-segment sighting timestamps for an exact object CLASS label (COCO class), the dedup
/// `retrieve::list_by_object_class` uses (one row per segment, `__frame__` excluded upstream).
pub async fn object_sighting_times(
    pool: &PgPool,
    label: &str,
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
) -> anyhow::Result<Vec<i64>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT t.start_unix_nanos FROM ( \
           SELECT DISTINCT ON (so.segment_id) so.segment_id, so.start_unix_nanos \
           FROM scene_objects so WHERE so.object_label = ",
    );
    qb.push_bind(label.to_string())
        .push(" AND so.start_unix_nanos IS NOT NULL");
    if let Some(d) = device_id {
        qb.push(" AND so.device_id = ").push_bind(d.to_string());
    }
    if let Some(a) = after {
        qb.push(" AND so.start_unix_nanos >= ").push_bind(a);
    }
    if let Some(b) = before {
        qb.push(" AND so.start_unix_nanos < ").push_bind(b);
    }
    qb.push(" ORDER BY so.segment_id, so.start_unix_nanos ASC ) t");
    let rows = qb.build().fetch_all(pool).await?;
    rows.iter().map(|r| r.try_get::<i64, _>("start_unix_nanos").map_err(Into::into)).collect()
}

/// Deterministic presence rollup for an object class label.
pub async fn object_presence(
    pool: &PgPool,
    label: &str,
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
    tz_offset_secs: i64,
    visit_gap_nanos: i64,
) -> anyhow::Result<PresenceSummary> {
    let times = object_sighting_times(pool, label, device_id, after, before).await?;
    Ok(PresenceSummary::from_sighting_times(&times, tz_offset_secs, visit_gap_nanos))
}

/// Render a presence rollup into ONE plain-language line for the LLM to narrate verbatim — the model
/// no longer does arithmetic, it just phrases the pre-computed figure. `now_unix_nanos` anchors the
/// relative-time phrasing (matches `humanize.rs`).
pub fn render_presence(
    s: &PresenceSummary,
    subject_label: &str,
    now_unix_nanos: i64,
    tz_offset_secs: i64,
) -> String {
    if s.count == 0 {
        return format!("{subject_label} was not seen in the recordings for this period.");
    }
    let humanize = |t: i64| crate::humanize::humanize_time(t, now_unix_nanos, tz_offset_secs);
    // A visit shorter than a minute reads as a moment ("at 8:19 PM"), not a span.
    const MOMENT_NANOS: i64 = 60_000_000_000;
    let mut out = if s.count == 1 {
        let (start, end) = s.visits[0];
        if end - start < MOMENT_NANOS {
            format!("{subject_label} was seen once in the recordings, {}.", humanize(start))
        } else {
            format!(
                "{subject_label} was seen once in the recordings, from {} until {}.",
                humanize(start),
                humanize(end)
            )
        }
    } else {
        format!("{subject_label} was seen {} times in the recordings.", s.count)
    };
    if s.count > 1 {
        if let (Some(f), Some(l)) = (s.first_ns.map(humanize), s.last_ns.map(humanize)) {
            out.push_str(&format!(" First {f}; most recently {l}."));
        }
    }
    let peak_h = s.peak_hours();
    let peak_d = s.peak_dows();
    if s.count >= 3 && (!peak_h.is_empty() || !peak_d.is_empty()) {
        let mut usually = String::from(" Usually ");
        if let Some(&d) = peak_d.first() {
            usually.push_str(&format!("on {}s ", weekday_name(d)));
        }
        if let Some(&h) = peak_h.first() {
            usually.push_str(&format!("around {}", hour_label(h)));
        }
        out.push_str(usually.trim_end());
        out.push('.');
    }
    out
}

/// Deterministically phrase a co-presence set ("who was I with"). The small LLM sometimes DROPS a
/// person when enumerating a multi-person list; `list_co_occurring_persons` already returns one row
/// per co-present person, so we render the full set ourselves (same "don't let the model enumerate"
/// principle as the counts). `names` should already be the distinct, display-ready labels.
pub fn render_people_list(names: &[String]) -> String {
    match names {
        [] => "I didn't see you with anyone in the recordings.".to_string(),
        [only] => format!("You were with {only}."),
        [rest @ .., last] => format!("You were with {} and {}.", rest.join(", "), last),
    }
}

/// Deterministically answer "how many people did you see": DISTINCT people, named where we can.
/// `labels` is one display label per distinct person (named or the "someone we haven't identified"
/// style), already deduped by the caller.
pub fn render_people_count(labels: &[String]) -> String {
    match labels {
        [] => "I didn't see anyone in the recordings for this period.".to_string(),
        [only] => format!("I saw 1 person in that period: {only}."),
        [rest @ .., last] => format!(
            "I saw {} people in that period: {} and {}.",
            labels.len(),
            rest.join(", "),
            last
        ),
    }
}

fn weekday_name(dow0_mon: usize) -> &'static str {
    ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"]
        .get(dow0_mon)
        .copied()
        .unwrap_or("that day")
}

fn hour_label(h: usize) -> String {
    let (h12, ap) = match h {
        0 => (12, "AM"),
        1..=11 => (h, "AM"),
        12 => (12, "PM"),
        _ => (h - 12, "PM"),
    };
    format!("{h12} {ap}")
}

/// Does the question ask for a COUNT / FREQUENCY / TIMING / RHYTHM rollup (route to presence
/// aggregation) rather than a plain sighting list? Deterministic keyword heuristic — cheap, testable,
/// and no extra LLM round-trip. Conservative: only trips on unambiguous aggregate phrasings.
pub fn is_count_intent(question: &str) -> bool {
    let q = question.to_lowercase();
    const MARKERS: &[&str] = &[
        "how many", "how often", "how frequently", "number of times", "how much",
        "most often", "most frequent", "which .* most", "come the most",
        "what time", "what times", "what days", "which days", "what days of the week",
        "usually come", "usually here", "usually show", "typically come",
        "first see", "first saw", "last see", "last saw", "last time", "first time",
        "how many times", "times has", "times did", "times have",
    ];
    MARKERS.iter().any(|m| {
        if m.contains(".*") {
            // tiny two-part contains (avoid a regex dep): "which" ... "most"
            let parts: Vec<&str> = m.split(".*").collect();
            parts.iter().all(|p| q.contains(p.trim()))
        } else {
            q.contains(m)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000_000_000;
    const SEC: i64 = 1_000_000_000;
    const GAP: i64 = 120 * SEC; // the PRESENCE_VISIT_GAP_SECS default

    #[test]
    fn coalesce_merges_segment_cadence_into_one_visit() {
        // 60 detections 2s apart (the "seen 62 times" bug shape) → ONE visit spanning them.
        let base = 1_781_784_000_000_000_000;
        let times: Vec<i64> = (0..60).map(|i| base + i * 2 * SEC).collect();
        let visits = coalesce_visits(&times, GAP);
        assert_eq!(visits, vec![(base, base + 118 * SEC)]);
    }

    #[test]
    fn coalesce_splits_on_gap_and_sorts() {
        let base = 1_781_784_000_000_000_000;
        // Deliberately unsorted; two clusters a day apart.
        let times = vec![base + DAY, base, base + 2 * SEC, base + DAY + 4 * SEC];
        let visits = coalesce_visits(&times, GAP);
        assert_eq!(visits, vec![(base, base + 2 * SEC), (base + DAY, base + DAY + 4 * SEC)]);
        assert!(coalesce_visits(&[], GAP).is_empty());
    }

    #[test]
    fn summary_counts_visits_not_segments() {
        // Two clusters of segment-cadence detections a day apart → 2 visits, 4 raw sightings.
        let base = 1_781_784_000_000_000_000;
        let times = vec![base, base + 2 * SEC, base + DAY, base + DAY + 2 * SEC];
        let s = PresenceSummary::from_sighting_times(&times, 0, GAP);
        assert_eq!(s.count, 2);
        assert_eq!(s.raw_sightings, 4);
        assert_eq!(s.first_ns, Some(base));
        assert_eq!(s.last_ns, Some(base + DAY));
        // Buckets count ARRIVALS (visit starts), not every detection.
        assert_eq!(s.by_hour.iter().sum::<i64>(), 2);
        assert_eq!(s.by_dow.iter().sum::<i64>(), 2);
    }

    #[test]
    fn empty_summary_is_zero() {
        let s = PresenceSummary::from_sighting_times(&[], 0, GAP);
        assert_eq!(s.count, 0);
        assert_eq!(s.first_ns, None);
        assert!(render_presence(&s, "Alice", 0, 0).contains("not seen"));
    }

    #[test]
    fn render_states_the_visit_count_verbatim() {
        let base = 1_781_784_000_000_000_000;
        let times = vec![base, base + DAY, base + 2 * DAY];
        let s = PresenceSummary::from_sighting_times(&times, 0, GAP);
        let line = render_presence(&s, "Alice", base + 3 * DAY, 0);
        assert!(line.contains("3 times"), "visit count must be stated: {line}");
        assert!(line.contains("First"), "first/last narrated: {line}");
    }

    #[test]
    fn render_single_visit_as_span_or_moment() {
        let base = 1_781_784_000_000_000_000;
        // A 10-minute continuous appearance renders as one visit with a from/until span.
        let times: Vec<i64> = (0..300).map(|i| base + i * 2 * SEC).collect();
        let s = PresenceSummary::from_sighting_times(&times, 0, GAP);
        assert_eq!(s.count, 1);
        let line = render_presence(&s, "Mendel", base + DAY, 0);
        assert!(line.contains("once"), "one visit, not 300 sightings: {line}");
        assert!(line.contains("from") && line.contains("until"), "span narrated: {line}");
        // A single blink-and-gone detection reads as a moment, not a span.
        let s = PresenceSummary::from_sighting_times(&[base], 0, GAP);
        let line = render_presence(&s, "Mendel", base + DAY, 0);
        assert!(line.contains("once") && !line.contains("until"), "moment phrasing: {line}");
    }

    #[test]
    fn people_count_lists_everyone() {
        assert!(render_people_count(&[]).contains("didn't see anyone"));
        let one = render_people_count(&["Mendel".into()]);
        assert!(one.contains("1 person") && one.contains("Mendel"), "{one}");
        let two = render_people_count(&["Mendel".into(), "someone we haven't identified yet".into()]);
        assert!(two.contains("2 people") && two.contains("Mendel") && two.contains("haven't identified"), "{two}");
    }

    #[test]
    fn count_intent_detection() {
        assert!(is_count_intent("How many times has Alice been here?"));
        assert!(is_count_intent("what time does the mail carrier usually come?"));
        assert!(is_count_intent("When did I first see Bob?"));
        assert!(is_count_intent("which plate came the most?"));
        // Plain sighting-list questions are NOT count intent.
        assert!(!is_count_intent("When did I see Bob?"));
        assert!(!is_count_intent("Who was I with yesterday?"));
    }
}
