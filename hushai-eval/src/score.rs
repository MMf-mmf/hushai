//! Per-modality scorers (Invariant 6: assignment-invariant, structure-not-prose, tolerant).
//!
//! Each scorer turns observed pipeline output + ground truth into a list of `Metric`s. A metric
//! carries a direction (so the baseline layer knows which way is "better"), an absolute-floor pass
//! flag (from expected.json), and a human detail string. Identity metrics never key on minted
//! UUIDs — they use optimal label assignment + denormalized display names.

use crate::ctx::Ctx;
use crate::fixtures::*;
use crate::query::*;
use crate::query_rag::RagAnswer;
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
    if on("conversations") {
        if let Some(gt) = &expected.conversations {
            m.extend(score_conversations(gt, obs, base_ns));
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
    if on("graph") {
        if let Some(gt) = &expected.graph {
            m.extend(score_graph(gt, obs));
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

// ----- conversations (threading) ----------------------------------------------

/// One GT utterance's observed-conversation evidence: matched sentences (window overlap +
/// `text_contains`, the `score_speakers` recipe) and their non-NULL `conversation_id` counts.
struct ConvUttMatch {
    label: String,
    id_counts: HashMap<Uuid, usize>,
    dominant: Option<Uuid>,
}

fn match_conv_utterances(utts: &[ConvUttGt], sentences: &[Sentence], base_ns: i64) -> Vec<ConvUttMatch> {
    utts.iter()
        .map(|u| {
            let (a, b) = (base_ns + u.window_ns[0], base_ns + u.window_ns[1]);
            let needle = normalize(&u.text_contains);
            let mut id_counts: HashMap<Uuid, usize> = HashMap::new();
            for s in sentences {
                if overlaps(s.start_ns, s.end_ns, a, b) && (needle.is_empty() || normalize(&s.text).contains(&needle)) {
                    if let Some(cid) = s.conversation_id {
                        *id_counts.entry(cid).or_default() += 1;
                    }
                }
            }
            let dominant = dominant_conv_id(&id_counts);
            ConvUttMatch { label: u.label.clone(), id_counts, dominant }
        })
        .collect()
}

/// The conversation id with the most matched sentences; ties break to the lexicographically
/// smaller uuid (Uuid byte order == canonical-string order) so the result is deterministic.
fn dominant_conv_id(counts: &HashMap<Uuid, usize>) -> Option<Uuid> {
    let mut v: Vec<(&Uuid, &usize)> = counts.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    v.first().map(|(id, _)| **id)
}

/// Aggregate dominant id for one GT LABEL: sentence counts summed across every utterance carrying
/// the label, then the same deterministic dominant pick. `None` when nothing threaded matched.
fn label_dominant(matches: &[ConvUttMatch], label: &str) -> Option<Uuid> {
    let mut agg: HashMap<Uuid, usize> = HashMap::new();
    for mm in matches.iter().filter(|m| m.label == label) {
        for (id, c) in &mm.id_counts {
            *agg.entry(*id).or_default() += c;
        }
    }
    dominant_conv_id(&agg)
}

/// Pairwise clustering (precision, recall, F1) over GT utterances given each utterance's
/// (label, dominant observed id). NULL dominants are singletons — never "same-observed-id" with
/// anything. No same-GT-label pairs AND no same-observed-id pairs => vacuously perfect (1.0).
fn conv_pairwise_f1(utts: &[(String, Option<Uuid>)]) -> (f64, f64, f64) {
    let mut same_obs = 0usize; // pairs whose dominants are equal and non-NULL
    let mut same_gt = 0usize; // pairs sharing a GT label
    let mut both = 0usize;
    for i in 0..utts.len() {
        for j in (i + 1)..utts.len() {
            let so = matches!((&utts[i].1, &utts[j].1), (Some(a), Some(b)) if a == b);
            let sg = utts[i].0 == utts[j].0;
            if so {
                same_obs += 1;
            }
            if sg {
                same_gt += 1;
            }
            if so && sg {
                both += 1;
            }
        }
    }
    if same_obs == 0 && same_gt == 0 {
        return (1.0, 1.0, 1.0);
    }
    let p = if same_obs == 0 { 1.0 } else { both as f64 / same_obs as f64 };
    let r = if same_gt == 0 { 1.0 } else { both as f64 / same_gt as f64 };
    let f1 = if p + r == 0.0 { 0.0 } else { 2.0 * p * r / (p + r) };
    (p, r, f1)
}

/// Labels joined by `must_merge` pairs describe ONE ground-truth conversation (the labels only
/// exist as handles for the merge assertion), so clustering metrics must not count their
/// cross-label pairs as "should be separate". Maps every utterance label to the
/// lexicographically-smallest label of its `must_merge`-connected component; labels not in any
/// pair map to themselves. `must_not_merge` checks keep the raw labels.
fn canon_conv_labels(gt: &ConversationsGt) -> HashMap<String, String> {
    let mut canon: HashMap<String, String> = HashMap::new();
    for u in &gt.utterances {
        canon.insert(u.label.clone(), u.label.clone());
    }
    // Tiny union-find via path-free root lookup — label sets are single digits in practice.
    fn root(canon: &HashMap<String, String>, mut l: String) -> String {
        while canon.get(&l).is_some_and(|p| p != &l) {
            l = canon[&l].clone();
        }
        l
    }
    for pair in &gt.must_merge {
        let (ra, rb) = (root(&canon, pair[0].clone()), root(&canon, pair[1].clone()));
        if ra != rb {
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            canon.insert(hi, lo);
        }
    }
    let keys: Vec<String> = canon.keys().cloned().collect();
    keys.into_iter().map(|k| (k.clone(), root(&canon, k))).collect()
}

/// Score conversation threading (migration 0025). Assignment-invariant like `score_speakers`:
/// never keys on minted conversation UUIDs — GT labels map to observed ids via per-utterance
/// dominant ids + optimal assignment. NULL conversation_id is "no evidence" (coverage catches a
/// threader that never ran), never a merge/split signal.
fn score_conversations(gt: &ConversationsGt, obs: &Observed, base_ns: i64) -> Vec<Metric> {
    // "In-window" sentences: inside the UNION of GT utterance windows when utterances are given,
    // else every observed sentence in the case window.
    let wins: Vec<(i64, i64)> =
        gt.utterances.iter().map(|u| (base_ns + u.window_ns[0], base_ns + u.window_ns[1])).collect();
    let distinct: HashSet<Uuid> = obs
        .sentences
        .iter()
        .filter(|s| wins.is_empty() || wins.iter().any(|(a, b)| overlaps(s.start_ns, s.end_ns, *a, *b)))
        .filter_map(|s| s.conversation_id)
        .collect();
    let observed_count = distinct.len() as i64;
    let count_err = (observed_count - gt.distinct_count).abs();

    let mut out = vec![Metric::new(
        "conversations.count_error",
        count_err as f64,
        Direction::LowerBetter,
        count_err <= gt.count_tolerance,
        format!("{observed_count} distinct conversations (expected {} ±{})", gt.distinct_count, gt.count_tolerance),
    )];
    let fragmentation = observed_count as f64 / gt.distinct_count.max(1) as f64;
    let frag_detail = format!("{observed_count} observed conversation ids / {} expected", gt.distinct_count.max(1));

    if gt.utterances.is_empty() {
        out.push(Metric::info("conversations.fragmentation", fragmentation, frag_detail));
        return out;
    }

    let matches = match_conv_utterances(&gt.utterances, &obs.sentences, base_ns);

    let covered = matches.iter().filter(|m| !m.id_counts.is_empty()).count();
    let coverage = covered as f64 / matches.len() as f64;
    out.push(Metric::new(
        "conversations.coverage",
        coverage,
        Direction::HigherBetter,
        coverage >= gt.min_coverage,
        format!("{covered}/{} utterances threaded (any non-NULL conversation_id)", matches.len()),
    ));

    // Clustering metrics see must_merge-joined labels as ONE cluster (they are one GT
    // conversation); the merge/no-merge Boolean gates below keep the raw labels.
    let canon = canon_conv_labels(gt);
    let clab = |l: &str| canon.get(l).cloned().unwrap_or_else(|| l.to_string());
    let pairs: Vec<(String, Option<Uuid>)> =
        matches.iter().map(|m| (clab(&m.label), m.dominant)).collect();
    let (prec, rec, f1) = conv_pairwise_f1(&pairs);
    out.push(Metric::new(
        "conversations.pairwise_f1",
        f1,
        Direction::HigherBetter,
        f1 >= gt.min_pairwise_f1,
        format!("pairwise P {prec:.3} R {rec:.3} F1 {f1:.3} over {} utterances", matches.len()),
    ));

    // NULLs never match: only NON-NULL ids can be "shared", so two unthreaded labels don't merge.
    let ids_of = |lab: &str| -> HashSet<Uuid> {
        matches.iter().filter(|m| m.label == lab).flat_map(|m| m.id_counts.keys().copied()).collect()
    };
    for pair in &gt.must_not_merge {
        let (la, lb) = (pair[0].as_str(), pair[1].as_str());
        let (sa, sb) = (ids_of(la), ids_of(lb));
        let mut shared: Vec<Uuid> = sa.intersection(&sb).copied().collect();
        shared.sort();
        let ok = shared.is_empty();
        out.push(Metric::new(
            format!("conversations.must_not_merge.{la}-{lb}"),
            if ok { 1.0 } else { 0.0 },
            Direction::Boolean,
            ok,
            if ok {
                format!("labels {la}/{lb} share no conversation id")
            } else {
                format!("labels {la}/{lb} SHARE conversation id(s) {shared:?}")
            },
        ));
    }
    for pair in &gt.must_merge {
        let (la, lb) = (pair[0].as_str(), pair[1].as_str());
        let (da, db) = (label_dominant(&matches, la), label_dominant(&matches, lb));
        let ok = matches!((da, db), (Some(x), Some(y)) if x == y);
        out.push(Metric::new(
            format!("conversations.must_merge.{la}-{lb}"),
            if ok { 1.0 } else { 0.0 },
            Direction::Boolean,
            ok,
            format!("dominant ids {la}={da:?} {lb}={db:?} (must be equal and non-NULL)"),
        ));
    }

    // purity (Info): best_assignment over (GT label -> dominant conversation id), exactly the
    // speakers.purity recipe. Info-only — pairwise_f1 is the gating clustering metric.
    let mut labels: Vec<String> = Vec::new();
    let mut utt_obs: Vec<(usize, Option<Uuid>)> = Vec::new();
    for mm in &matches {
        let cl = clab(&mm.label);
        let li = match labels.iter().position(|l| l == &cl) {
            Some(i) => i,
            None => {
                labels.push(cl);
                labels.len() - 1
            }
        };
        utt_obs.push((li, mm.dominant));
    }
    let obs_ids: Vec<Uuid> = utt_obs.iter().filter_map(|(_, o)| *o).collect::<HashSet<_>>().into_iter().collect();
    let mut cont = vec![HashMap::<Uuid, usize>::new(); labels.len()];
    for (li, o) in &utt_obs {
        if let Some(id) = o {
            *cont[*li].entry(*id).or_default() += 1;
        }
    }
    let (_assignment, matched_n) = best_assignment(&labels, &obs_ids, &cont);
    let purity = matched_n as f64 / matches.len() as f64;
    out.push(Metric::info(
        "conversations.purity",
        purity,
        format!("{matched_n}/{} utterances on the optimally-mapped conversation", matches.len()),
    ));
    out.push(Metric::info("conversations.fragmentation", fragmentation, frag_detail));
    out
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
            // Gate on actual in-window OCR reads too, not just the seeded catalog row's display_name:
            // enroll_plate INSERTs a named row directly, so without the `reads` gate this passed 1.0
            // even when the pipeline never re-read/attributed the plate in the clip.
            let nok = reads >= p.min_reads.max(1)
                && matched.and_then(|c| c.display_name.as_deref()) == Some(name.as_str());
            out.push(Metric::new(format!("plates.named.{}", p.text), if nok { 1.0 } else { 0.0 }, Direction::Boolean, nok,
                format!("plate {} display_name expected {} (>= {} reads)", p.text, name, p.min_reads.max(1))));
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

// ----- graph (Gotham G1) ------------------------------------------------------

/// Score the materialized entity graph against `GraphGt`. ASSIGNMENT-INVARIANT: assertions name
/// entities by enrolled `display_name` / `device_id`, resolved to catalog ids via `obs.entity_ids` —
/// never a minted UUID (Gotham.md §3.1). Undirected edge types (`co_present`, `conversed_with`,
/// `same_identity_candidate`) match in EITHER endpoint order. `expect_no_edge` is threshold-aware:
/// it passes when no edge reaches `min_evidence` (a below-bar co-sighting is "did not bind").
fn score_graph(gt: &GraphGt, obs: &Observed) -> Vec<Metric> {
    let mut out = Vec::new();
    let mut satisfied = 0usize;
    let total = gt.entities.len() + gt.edges.len() + gt.no_edges.len()
        + gt.anomalies.len() + gt.no_anomalies.len() + gt.baselines.len();

    for e in &gt.entities {
        let ok = resolve_entity(e, &obs.entity_ids).is_some();
        if ok {
            satisfied += 1;
        }
        out.push(Metric::new(
            format!("graph.entity.{}.{}", e.kind, key_name(&e.name)),
            b2f(ok), Direction::Boolean, ok,
            format!("entity {}:{} resolved={ok}", e.kind, e.name),
        ));
    }

    for e in &gt.edges {
        let m = match_edge(e, obs);
        let ok = m.is_some_and(|edge| {
            edge.observation_count >= e.min_evidence
                && e.status.as_ref().is_none_or(|s| edge.status.as_deref() == Some(s.as_str()))
        });
        if ok {
            satisfied += 1;
        }
        let obsn = m.map(|edge| edge.observation_count).unwrap_or(0);
        let st = m.and_then(|edge| edge.status.clone());
        out.push(Metric::new(
            format!("graph.edge.{}.{}__{}", e.kind, key_name(&e.from.name), key_name(&e.to.name)),
            b2f(ok), Direction::Boolean, ok,
            format!(
                "{} {}->{} obs={obsn} (>= {}) status={st:?}{}",
                e.kind, e.from.name, e.to.name, e.min_evidence,
                e.status.as_ref().map(|s| format!(" want={s}")).unwrap_or_default()
            ),
        ));
    }

    for e in &gt.no_edges {
        let m = match_edge(e, obs);
        let bound = m.is_some_and(|edge| edge.observation_count >= e.min_evidence);
        let ok = !bound;
        if ok {
            satisfied += 1;
        }
        let obsn = m.map(|edge| edge.observation_count).unwrap_or(0);
        out.push(Metric::new(
            format!("graph.no_edge.{}.{}__{}", e.kind, key_name(&e.from.name), key_name(&e.to.name)),
            b2f(ok), Direction::Boolean, ok,
            format!(
                "{} {}->{} must-not-bind: obs={obsn} (bar {}) bound={bound}",
                e.kind, e.from.name, e.to.name, e.min_evidence
            ),
        ));
    }

    // Anomalies (G2): resolve the subject by enrolled name; a pattern_anomaly of `kind` must exist.
    for a in &gt.anomalies {
        let sid = resolve_entity(&a.subject, &obs.entity_ids);
        let ok = sid.as_ref().is_some_and(|id| anomaly_present(obs, &a.subject.kind, id, &a.kind));
        if ok {
            satisfied += 1;
        }
        out.push(Metric::new(
            format!("graph.anomaly.{}.{}", a.kind, key_name(&a.subject.name)),
            b2f(ok), Direction::Boolean, ok,
            format!("anomaly {} for {}:{} present={ok}", a.kind, a.subject.kind, a.subject.name),
        ));
    }

    // Counter-assertion (G2): the subject must have NO anomaly of `kind` (non-over-firing).
    for a in &gt.no_anomalies {
        let sid = resolve_entity(&a.subject, &obs.entity_ids);
        let present = sid.as_ref().is_some_and(|id| anomaly_present(obs, &a.subject.kind, id, &a.kind));
        let ok = !present;
        if ok {
            satisfied += 1;
        }
        out.push(Metric::new(
            format!("graph.no_anomaly.{}.{}", a.kind, key_name(&a.subject.name)),
            b2f(ok), Direction::Boolean, ok,
            format!("no anomaly {} for {}:{} must hold: present={present}", a.kind, a.subject.kind, a.subject.name),
        ));
    }

    // Baselines (G2): the subject's recomputed row meets the visit floor + peak hour-of-day.
    for b in &gt.baselines {
        let sid = resolve_entity(&b.subject, &obs.entity_ids);
        let row = sid.as_ref().and_then(|id| {
            obs.baselines.iter().find(|r| r.subject_type == b.subject.kind && &r.subject_id == id)
        });
        let ok = row.is_some_and(|r| {
            b.min_visits.is_none_or(|m| r.visits_in_window >= m)
                && b.peak_hour_of_day.is_none_or(|h| peak_hour_of_day(&r.hour_histogram) == Some(h))
        });
        if ok {
            satisfied += 1;
        }
        let (v, pk) =
            row.map(|r| (r.visits_in_window, peak_hour_of_day(&r.hour_histogram))).unwrap_or((0, None));
        out.push(Metric::new(
            format!("graph.baseline.{}.{}", b.subject.kind, key_name(&b.subject.name)),
            b2f(ok), Direction::Boolean, ok,
            format!(
                "baseline {}:{} visits={v} peak_hod={pk:?} (want visits>={:?} hod={:?})",
                b.subject.kind, b.subject.name, b.min_visits, b.peak_hour_of_day
            ),
        ));
    }

    let frac = if total == 0 { 1.0 } else { satisfied as f64 / total as f64 };
    out.insert(0, Metric::new("graph.match", frac, Direction::HigherBetter, frac >= 1.0,
        format!("{satisfied}/{total} graph assertions satisfied")));
    out
}

/// True when a `pattern_anomaly` of `kind` exists for `(subject_kind, subject_id)`.
fn anomaly_present(obs: &Observed, subject_kind: &str, subject_id: &str, kind: &str) -> bool {
    obs.anomalies.iter().any(|a| {
        a.subject_type.as_deref() == Some(subject_kind)
            && a.subject_id.as_deref() == Some(subject_id)
            && a.kind.as_deref() == Some(kind)
    })
}

/// Hour-of-day (0..24) of the modal hour-of-week bucket (first max by bucket index; None if empty).
fn peak_hour_of_day(hist: &[i32]) -> Option<i64> {
    let mut best_i = usize::MAX;
    let mut best_v = i32::MIN;
    for (i, &v) in hist.iter().enumerate() {
        if v > best_v {
            best_v = v;
            best_i = i;
        }
    }
    if best_v <= 0 {
        return None;
    }
    Some((best_i % 24) as i64)
}

/// Resolve an [`EntityRef`] to its catalog id string. `device` ids are literal; the others map an
/// enrolled `display_name` through `EntityIds` (None ⇒ never enrolled / never re-identified).
fn resolve_entity(r: &EntityRef, ids: &EntityIds) -> Option<String> {
    match r.kind.as_str() {
        "device" => Some(r.name.clone()),
        "person" => ids.person.get(&r.name).cloned(),
        "speaker" => ids.speaker.get(&r.name).cloned(),
        "plate" => ids.plate.get(&r.name).cloned(),
        _ => None,
    }
}

/// Find the observed edge matching an [`EdgeExpect`], accepting either endpoint order (undirected
/// edges are producer-canonicalized; checking both orders is harmless for the directed types).
/// None when either endpoint fails to resolve OR no such edge exists.
fn match_edge<'a>(e: &EdgeExpect, obs: &'a Observed) -> Option<&'a EdgeObs> {
    let from = resolve_entity(&e.from, &obs.entity_ids)?;
    let to = resolve_entity(&e.to, &obs.entity_ids)?;
    obs.graph_edges.iter().find(|edge| {
        edge.edge_type == e.kind
            && ((edge.src_type == e.from.kind && edge.src_id == from && edge.dst_type == e.to.kind && edge.dst_id == to)
                || (edge.src_type == e.to.kind && edge.src_id == to && edge.dst_type == e.from.kind && edge.dst_id == from))
    })
}

fn b2f(b: bool) -> f64 {
    if b { 1.0 } else { 0.0 }
}

/// Metric-key-safe rendering of an entity name (lowercased, non-alnum → '_') so baseline keys stay
/// stable and shell-clean.
fn key_name(s: &str) -> String {
    s.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect()
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

// ----- chat / rag ------------------------------------------------------------

/// Score live RAG answers. Deterministic-first: `contains`/`clean`/`count_ok`/`routed`/`citations`/
/// `attribution`/`conv_scoped`/`errored` gate the verdict (they test facts + structure, robust to
/// LLM wording); `similarity` gates only if the fixture set a floor; `judge` is ALWAYS Info-only.
/// Metric keys are indexed by question position (`chat.q{i}.*`) so baselines line up — do NOT
/// reorder a fixture's questions once a baseline exists. Async because similarity + judge call
/// Ollama via `ctx` (whose pool also backs the `conv_scoped` citation lookup). `conv_gt`/`obs`/
/// `base_ns` feed `citation_conversation_label` (the GT-label -> dominant-id matching).
pub async fn score_chat(
    gt: &ChatGt,
    answers: &[RagAnswer],
    ctx: &Ctx,
    conv_gt: Option<&ConversationsGt>,
    obs: &Observed,
    base_ns: i64,
) -> Vec<Metric> {
    let mut m = Vec::new();
    let embed_model = std::env::var("EMBED_MODEL").unwrap_or_else(|_| "mxbai-embed-large".into());
    for (i, (q, a)) in gt.questions.iter().zip(answers.iter()).enumerate() {
        let p = |k: &str| format!("chat.q{i}.{k}");
        let ans_norm = normalize(&a.answer);

        // Guard: a swallowed `error` event must not silently pass the other checks.
        m.push(Metric::new(
            p("errored"),
            if a.errored { 0.0 } else { 1.0 },
            Direction::Boolean,
            !a.errored,
            if a.errored { "stream reported an error event" } else { "clean stream" },
        ));

        if !q.must_contain.is_empty() {
            let hits = q.must_contain.iter().filter(|s| ans_norm.contains(&normalize(s))).count();
            let frac = hits as f64 / q.must_contain.len() as f64;
            // On a miss, quote the answer head — "0/1 present" alone is undiagnosable
            // (paraphrase drift vs decline vs empty stream all look identical).
            let detail = if hits < q.must_contain.len() {
                let head: String = a.answer.chars().take(160).collect();
                format!(
                    "{hits}/{} required phrases present; answer: {head:?}",
                    q.must_contain.len()
                )
            } else {
                format!("{hits}/{} required phrases present", q.must_contain.len())
            };
            m.push(Metric::new(p("contains"), frac, Direction::HigherBetter, frac >= 1.0, detail));
        }
        if !q.must_contain_any.is_empty() {
            let hit = q.must_contain_any.iter().find(|s| ans_norm.contains(&normalize(s)));
            let ok = hit.is_some();
            let detail = match hit {
                Some(h) => format!("matched {h:?} (1 of {} accepted variants)", q.must_contain_any.len()),
                None => {
                    let head: String = a.answer.chars().take(160).collect();
                    format!(
                        "none of {} accepted variants present; answer: {head:?}",
                        q.must_contain_any.len()
                    )
                }
            };
            m.push(Metric::new(
                p("contains_any"),
                if ok { 1.0 } else { 0.0 },
                Direction::Boolean,
                ok,
                detail,
            ));
        }
        if !q.must_not_contain.is_empty() {
            let bad: Vec<&String> =
                q.must_not_contain.iter().filter(|s| ans_norm.contains(&normalize(s))).collect();
            m.push(Metric::new(
                p("clean"),
                if bad.is_empty() { 1.0 } else { 0.0 },
                Direction::Boolean,
                bad.is_empty(),
                if bad.is_empty() { "no forbidden phrases".to_string() } else { format!("forbidden phrase(s) present: {bad:?}") },
            ));
        }
        if let Some(n) = q.expect_number {
            let ok = answer_has_number(&ans_norm, n);
            m.push(Metric::new(
                p("count_ok"),
                if ok { 1.0 } else { 0.0 },
                Direction::Boolean,
                ok,
                format!("expected count {n} {}", if ok { "present" } else { "MISSING" }),
            ));
        }
        if let Some(want) = &q.expect_routed_agent {
            match &a.routed_agent_id {
                Some(got) => {
                    let ok = got.eq_ignore_ascii_case(want);
                    m.push(Metric::new(
                        p("routed"),
                        if ok { 1.0 } else { 0.0 },
                        Direction::Boolean,
                        ok,
                        format!("routed to '{got}', expected '{want}'"),
                    ));
                }
                // Old RAG binary without the routed_agent_id field: degrade to Info (never gates).
                None => m.push(Metric::info(
                    p("routed"),
                    0.0,
                    format!("routed_agent_id absent (RAG binary predates it); expected '{want}'"),
                )),
            }
        }
        if let Some(minc) = q.min_citations {
            let n = a.sources.len() as f64;
            m.push(Metric::new(
                p("citations"),
                n,
                Direction::HigherBetter,
                n >= minc as f64,
                format!("{} citations (min {minc})", a.sources.len()),
            ));
        }
        if !q.citation_must_attribute.is_empty() {
            let names: Vec<String> = a
                .sources
                .iter()
                .filter_map(|s| s.speaker_name.as_ref())
                .map(|s| normalize(s))
                .collect();
            let hits = q
                .citation_must_attribute
                .iter()
                .filter(|want| {
                    let w = normalize(want);
                    names.iter().any(|n| n.contains(&w))
                })
                .count();
            let frac = hits as f64 / q.citation_must_attribute.len() as f64;
            m.push(Metric::new(
                p("attribution"),
                frac,
                Direction::HigherBetter,
                frac >= 1.0,
                format!("{hits}/{} expected names attributed in citations", q.citation_must_attribute.len()),
            ));
        }
        if q.citations_single_conversation {
            m.push(conv_scoped_metric(p("conv_scoped"), q, a, ctx, conv_gt, obs, base_ns).await);
        }
        if let Some(reference) = &q.reference_answer {
            let sim = match (
                embed(ctx, &embed_model, &a.answer).await,
                embed(ctx, &embed_model, reference).await,
            ) {
                (Ok(x), Ok(y)) => cosine(&x, &y),
                _ => f64::NAN,
            };
            if sim.is_nan() {
                m.push(Metric::info(p("similarity"), 0.0, "similarity unavailable (embed failed)"));
            } else if let Some(floor) = gt.similarity_floor {
                m.push(Metric::new(
                    p("similarity"),
                    sim,
                    Direction::HigherBetter,
                    sim >= floor,
                    format!("cosine {sim:.3} vs floor {floor:.3}"),
                ));
            } else {
                m.push(Metric::info(p("similarity"), sim, format!("cosine {sim:.3} (Info)")));
            }
        }
        if gt.judge_enabled {
            if let Some(rubric) = &q.judge_rubric {
                let score = judge(ctx, &q.ask, &a.answer, rubric).await.unwrap_or(0.0);
                m.push(Metric::info(p("judge"), score, format!("LLM-judge {score:.2} (Info)")));
            }
        }
    }
    m
}

/// `chat.q{i}.conv_scoped`: did the answer's citations stay inside ONE threaded conversation?
/// Resolves each cited source's segment_id to its sentences' `conversation_id`s via the eval pool
/// (`ctx.pool` — the same DB-direct access `observe` uses). Passes iff they collapse to exactly one
/// non-NULL id (plus, when `citation_conversation_label` is set, that id must equal the GT label's
/// dominant observed id). When EVERY cited sentence is unthreaded (NULL), degrades to an Info-style
/// non-gating pass — the `routed` pattern: a threader that hasn't run is missing infrastructure,
/// not a wrong answer. A failed lookup likewise degrades to Info (never a false regression).
async fn conv_scoped_metric(
    key: String,
    q: &ChatQ,
    a: &RagAnswer,
    ctx: &Ctx,
    conv_gt: Option<&ConversationsGt>,
    obs: &Observed,
    base_ns: i64,
) -> Metric {
    if a.sources.is_empty() {
        return Metric::new(key, 0.0, Direction::Boolean, false, "no citations to scope");
    }
    let mut seg_ids: Vec<Uuid> =
        a.sources.iter().filter_map(|s| Uuid::parse_str(&s.segment_id).ok()).collect();
    seg_ids.sort();
    seg_ids.dedup();
    if seg_ids.is_empty() {
        return Metric::new(
            key,
            0.0,
            Direction::Boolean,
            false,
            format!("{} citations but no parseable segment ids", a.sources.len()),
        );
    }
    let rows: Vec<(Option<Uuid>,)> = match sqlx::query_as(
        "SELECT DISTINCT conversation_id FROM transcript_sentences WHERE segment_id = ANY($1)",
    )
    .bind(&seg_ids)
    .fetch_all(&ctx.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return Metric::info(key, 0.0, format!("conversation lookup failed: {e}")),
    };
    if rows.is_empty() {
        return Metric::new(
            key,
            0.0,
            Direction::Boolean,
            false,
            format!("no transcript sentences found for {} cited segment(s)", seg_ids.len()),
        );
    }
    let has_null = rows.iter().any(|(c,)| c.is_none());
    let mut ids: Vec<Uuid> = rows.into_iter().filter_map(|(c,)| c).collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        // Every cited sentence unthreaded: the threader hasn't assigned here — degrade, don't fail.
        return Metric::info(
            key,
            1.0,
            "all cited segments unthreaded (conversation_id NULL); non-gating pass until the threader runs",
        );
    }

    let mut ok = ids.len() == 1 && !has_null;
    let mut detail = format!(
        "{} distinct conversation id(s) across {} cited segment(s){}",
        ids.len(),
        seg_ids.len(),
        if has_null { " (+ unthreaded sentences)" } else { "" },
    );
    if let Some(label) = &q.citation_conversation_label {
        let want = conv_gt.and_then(|g| {
            let matches = match_conv_utterances(&g.utterances, &obs.sentences, base_ns);
            label_dominant(&matches, label)
        });
        match (want, ok) {
            (Some(w), true) => {
                ok = ids[0] == w;
                detail = format!("{detail}; cited {} vs label '{label}' dominant {w}", ids[0]);
            }
            (Some(w), false) => detail = format!("{detail}; label '{label}' dominant {w} (unchecked: not a single id)"),
            (None, _) => {
                ok = false;
                detail = format!("{detail}; label '{label}' has no dominant conversation id in ground truth");
            }
        }
    }
    Metric::new(key, if ok { 1.0 } else { 0.0 }, Direction::Boolean, ok, detail)
}

/// Embed `text` via the same Ollama the RAG/worker use (`/api/embeddings`, `EMBED_MODEL`). Local +
/// deterministic; the model is already in the config-hash.
async fn embed(ctx: &Ctx, model: &str, text: &str) -> anyhow::Result<Vec<f32>> {
    let url = format!("{}/api/embeddings", ctx.ollama_url.trim_end_matches('/'));
    let resp = ctx
        .http
        .post(&url)
        .json(&serde_json::json!({ "model": model, "prompt": text }))
        .send()
        .await?
        .error_for_status()?;
    let v: serde_json::Value = resp.json().await?;
    let emb = v
        .get("embedding")
        .and_then(|e| e.as_array())
        .ok_or_else(|| anyhow::anyhow!("no embedding in Ollama response"))?
        .iter()
        .filter_map(|x| x.as_f64().map(|f| f as f32))
        .collect::<Vec<f32>>();
    if emb.is_empty() {
        anyhow::bail!("empty embedding");
    }
    Ok(emb)
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return f64::NAN;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b.iter()) {
        dot += (*x as f64) * (*y as f64);
        na += (*x as f64).powi(2);
        nb += (*y as f64).powi(2);
    }
    if na == 0.0 || nb == 0.0 { f64::NAN } else { dot / (na.sqrt() * nb.sqrt()) }
}

/// Info-only LLM judge: ask the local model to grade the answer 0..1 against a rubric. Best-effort —
/// any failure yields 0.0 and, being an Info metric, never gates the verdict.
async fn judge(ctx: &Ctx, question: &str, answer: &str, rubric: &str) -> anyhow::Result<f64> {
    let model = std::env::var("RAG_LLM_MODEL").unwrap_or_else(|_| "qwen2.5:7b".into());
    let prompt = format!(
        "You are grading an assistant's answer. Rubric: {rubric}\n\nQuestion: {question}\nAnswer: {answer}\n\n\
         Reply with ONLY a number from 0.0 (fails the rubric) to 1.0 (fully satisfies it)."
    );
    let url = format!("{}/api/generate", ctx.ollama_url.trim_end_matches('/'));
    let resp = ctx
        .http
        .post(&url)
        .json(&serde_json::json!({ "model": model, "prompt": prompt, "stream": false, "options": {"temperature": 0.0} }))
        .send()
        .await?
        .error_for_status()?;
    let v: serde_json::Value = resp.json().await?;
    let text = v.get("response").and_then(|x| x.as_str()).unwrap_or("");
    // Pull the first float-looking token.
    let num: String = text
        .chars()
        .skip_while(|c| !c.is_ascii_digit() && *c != '.')
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    Ok(num.parse::<f64>().unwrap_or(0.0).clamp(0.0, 1.0))
}

/// True if `answer_norm` (already normalized: lowercase, single-spaced) contains `n` as a digit
/// token OR its English number-word form. Whole-token match so "13" doesn't satisfy "3".
fn answer_has_number(answer_norm: &str, n: i64) -> bool {
    let tokens: Vec<&str> = answer_norm.split_whitespace().collect();
    let digit = n.to_string();
    if tokens.iter().any(|t| *t == digit) {
        return true;
    }
    if let Some(word) = number_word(n) {
        let wt: Vec<&str> = word.split_whitespace().collect();
        if !wt.is_empty() && tokens.windows(wt.len()).any(|w| w == wt.as_slice()) {
            return true;
        }
    }
    false
}

/// English number-word for 0..=99 (lowercase, space-separated e.g. "twenty three"). None otherwise.
fn number_word(n: i64) -> Option<String> {
    const ONES: [&str; 20] = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen",
        "nineteen",
    ];
    const TENS: [&str; 10] =
        ["", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];
    match n {
        0..=19 => Some(ONES[n as usize].to_string()),
        20..=99 => {
            let (t, o) = ((n / 10) as usize, (n % 10) as usize);
            if o == 0 { Some(TENS[t].to_string()) } else { Some(format!("{} {}", TENS[t], ONES[o])) }
        }
        _ => None,
    }
}

// ----- advisor ---------------------------------------------------------------

/// Score a scripted advisor conversation (the `advisor` modality). Deterministic-first, like
/// `score_chat`: every check tests STRUCTURE (did a follow-up round fire, did the turn end in an
/// answer, what grounded it) or FACTS (substrings), never prose shape. Metric keys are indexed by
/// turn position (`advisor.t{i}.*`) so baselines line up — do NOT reorder a fixture's turns once
/// a baseline exists. Sync + pure (no service calls): the live querying already happened in
/// `query_advisor`, and any transport failure went INCONCLUSIVE before reaching here.
pub fn score_advisor(gt: &AdvisorGt, turns: &[crate::query_advisor::AdvisorTurnResult]) -> Vec<Metric> {
    let mut m = Vec::new();
    for (i, (t, r)) in gt.turns.iter().zip(turns.iter()).enumerate() {
        let p = |k: &str| format!("advisor.t{i}.{k}");
        let ans_norm = normalize(&r.answer);

        // Guard: a swallowed `error` event must not silently pass the other checks.
        m.push(Metric::new(
            p("errored"),
            if r.errored { 0.0 } else { 1.0 },
            Direction::Boolean,
            !r.errored,
            if r.errored { "stream reported an error event" } else { "clean stream" },
        ));

        if let Some(want) = t.expect_questions {
            let ok = r.saw_questions == want;
            let detail = match (want, r.saw_questions) {
                (true, true) => format!("follow-up round fired ({} questions)", r.questions.len()),
                (true, false) => "expected a follow-up questions round; none fired".to_string(),
                (false, false) => "no follow-up round (as expected)".to_string(),
                (false, true) => format!("unexpected follow-up round: {:?}", r.questions),
            };
            m.push(Metric::new(p("questions"), if ok { 1.0 } else { 0.0 }, Direction::Boolean, ok, detail));
        }
        if let Some(want) = t.expect_final_answer {
            // "Ended in a final answer" == token text streamed (a questions turn streams none).
            let has_answer = !ans_norm.is_empty();
            let ok = has_answer == want;
            let detail = match (want, has_answer) {
                (true, true) => format!("final answer streamed ({} chars)", r.answer.len()),
                (true, false) if r.saw_questions => "turn ended in a questions round, not an answer".to_string(),
                (true, false) => "no final answer text streamed".to_string(),
                (false, false) => "no final answer (as expected)".to_string(),
                (false, true) => {
                    let head: String = r.answer.chars().take(160).collect();
                    format!("unexpected final answer: {head:?}")
                }
            };
            m.push(Metric::new(p("final_answer"), if ok { 1.0 } else { 0.0 }, Direction::Boolean, ok, detail));
        }
        if !t.expect_chapters_any.is_empty() {
            let ok = t.expect_chapters_any.iter().any(|c| r.chapters.contains(c));
            m.push(Metric::new(
                p("chapters_any"),
                if ok { 1.0 } else { 0.0 },
                Direction::Boolean,
                ok,
                format!("final grounding {:?} vs any-of {:?}", r.chapters, t.expect_chapters_any),
            ));
        }
        if !t.expect_chapters_all.is_empty() {
            let hits = t.expect_chapters_all.iter().filter(|c| r.chapters.contains(c)).count();
            let frac = hits as f64 / t.expect_chapters_all.len() as f64;
            m.push(Metric::new(
                p("chapters_all"),
                frac,
                Direction::HigherBetter,
                frac >= 1.0,
                format!("{hits}/{} required chapters in final grounding {:?}", t.expect_chapters_all.len(), r.chapters),
            ));
        }
        if !t.expect_substrings.is_empty() {
            let hits = t.expect_substrings.iter().filter(|s| ans_norm.contains(&normalize(s))).count();
            let frac = hits as f64 / t.expect_substrings.len() as f64;
            // On a miss, quote the answer head (the `score_chat` pattern) — a bare count is
            // undiagnosable (paraphrase drift vs questions-round vs empty stream look identical).
            let detail = if hits < t.expect_substrings.len() {
                let head: String = r.answer.chars().take(160).collect();
                format!("{hits}/{} required substrings present; answer: {head:?}", t.expect_substrings.len())
            } else {
                format!("{hits}/{} required substrings present", t.expect_substrings.len())
            };
            m.push(Metric::new(p("contains"), frac, Direction::HigherBetter, frac >= 1.0, detail));
        }
    }
    m
}

#[cfg(test)]
mod advisor_tests {
    use super::*;
    use crate::query_advisor::AdvisorTurnResult;

    fn turn(msg: &str) -> AdvisorTurn {
        AdvisorTurn {
            message: msg.into(),
            expect_questions: None,
            expect_final_answer: None,
            expect_chapters_any: vec![],
            expect_chapters_all: vec![],
            expect_substrings: vec![],
        }
    }

    fn questions_turn() -> AdvisorTurnResult {
        AdvisorTurnResult {
            message: "m".into(),
            session_id: "s".into(),
            saw_questions: true,
            questions: vec!["What happened?".into()],
            ..Default::default()
        }
    }

    fn answer_turn(answer: &str, chapters: &[i64]) -> AdvisorTurnResult {
        AdvisorTurnResult {
            message: "m".into(),
            session_id: "s".into(),
            answer: answer.into(),
            chapters: chapters.to_vec(),
            ..Default::default()
        }
    }

    fn find<'a>(ms: &'a [Metric], key: &str) -> &'a Metric {
        ms.iter().find(|m| m.key == key).unwrap_or_else(|| panic!("metric {key} missing"))
    }

    #[test]
    fn questions_assertion_both_polarities() {
        let mut t0 = turn("bare");
        t0.expect_questions = Some(true);
        let mut t1 = turn("full");
        t1.expect_questions = Some(false);
        let gt = AdvisorGt { turns: vec![t0, t1] };

        // t0 asked (as expected), t1 answered (as expected) -> both pass.
        let ms = score_advisor(&gt, &[questions_turn(), answer_turn("do this", &[])]);
        assert!(find(&ms, "advisor.t0.questions").floor_ok);
        assert!(find(&ms, "advisor.t1.questions").floor_ok);

        // Flipped observations -> both fail.
        let ms = score_advisor(&gt, &[answer_turn("do this", &[]), questions_turn()]);
        assert!(!find(&ms, "advisor.t0.questions").floor_ok);
        assert!(!find(&ms, "advisor.t1.questions").floor_ok);
    }

    #[test]
    fn final_answer_requires_token_text() {
        let mut t = turn("full context");
        t.expect_final_answer = Some(true);
        let gt = AdvisorGt { turns: vec![t] };

        assert!(find(&score_advisor(&gt, &[answer_turn("here is a plan", &[])]), "advisor.t0.final_answer").floor_ok);
        // A questions round streams no tokens -> the final-answer assertion fails, diagnosably.
        let m = &score_advisor(&gt, &[questions_turn()]);
        let fa = find(m, "advisor.t0.final_answer");
        assert!(!fa.floor_ok);
        assert!(fa.detail.contains("questions round"));

        // Negative polarity: a questions turn must NOT have streamed an answer.
        let mut t = turn("bare");
        t.expect_final_answer = Some(false);
        let gt = AdvisorGt { turns: vec![t] };
        assert!(find(&score_advisor(&gt, &[questions_turn()]), "advisor.t0.final_answer").floor_ok);
        assert!(!find(&score_advisor(&gt, &[answer_turn("surprise", &[])]), "advisor.t0.final_answer").floor_ok);
    }

    #[test]
    fn chapters_any_and_all_check_final_grounding() {
        let mut t = turn("q");
        t.expect_chapters_any = vec![3, 7];
        t.expect_chapters_all = vec![3, 9];
        let gt = AdvisorGt { turns: vec![t] };

        let ms = score_advisor(&gt, &[answer_turn("a", &[3, 9, 12])]);
        assert!(find(&ms, "advisor.t0.chapters_any").floor_ok); // 3 present
        let all = find(&ms, "advisor.t0.chapters_all");
        assert_eq!(all.value, 1.0); // 3 and 9 both present
        assert!(all.floor_ok);

        let ms = score_advisor(&gt, &[answer_turn("a", &[9])]);
        assert!(!find(&ms, "advisor.t0.chapters_any").floor_ok); // neither 3 nor 7
        let all = find(&ms, "advisor.t0.chapters_all");
        assert_eq!(all.value, 0.5); // 9 of {3,9}
        assert!(!all.floor_ok);
    }

    #[test]
    fn substrings_are_case_insensitive_and_normalized() {
        let mut t = turn("q");
        t.expect_substrings = vec!["Win-Win".into(), "reciprocity".into()];
        let gt = AdvisorGt { turns: vec![t] };
        // Case + punctuation differences must not matter (both sides go through `normalize`).
        let ms = score_advisor(&gt, &[answer_turn("Aim for a win win outcome; use RECIPROCITY.", &[])]);
        let c = find(&ms, "advisor.t0.contains");
        assert_eq!(c.value, 1.0);
        assert!(c.floor_ok);

        let ms = score_advisor(&gt, &[answer_turn("Aim for a win win outcome.", &[])]);
        let c = find(&ms, "advisor.t0.contains");
        assert_eq!(c.value, 0.5);
        assert!(!c.floor_ok);
        assert!(c.detail.contains("answer:")); // miss quotes the answer head
    }

    #[test]
    fn errored_stream_gates_even_without_assertions() {
        let gt = AdvisorGt { turns: vec![turn("q")] };
        let mut r = answer_turn("partial", &[]);
        r.errored = true;
        assert!(!find(&score_advisor(&gt, &[r]), "advisor.t0.errored").floor_ok);
        assert!(find(&score_advisor(&gt, &[answer_turn("ok", &[])]), "advisor.t0.errored").floor_ok);
    }
}

