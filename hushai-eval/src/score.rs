//! Per-modality scorers (Invariant 6: assignment-invariant, structure-not-prose, tolerant).
//!
//! Each scorer turns observed pipeline output + ground truth into a list of `Metric`s. A metric
//! carries a direction (so the baseline layer knows which way is "better"), an absolute-floor pass
//! flag (from expected.json), and a human detail string. Identity metrics never key on minted
//! UUIDs — they use optimal label assignment + denormalized display names.

use crate::fixtures::*;
use crate::query::*;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Direction {
    HigherBetter,
    LowerBetter,
    Boolean,
    Info,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Metric {
    pub key: String,
    pub value: f64,
    pub direction: Direction,
    pub floor_ok: bool,
    pub detail: String,
}

impl Metric {
    fn new(key: impl Into<String>, value: f64, direction: Direction, floor_ok: bool, detail: impl Into<String>) -> Self {
        Self { key: key.into(), value, direction, floor_ok, detail: detail.into() }
    }
    fn info(key: impl Into<String>, value: f64, detail: impl Into<String>) -> Self {
        Self::new(key, value, Direction::Info, true, detail)
    }
}

/// Run a scorer only when BOTH its ground-truth block is present AND the modality is listed in
/// `meta.modalities`. This lets a fixture keep ground truth for a modality that isn't being scored
/// yet (e.g. diarization while the speaker lane is under investigation) without gating the verdict.
pub fn score_all(expected: &Expected, obs: &Observed, base_ns: i64, modalities: &[String]) -> Vec<Metric> {
    let on = |m: &str| modalities.iter().any(|x| x == m);
    let mut m = Vec::new();
    if on("transcript") {
        if let Some(gt) = &expected.transcript {
            m.extend(score_transcript(gt, obs, base_ns));
        }
    }
    if on("speakers") {
        if let Some(gt) = &expected.speakers {
            m.extend(score_speakers(gt, obs, base_ns));
        }
    }
    if on("sentiment") {
        if let Some(gt) = &expected.sentiment {
            m.extend(score_sentiment(gt, obs, base_ns));
        }
    }
    if on("persons") || on("faces") {
        if let Some(gt) = &expected.persons {
            m.extend(score_persons(gt, obs));
        }
    }
    if on("objects") {
        if let Some(gt) = &expected.objects {
            m.extend(score_objects(gt, obs, base_ns));
        }
    }
    if on("plates") {
        if let Some(gt) = &expected.plates {
            m.extend(score_plates(gt, obs));
        }
    }
    if on("events") {
        if let Some(gt) = &expected.events {
            m.extend(score_events(gt, obs));
        }
    }
    m
}

// ----- transcript ------------------------------------------------------------

fn score_transcript(gt: &TranscriptGt, obs: &Observed, base_ns: i64) -> Vec<Metric> {
    let hyp_full = obs.sentences.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ");
    let refn = normalize(&gt.full_text);
    let hypn = normalize(&hyp_full);
    let ref_words: Vec<&str> = refn.split_whitespace().collect();
    let hyp_words: Vec<&str> = hypn.split_whitespace().collect();
    let dist = word_levenshtein(&ref_words, &hyp_words);
    let wer = dist as f64 / ref_words.len().max(1) as f64;
    let sim = strsim::normalized_levenshtein(&refn, &hypn);

    let mut out = vec![
        Metric::new("transcript.wer", wer, Direction::LowerBetter, wer <= gt.max_wer,
            format!("WER {wer:.3} (<= {:.3}); ref {} words, hyp {} words", gt.max_wer, ref_words.len(), hyp_words.len())),
        Metric::new("transcript.similarity", sim, Direction::HigherBetter, sim >= gt.min_similarity,
            format!("normalized similarity {sim:.3} (>= {:.3})", gt.min_similarity)),
    ];

    if !gt.windows.is_empty() {
        let mut total = 0usize;
        let mut found = 0usize;
        for w in &gt.windows {
            let (a, b) = (base_ns + w.start_ns, base_ns + w.end_ns);
            let win_text = normalize(
                &obs.sentences
                    .iter()
                    .filter(|s| overlaps(s.start_ns, s.end_ns, a, b))
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            for phrase in &w.contains {
                total += 1;
                if win_text.contains(&normalize(phrase)) {
                    found += 1;
                }
            }
        }
        if total > 0 {
            let frac = found as f64 / total as f64;
            out.push(Metric::new("transcript.window_contains", frac, Direction::HigherBetter, frac >= 1.0,
                format!("{found}/{total} window phrases present")));
        }
    }
    out
}

// ----- speakers (diarization) ------------------------------------------------

fn score_speakers(gt: &SpeakersGt, obs: &Observed, base_ns: i64) -> Vec<Metric> {
    let distinct: HashSet<Uuid> = obs.sentences.iter().filter_map(|s| s.speaker_id).collect();
    let observed_count = distinct.len() as i64;
    let count_err = (observed_count - gt.distinct_count).abs();

    let mut out = vec![
        Metric::new("speakers.count_error", count_err as f64, Direction::LowerBetter,
            count_err <= gt.count_tolerance,
            format!("{observed_count} distinct speakers (expected {} ±{})", gt.distinct_count, gt.count_tolerance)),
        Metric::info("speakers.distinct_count", observed_count as f64, "distinct minted speakers in window"),
    ];

    if gt.utterances.is_empty() {
        return out;
    }

    // Dominant observed speaker per gt utterance (by overlap + text_contains).
    let mut labels: Vec<String> = Vec::new();
    let mut utt_obs: Vec<(usize, Option<Uuid>)> = Vec::new(); // (label_idx, dominant obs)
    for u in &gt.utterances {
        let li = match labels.iter().position(|l| l == &u.label) {
            Some(i) => i,
            None => {
                labels.push(u.label.clone());
                labels.len() - 1
            }
        };
        let (a, b) = (base_ns + u.window_ns[0], base_ns + u.window_ns[1]);
        let needle = normalize(&u.text_contains);
        let mut counts: HashMap<Uuid, usize> = HashMap::new();
        for s in &obs.sentences {
            if overlaps(s.start_ns, s.end_ns, a, b) && (needle.is_empty() || normalize(&s.text).contains(&needle)) {
                if let Some(sid) = s.speaker_id {
                    *counts.entry(sid).or_default() += 1;
                }
            }
        }
        let dom = counts.into_iter().max_by_key(|(_, c)| *c).map(|(id, _)| id);
        utt_obs.push((li, dom));
    }

    // Contingency[label_idx][obs_id] = number of utterances.
    let obs_ids: Vec<Uuid> = utt_obs.iter().filter_map(|(_, o)| *o).collect::<HashSet<_>>().into_iter().collect();
    let mut cont = vec![HashMap::<Uuid, usize>::new(); labels.len()];
    for (li, o) in &utt_obs {
        if let Some(id) = o {
            *cont[*li].entry(*id).or_default() += 1;
        }
    }
    let (assignment, matched) = best_assignment(&labels, &obs_ids, &cont);
    let purity = matched as f64 / gt.utterances.len() as f64;
    out.push(Metric::new("speakers.purity", purity, Direction::HigherBetter, purity >= gt.min_purity,
        format!("{matched}/{} utterances on the optimally-mapped speaker", gt.utterances.len())));

    for n in &gt.named {
        let key = format!("speakers.named.{}", n.expect_display_name);
        let li = labels.iter().position(|l| l == &n.label);
        let matched_name = li
            .and_then(|li| assignment.get(&li).copied())
            .and_then(|oid| obs.speaker_names.get(&oid).cloned().flatten());
        let ok = matched_name.as_deref() == Some(n.expect_display_name.as_str());
        out.push(Metric::new(key, if ok { 1.0 } else { 0.0 }, Direction::Boolean, ok,
            format!("label {} -> {:?} (expected {})", n.label, matched_name, n.expect_display_name)));
    }
    out
}

/// Brute-force optimal injective label->obs assignment maximizing matched utterances.
/// Sizes are tiny (a few speakers), so permutations are fine and avoid a solver dependency.
fn best_assignment(
    labels: &[String],
    obs_ids: &[Uuid],
    cont: &[HashMap<Uuid, usize>],
) -> (HashMap<usize, Uuid>, usize) {
    let mut best: (HashMap<usize, Uuid>, usize) = (HashMap::new(), 0);
    let mut chosen: HashMap<usize, Uuid> = HashMap::new();
    let mut used = vec![false; obs_ids.len()];

    fn search(
        li: usize,
        labels_len: usize,
        obs_ids: &[Uuid],
        cont: &[HashMap<Uuid, usize>],
        used: &mut Vec<bool>,
        chosen: &mut HashMap<usize, Uuid>,
        acc: usize,
        best: &mut (HashMap<usize, Uuid>, usize),
    ) {
        if li == labels_len {
            if acc > best.1 {
                *best = (chosen.clone(), acc);
            }
            return;
        }
        // Option: leave this label unmapped (when fewer obs than labels).
        search(li + 1, labels_len, obs_ids, cont, used, chosen, acc, best);
        for (oi, oid) in obs_ids.iter().enumerate() {
            if used[oi] {
                continue;
            }
            let gain = *cont[li].get(oid).unwrap_or(&0);
            used[oi] = true;
            chosen.insert(li, *oid);
            search(li + 1, labels_len, obs_ids, cont, used, chosen, acc + gain, best);
            chosen.remove(&li);
            used[oi] = false;
        }
    }
    search(0, labels.len(), obs_ids, cont, &mut used, &mut chosen, 0, &mut best);
    best
}

// ----- sentiment -------------------------------------------------------------

fn score_sentiment(gt: &SentimentGt, obs: &Observed, base_ns: i64) -> Vec<Metric> {
    let mut correct = 0usize;
    let mut scored = 0usize;
    let mut abstained = 0usize;
    for w in &gt.windows {
        let (a, b) = (base_ns + w.start_ns, base_ns + w.end_ns);
        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut any = false;
        for s in &obs.sentences {
            if overlaps(s.start_ns, s.end_ns, a, b) {
                any = true;
                if let Some(lab) = &s.sentiment {
                    *counts.entry(lab.clone()).or_default() += 1;
                }
            }
        }
        let observed = counts.into_iter().max_by_key(|(_, c)| *c).map(|(l, _)| l);
        match observed {
            None if gt.allow_null || !any => abstained += 1,
            None => {
                scored += 1; // a window with speech but no sentiment, and abstain disallowed: wrong
            }
            Some(lab) => {
                scored += 1;
                let allow = if w.allow.is_empty() { std::slice::from_ref(&w.label) } else { &w.allow[..] };
                if allow.iter().any(|x| x == &lab) {
                    correct += 1;
                }
            }
        }
    }
    let acc = if scored == 0 { 1.0 } else { correct as f64 / scored as f64 };
    vec![
        Metric::new("sentiment.accuracy", acc, Direction::HigherBetter, acc >= gt.min_accuracy,
            format!("{correct}/{scored} windows in allow-set ({abstained} abstained)")),
    ]
}

// ----- persons ---------------------------------------------------------------

fn score_persons(gt: &PersonsGt, obs: &Observed) -> Vec<Metric> {
    let count_err = (obs.distinct_persons - gt.distinct_count).abs();
    let mut out = vec![
        Metric::new("persons.count_error", count_err as f64, Direction::LowerBetter,
            count_err <= gt.count_tolerance,
            format!("{} distinct persons (expected {} ±{})", obs.distinct_persons, gt.distinct_count, gt.count_tolerance)),
        Metric::info("persons.distinct_count", obs.distinct_persons as f64, "distinct minted persons in window"),
    ];
    for n in &gt.named {
        let ok = obs.persons_named.iter().any(|(name, samples)| name == &n.expect_display_name && *samples >= n.min_sightings);
        out.push(Metric::new(format!("persons.named.{}", n.expect_display_name), if ok { 1.0 } else { 0.0 },
            Direction::Boolean, ok, format!("named person {} present with >= {} samples", n.expect_display_name, n.min_sightings)));
    }
    out
}

// ----- objects ---------------------------------------------------------------

fn score_objects(gt: &ObjectsGt, obs: &Observed, base_ns: i64) -> Vec<Metric> {
    let mut f1_sum = 0.0;
    let mut n = 0usize;
    for w in &gt.windows {
        let (a, b) = (base_ns + w.start_ns, base_ns + w.end_ns);
        let observed: HashSet<String> = obs
            .objects
            .iter()
            .filter(|o| overlaps(o.start_ns, o.end_ns, a, b))
            .map(|o| o.label.to_lowercase())
            .collect();
        let expected: HashSet<String> = w.labels.iter().map(|l| l.to_lowercase()).collect();
        let tp = expected.intersection(&observed).count() as f64;
        let prec = if observed.is_empty() { if expected.is_empty() { 1.0 } else { 0.0 } } else { tp / observed.len() as f64 };
        let rec = if expected.is_empty() { 1.0 } else { tp / expected.len() as f64 };
        let f1 = if prec + rec == 0.0 { 0.0 } else { 2.0 * prec * rec / (prec + rec) };
        f1_sum += f1;
        n += 1;
    }
    let f1 = if n == 0 { 1.0 } else { f1_sum / n as f64 };
    vec![Metric::new("objects.label_f1", f1, Direction::HigherBetter, f1 >= gt.min_label_f1,
        format!("macro label-set F1 {f1:.3} over {n} windows"))]
}

// ----- plates ----------------------------------------------------------------

fn score_plates(gt: &PlatesGt, obs: &Observed) -> Vec<Metric> {
    let mut found = 0usize;
    let mut out = Vec::new();
    for p in &gt.expected {
        let want_norm = p.text_norm.clone().unwrap_or_else(|| normalize_plate(&p.text));
        let matched = obs.plates.iter().find(|c| {
            (gt.require_exact && c.plate_text == p.text)
                || c.plate_text_norm == want_norm
                || strsim::levenshtein(&c.plate_text_norm, &want_norm) as i64 <= gt.max_norm_edit_distance
        });
        let reads = matched.map(|c| *obs.plate_reads.get(&c.plate_text_norm).unwrap_or(&0)).unwrap_or(0);
        let ok = matched.is_some() && reads >= p.min_reads;
        if ok {
            found += 1;
        }
        if let Some(name) = &p.expect_display_name {
            let nok = matched.and_then(|c| c.display_name.as_deref()) == Some(name.as_str());
            out.push(Metric::new(format!("plates.named.{}", p.text), if nok { 1.0 } else { 0.0 }, Direction::Boolean, nok,
                format!("plate {} display_name expected {}", p.text, name)));
        }
    }
    let recall = if gt.expected.is_empty() { 1.0 } else { found as f64 / gt.expected.len() as f64 };
    out.insert(0, Metric::new("plates.recall", recall, Direction::HigherBetter, recall >= 1.0,
        format!("{found}/{} expected plates read", gt.expected.len())));
    out
}

// ----- events ----------------------------------------------------------------

fn score_events(gt: &EventsGt, obs: &Observed) -> Vec<Metric> {
    let mut satisfied = 0usize;
    for e in &gt.expected {
        let floor = severity_rank(&e.min_severity);
        let count = obs
            .events
            .iter()
            .filter(|o| {
                o.event_type == e.event_type
                    && e.subject_type.as_ref().is_none_or(|t| o.subject_type.as_deref() == Some(t.as_str()))
                    && e.subject_label.as_ref().is_none_or(|l| o.subject_label.as_deref() == Some(l.as_str()))
                    && severity_rank(&o.severity) >= floor
            })
            .count() as i64;
        let ok = count >= e.min_count && e.max_count.is_none_or(|mx| count <= mx);
        if ok {
            satisfied += 1;
        }
    }
    let frac = if gt.expected.is_empty() { 1.0 } else { satisfied as f64 / gt.expected.len() as f64 };
    vec![Metric::new("events.match", frac, Direction::HigherBetter, frac >= 1.0,
        format!("{satisfied}/{} expected events matched", gt.expected.len()))]
}

// ----- helpers ---------------------------------------------------------------

fn overlaps(a0: i64, a1: i64, b0: i64, b1: i64) -> bool {
    a0 < b1 && a1 > b0
}

/// Word-level Levenshtein (edit distance) over token slices — the numerator of WER.
fn word_levenshtein(a: &[&str], b: &[&str]) -> usize {
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim().to_string()
}

fn normalize_plate(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_uppercase()).collect()
}

fn severity_rank(s: &str) -> i32 {
    match s {
        "critical" => 2,
        "warning" => 1,
        _ => 0,
    }
}