#[cfg(test)]
mod chat_tests {
    use super::*;

    #[test]
    fn number_matches_digit_and_word_whole_token() {
        assert!(answer_has_number(&normalize("Alice visited 3 times"), 3));
        assert!(answer_has_number(&normalize("Alice visited three times"), 3));
        // whole-token: "13" must not satisfy 3
        assert!(!answer_has_number(&normalize("there were 13 visits"), 3));
        assert!(answer_has_number(&normalize("twenty three sightings"), 23));
        assert!(answer_has_number(&normalize("I counted 0 visits"), 0));
    }

    #[test]
    fn cosine_of_identical_is_one() {
        let v = vec![0.1f32, 0.2, 0.3];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod conversation_tests {
    use super::*;

    fn u(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn sent(text: &str, start: i64, end: i64, conv: Option<Uuid>) -> Sentence {
        Sentence {
            text: text.into(),
            start_ns: start,
            end_ns: end,
            sentiment: None,
            speaker_id: None,
            conversation_id: conv,
        }
    }

    fn utt(label: &str, contains: &str, a: i64, b: i64) -> ConvUttGt {
        ConvUttGt { label: label.into(), text_contains: contains.into(), window_ns: [a, b] }
    }

    fn find<'a>(ms: &'a [Metric], key: &str) -> &'a Metric {
        ms.iter().find(|m| m.key == key).unwrap_or_else(|| panic!("metric {key} missing"))
    }

    #[test]
    fn pairwise_f1_perfect_clustering() {
        let utts = vec![
            ("A".to_string(), Some(u(1))),
            ("A".to_string(), Some(u(1))),
            ("B".to_string(), Some(u(2))),
        ];
        let (p, r, f1) = conv_pairwise_f1(&utts);
        assert_eq!((p, r, f1), (1.0, 1.0, 1.0));
    }

    #[test]
    fn pairwise_f1_everything_merged() {
        // 3 utterances all on one observed id: same_obs=3 pairs, same_gt=1 (A-A), both=1
        // -> P 1/3, R 1, F1 0.5.
        let utts = vec![
            ("A".to_string(), Some(u(1))),
            ("A".to_string(), Some(u(1))),
            ("B".to_string(), Some(u(1))),
        ];
        let (p, r, f1) = conv_pairwise_f1(&utts);
        assert!((p - 1.0 / 3.0).abs() < 1e-9);
        assert!((r - 1.0).abs() < 1e-9);
        assert!((f1 - 0.5).abs() < 1e-9);
    }

    #[test]
    fn canon_conv_labels_joins_must_merge_components() {
        // T1+T2 are one GT conversation (declared via must_merge); T3 stands alone. Canonical
        // key = smallest label of the component, so T1/T2 pairs stop counting as "should split".
        let gt = ConversationsGt {
            distinct_count: 2,
            count_tolerance: 0,
            min_pairwise_f1: 0.9,
            min_coverage: 0.9,
            must_not_merge: vec![],
            must_merge: vec![["T1".into(), "T2".into()]],
            utterances: vec![
                ConvUttGt { label: "T1".into(), text_contains: "a".into(), window_ns: [0, 1] },
                ConvUttGt { label: "T2".into(), text_contains: "b".into(), window_ns: [0, 1] },
                ConvUttGt { label: "T3".into(), text_contains: "c".into(), window_ns: [0, 1] },
            ],
        };
        let canon = canon_conv_labels(&gt);
        assert_eq!(canon["T1"], "T1");
        assert_eq!(canon["T2"], "T1");
        assert_eq!(canon["T3"], "T3");
        // With canonical labels, one observed id over T1+T2 is a PERFECT clustering.
        let utts = vec![
            (canon["T1"].clone(), Some(u(1))),
            (canon["T2"].clone(), Some(u(1))),
        ];
        let (p, r, f1) = conv_pairwise_f1(&utts);
        assert_eq!((p, r, f1), (1.0, 1.0, 1.0));
    }

    #[test]
    fn pairwise_f1_null_dominant_is_a_singleton() {
        // Same GT label but one side unthreaded: no same-observed pair -> recall 0 -> F1 0.
        let utts = vec![("A".to_string(), Some(u(1))), ("A".to_string(), None)];
        let (p, r, f1) = conv_pairwise_f1(&utts);
        assert_eq!(p, 1.0); // no predicted pairs -> no false positives
        assert_eq!(r, 0.0);
        assert_eq!(f1, 0.0);
    }

    #[test]
    fn pairwise_f1_vacuous_is_perfect() {
        // No same-GT-label pairs and no same-observed pairs => 1.0 by definition.
        let utts = vec![("A".to_string(), Some(u(1))), ("B".to_string(), Some(u(2)))];
        assert_eq!(conv_pairwise_f1(&utts).2, 1.0);
        // Two distinct labels, both unthreaded: also vacuous.
        let utts = vec![("A".to_string(), None), ("B".to_string(), None)];
        assert_eq!(conv_pairwise_f1(&utts).2, 1.0);
    }

    #[test]
    fn dominant_ties_break_to_smaller_uuid() {
        let mut c: HashMap<Uuid, usize> = HashMap::new();
        assert_eq!(dominant_conv_id(&c), None);
        c.insert(u(7), 2);
        c.insert(u(3), 2);
        assert_eq!(dominant_conv_id(&c), Some(u(3))); // tie -> lexicographically smaller
        c.insert(u(9), 5);
        assert_eq!(dominant_conv_id(&c), Some(u(9))); // count wins over uuid order
    }

    #[test]
    fn dominant_is_most_sentences_via_matching() {
        // One GT utterance whose window covers 3 sentences: 2x conv 5, 1x conv 4 -> dominant 5.
        let sentences = vec![
            sent("alpha one", 0, 10, Some(u(5))),
            sent("alpha two", 10, 20, Some(u(5))),
            sent("alpha three", 20, 30, Some(u(4))),
        ];
        let matches = match_conv_utterances(&[utt("A", "alpha", 0, 30)], &sentences, 0);
        assert_eq!(matches[0].dominant, Some(u(5)));
        // text_contains filters: only "alpha two" matches -> dominant follows the filter.
        let matches = match_conv_utterances(&[utt("A", "alpha two", 0, 30)], &sentences, 0);
        assert_eq!(matches[0].dominant, Some(u(5)));
        // Window filters: only the last sentence overlaps [25,30).
        let matches = match_conv_utterances(&[utt("A", "alpha", 25, 30)], &sentences, 0);
        assert_eq!(matches[0].dominant, Some(u(4)));
    }

    #[test]
    fn must_not_merge_nulls_never_match() {
        // Both labels entirely unthreaded (NULL): they must NOT count as sharing an id, so
        // must_not_merge passes — while coverage correctly fails (nothing threaded).
        let gt = ConversationsGt {
            distinct_count: 2,
            count_tolerance: 0,
            utterances: vec![utt("A", "hello", 0, 10), utt("B", "world", 10, 20)],
            min_pairwise_f1: 0.90,
            min_coverage: 0.90,
            must_not_merge: vec![["A".into(), "B".into()]],
            must_merge: vec![],
        };
        let obs = Observed {
            sentences: vec![sent("hello there", 1, 5, None), sent("world peace", 11, 15, None)],
            ..Default::default()
        };
        let ms = score_conversations(&gt, &obs, 0);
        assert!(find(&ms, "conversations.must_not_merge.A-B").floor_ok);
        assert!(!find(&ms, "conversations.coverage").floor_ok);
        // 0 observed ids vs expected 2 -> count_error 2 breaches tolerance 0.
        let ce = find(&ms, "conversations.count_error");
        assert_eq!(ce.value, 2.0);
        assert!(!ce.floor_ok);
    }

    #[test]
    fn must_not_merge_fails_on_shared_id_and_must_merge_passes_on_equal_dominants() {
        let gt = ConversationsGt {
            distinct_count: 1,
            count_tolerance: 0,
            utterances: vec![utt("A", "hello", 0, 10), utt("B", "world", 10, 20)],
            min_pairwise_f1: 0.90,
            min_coverage: 0.90,
            must_not_merge: vec![["A".into(), "B".into()]],
            must_merge: vec![["A".into(), "B".into()]],
        };
        let obs = Observed {
            sentences: vec![sent("hello there", 1, 5, Some(u(1))), sent("world peace", 11, 15, Some(u(1)))],
            ..Default::default()
        };
        let ms = score_conversations(&gt, &obs, 0);
        assert!(!find(&ms, "conversations.must_not_merge.A-B").floor_ok);
        assert!(find(&ms, "conversations.must_merge.A-B").floor_ok);
        assert!(find(&ms, "conversations.coverage").floor_ok);
        assert!(find(&ms, "conversations.count_error").floor_ok);
        // fragmentation Info: 1 observed / 1 expected.
        assert_eq!(find(&ms, "conversations.fragmentation").value, 1.0);
    }

    #[test]
    fn must_merge_requires_non_null_dominants() {
        let gt = ConversationsGt {
            distinct_count: 1,
            count_tolerance: 1,
            utterances: vec![utt("A", "hello", 0, 10), utt("B", "world", 10, 20)],
            min_pairwise_f1: 0.90,
            min_coverage: 0.90,
            must_not_merge: vec![],
            must_merge: vec![["A".into(), "B".into()]],
        };
        // B unthreaded -> its dominant is None -> must_merge fails even though A has an id.
        let obs = Observed {
            sentences: vec![sent("hello there", 1, 5, Some(u(1))), sent("world peace", 11, 15, None)],
            ..Default::default()
        };
        let ms = score_conversations(&gt, &obs, 0);
        assert!(!find(&ms, "conversations.must_merge.A-B").floor_ok);
    }

    #[test]
    fn count_error_scopes_to_union_of_gt_windows() {
        // A sentence OUTSIDE every GT window carries a third id — it must not inflate the count.
        let gt = ConversationsGt {
            distinct_count: 2,
            count_tolerance: 0,
            utterances: vec![utt("A", "", 0, 10), utt("B", "", 10, 20)],
            min_pairwise_f1: 0.90,
            min_coverage: 0.90,
            must_not_merge: vec![],
            must_merge: vec![],
        };
        let obs = Observed {
            sentences: vec![
                sent("in a", 1, 5, Some(u(1))),
                sent("in b", 11, 15, Some(u(2))),
                sent("way outside", 1_000, 1_010, Some(u(3))),
            ],
            ..Default::default()
        };
        let ms = score_conversations(&gt, &obs, 0);
        let ce = find(&ms, "conversations.count_error");
        assert_eq!(ce.value, 0.0);
        assert!(ce.floor_ok);
        assert!(find(&ms, "conversations.pairwise_f1").floor_ok);
    }
}

#[cfg(test)]
mod graph_tests {
    use super::*;

    fn find<'a>(ms: &'a [Metric], key: &str) -> &'a Metric {
        ms.iter().find(|m| m.key == key).unwrap_or_else(|| panic!("metric {key} not found in {:?}", ms.iter().map(|m| &m.key).collect::<Vec<_>>()))
    }

    fn edge(kind: &str, st: &str, si: &str, dt: &str, di: &str, obs: i64, status: Option<&str>) -> EdgeObs {
        EdgeObs {
            edge_type: kind.into(),
            src_type: st.into(), src_id: si.into(),
            dst_type: dt.into(), dst_id: di.into(),
            observation_count: obs,
            confidence: None,
            status: status.map(|s| s.into()),
        }
    }
    fn eref(kind: &str, name: &str) -> EntityRef {
        EntityRef { kind: kind.into(), name: name.into() }
    }
    fn expect(from: EntityRef, to: EntityRef, kind: &str, min_evidence: i64, status: Option<&str>) -> EdgeExpect {
        EdgeExpect { from, to, kind: kind.into(), min_evidence, status: status.map(|s| s.into()) }
    }
    /// Observed with the person "Alice"→A, speaker "Alice"→S, plate "EMD774"→P resolution seeded.
    fn obs_with(edges: Vec<EdgeObs>) -> Observed {
        let mut o = Observed { graph_edges: edges, ..Default::default() };
        o.entity_ids.person.insert("Alice".into(), "A".into());
        o.entity_ids.person.insert("Bob".into(), "B".into());
        o.entity_ids.speaker.insert("Alice".into(), "S".into());
        o.entity_ids.plate.insert("EMD774".into(), "P".into());
        o
    }

    #[test]
    fn edge_present_at_or_above_evidence_passes_below_fails() {
        // visits_place person Alice(A) -> device front, obs 3.
        let o = obs_with(vec![edge("visits_place", "person", "A", "device", "front", 3, None)]);
        let e = expect(eref("person", "Alice"), eref("device", "front"), "visits_place", 3, None);
        let gt = GraphGt { edges: vec![e.clone()], ..Default::default() };
        assert!(find(&score_graph(&gt, &o), "graph.edge.visits_place.alice__front").floor_ok);

        // Same edge, but demand 4 observations — fails.
        let mut too_high = e;
        too_high.min_evidence = 4;
        let gt = GraphGt { edges: vec![too_high], ..Default::default() };
        assert!(!find(&score_graph(&gt, &o), "graph.edge.visits_place.alice__front").floor_ok);
    }

    #[test]
    fn undirected_edge_matches_either_endpoint_order() {
        // Stored canonical (speaker S before person A alphabetically by (type,id)); the assertion
        // names person→speaker (the opposite order) and must still match.
        let o = obs_with(vec![edge("same_identity_candidate", "person", "A", "speaker", "S", 5, Some("candidate"))]);
        let e = expect(eref("speaker", "Alice"), eref("person", "Alice"), "same_identity_candidate", 1, None);
        let gt = GraphGt { edges: vec![e], ..Default::default() };
        assert!(find(&score_graph(&gt, &o), "graph.edge.same_identity_candidate.alice__alice").floor_ok);
    }

    #[test]
    fn status_pin_gates_binding() {
        let o = obs_with(vec![edge("same_identity_candidate", "person", "A", "speaker", "S", 5, Some("candidate"))]);
        // Want confirmed but it's only a candidate -> fail.
        let e = expect(eref("person", "Alice"), eref("speaker", "Alice"), "same_identity_candidate", 1, Some("confirmed"));
        let gt = GraphGt { edges: vec![e], ..Default::default() };
        assert!(!find(&score_graph(&gt, &o), "graph.edge.same_identity_candidate.alice__alice").floor_ok);
    }

    #[test]
    fn no_edge_is_threshold_aware() {
        // A single co-sighting made a person->plate edge with obs=1. no_edge with bar 2 PASSES
        // (below bar = did not bind); the SAME assertion with bar 1 FAILS (an edge exists at >=1).
        let o = obs_with(vec![edge("arrived_with_vehicle", "person", "B", "plate", "P", 1, None)]);
        let below = expect(eref("person", "Bob"), eref("plate", "EMD774"), "arrived_with_vehicle", 2, None);
        let gt = GraphGt { no_edges: vec![below], ..Default::default() };
        assert!(find(&score_graph(&gt, &o), "graph.no_edge.arrived_with_vehicle.bob__emd774").floor_ok);

        let strict = expect(eref("person", "Bob"), eref("plate", "EMD774"), "arrived_with_vehicle", 1, None);
        let gt = GraphGt { no_edges: vec![strict], ..Default::default() };
        assert!(!find(&score_graph(&gt, &o), "graph.no_edge.arrived_with_vehicle.bob__emd774").floor_ok);
    }

    #[test]
    fn no_edge_passes_when_endpoint_never_enrolled() {
        // The silent face was never enrolled/named -> unresolvable -> no matching edge -> no_edge OK.
        let o = obs_with(vec![]);
        let e = expect(eref("person", "Ghost"), eref("speaker", "Alice"), "same_identity_candidate", 1, None);
        let gt = GraphGt { no_edges: vec![e], ..Default::default() };
        assert!(find(&score_graph(&gt, &o), "graph.no_edge.same_identity_candidate.ghost__alice").floor_ok);
    }

    #[test]
    fn entity_resolution_and_aggregate() {
        let o = obs_with(vec![]);
        let gt = GraphGt {
            entities: vec![eref("person", "Alice"), eref("device", "front"), eref("person", "Nobody")],
            ..Default::default()
        };
        let ms = score_graph(&gt, &o);
        assert!(find(&ms, "graph.entity.person.alice").floor_ok); // enrolled
        assert!(find(&ms, "graph.entity.device.front").floor_ok); // device literal always resolves
        assert!(!find(&ms, "graph.entity.person.nobody").floor_ok); // never enrolled
        // aggregate: 2 of 3 satisfied.
        let agg = find(&ms, "graph.match");
        assert!((agg.value - 2.0 / 3.0).abs() < 1e-9);
        assert!(!agg.floor_ok);
    }

    fn anom(kind: &str) -> crate::query::AnomalyObs {
        crate::query::AnomalyObs {
            subject_type: Some("person".into()),
            subject_id: Some("A".into()), // Alice (person) resolves to "A" in obs_with
            kind: Some(kind.into()),
        }
    }

    #[test]
    fn anomaly_present_and_no_anomaly_counter() {
        let mut o = obs_with(vec![]);
        o.anomalies = vec![anom("off_schedule_presence")];
        // expect_anomaly for the present kind PASSES; a different kind FAILS.
        let gt = GraphGt {
            anomalies: vec![
                AnomalyExpect { subject: eref("person", "Alice"), kind: "off_schedule_presence".into() },
                AnomalyExpect { subject: eref("person", "Alice"), kind: "unknown_person_cluster".into() },
            ],
            ..Default::default()
        };
        let ms = score_graph(&gt, &o);
        assert!(find(&ms, "graph.anomaly.off_schedule_presence.alice").floor_ok);
        assert!(!find(&ms, "graph.anomaly.unknown_person_cluster.alice").floor_ok);

        // expect_no_anomaly: FAILS for the present kind, PASSES for an absent kind.
        let gt = GraphGt {
            no_anomalies: vec![
                AnomalyExpect { subject: eref("person", "Alice"), kind: "off_schedule_presence".into() },
                AnomalyExpect { subject: eref("person", "Alice"), kind: "first_time_pairing".into() },
            ],
            ..Default::default()
        };
        let ms = score_graph(&gt, &o);
        assert!(!find(&ms, "graph.no_anomaly.off_schedule_presence.alice").floor_ok);
        assert!(find(&ms, "graph.no_anomaly.first_time_pairing.alice").floor_ok);
    }

    #[test]
    fn baseline_visits_floor_and_peak_hour() {
        let mut o = obs_with(vec![]);
        let mut hist = vec![0i32; 168];
        hist[105] = 5; // Thursday 09:00 (weekday 4 * 24 + 9)
        hist[3] = 1; // a lone off-hours visit
        o.baselines = vec![crate::query::BaselineObs {
            subject_type: "person".into(),
            subject_id: "A".into(),
            visits_in_window: 6,
            hour_histogram: hist,
        }];
        // visits>=5 AND peak hour-of-day 9 -> pass.
        let gt = GraphGt {
            baselines: vec![BaselineExpect {
                subject: eref("person", "Alice"),
                min_visits: Some(5),
                peak_hour_of_day: Some(9),
            }],
            ..Default::default()
        };
        assert!(find(&score_graph(&gt, &o), "graph.baseline.person.alice").floor_ok);
        // Too-high visit floor -> fail.
        let gt = GraphGt {
            baselines: vec![BaselineExpect { subject: eref("person", "Alice"), min_visits: Some(7), peak_hour_of_day: None }],
            ..Default::default()
        };
        assert!(!find(&score_graph(&gt, &o), "graph.baseline.person.alice").floor_ok);
        // Wrong peak hour -> fail.
        let gt = GraphGt {
            baselines: vec![BaselineExpect { subject: eref("person", "Alice"), min_visits: None, peak_hour_of_day: Some(3) }],
            ..Default::default()
        };
        assert!(!find(&score_graph(&gt, &o), "graph.baseline.person.alice").floor_ok);
    }
}
