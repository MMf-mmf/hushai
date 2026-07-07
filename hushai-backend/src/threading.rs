//! Pure conversation-threading core (migration 0025) — side-effect-free and deterministic.
//!
//! Clusters one device's transcript sentences into conversations. Industry-standard
//! *conversation disentanglement* (Elsner–Charniak / Kummerfeld-style pairwise
//! same-conversation evidence + graph partition), adapted from chat text to diarized
//! speech and layered on the signals the pipeline already persists:
//!
//!   time  — a silence gap > `gap_secs` is a hard conversation boundary (stage B);
//!   voice — speaker turn-taking: reply-shaped adjacency between two speakers is
//!           evidence they are conversing (stage C);
//!   topic — the 1024-d sentence embeddings: sustained topical affinity binds a
//!           speaker pair, sustained divergence repels (stage C);
//!   ids   — clusters inherit the conversation_id already carried by the plurality of
//!           their previously-assigned sentences, so re-running a pass is a no-op
//!           diff and open-tail revisions never churn ids gratuitously (stage E).
//!
//! DETERMINISM CONTRACT (the eval config-hash gate depends on it): every sort has a
//! total tie-break on `(start_unix_nanos, id)`, every threshold comes from
//! [`ThreaderCfg`] (hashed by [`config_hash`]), and id minting is injected by the
//! caller — same input + same config + same mint sequence ⇒ byte-identical output.
//! No clock, no RNG, no DB, no LLM in this module.
//!
//! What this deliberately does NOT solve (documented in AGENTS.md): two groups
//! talking about the SAME topic, interleaved on ONE mono mic, are
//! information-theoretically indistinguishable without spatial audio; acoustically
//! overlapped 2s segments arrive here with `speaker_id NULL` (the multi-speaker
//! refusal in the worker) and are topic-attached as orphans — a safe degradation,
//! never a speaker misattribution.

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use uuid::Uuid;

const NANOS_PER_SEC: f64 = 1_000_000_000.0;

/// All threader knobs. Every field participates in [`config_hash`]; changing any of
/// them starts a new eval lineage. Defaults mirror `.env.example`.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreaderCfg {
    /// Hard temporal boundary: a silence gap longer than this always starts a new
    /// conversation (shared truth with CONVERSATION_GAP_SECS in rag/profiles).
    pub gap_secs: f64,
    /// Stage A: consecutive same-speaker sentences closer than this merge into one utterance.
    pub utterance_merge_max_gap_secs: f64,
    /// Stage C: an utterance by `a` followed by `b` within this gap is a reply-shaped
    /// adjacency event.
    pub alternation_max_secs: f64,
    /// Stage C: adjacency counts full weight (1.0) when the two utterances' cosine
    /// similarity clears this floor; below it, raw adjacency is weak evidence (0.25) —
    /// interleaved groups on one mic alternate too.
    pub reply_sim_floor: f32,
    /// Stage C: mean pairwise topic similarity ≥ this adds +2.0 to the speaker-pair edge.
    pub topic_attract_sim: f32,
    /// Stage C: mean pairwise topic similarity ≤ this adds −2.0.
    pub topic_repel_sim: f32,
    /// Stage C: minimum accumulated edge weight for "these two speakers are conversing".
    /// 2.0 means a single stray cross-talk adjacency (≤1.0) can never fuse two groups.
    pub speaker_link_min: f32,
    /// Split gate: every candidate sub-conversation must have at least this many utterances.
    pub min_cluster_utterances: usize,
    /// Split gate: mean cross-component similarity must be at or below this to accept a split.
    pub split_max_cross_sim: f32,
    /// When false (default), a block whose candidate components do not temporally
    /// interleave is NEVER split — sequential topic drift within one gap block is one
    /// conversation, not two.
    pub topic_only_split: bool,
}

impl Default for ThreaderCfg {
    fn default() -> Self {
        Self {
            gap_secs: 300.0,
            utterance_merge_max_gap_secs: 1.0,
            alternation_max_secs: 5.0,
            reply_sim_floor: 0.45,
            topic_attract_sim: 0.60,
            topic_repel_sim: 0.35,
            speaker_link_min: 2.0,
            min_cluster_utterances: 4,
            split_max_cross_sim: 0.40,
            topic_only_split: false,
        }
    }
}

/// Stable fingerprint of the knob set (hex, 16 chars). Stored on `threader_state` and on
/// every conversation row the pass closes; the eval manifest folds the same env knobs, so
/// a knob change shows up as a new lineage on both sides.
pub fn config_hash(cfg: &ThreaderCfg) -> String {
    let canonical = format!(
        "gap={:.3};umerge={:.3};alt={:.3};reply={:.4};attract={:.4};repel={:.4};link={:.4};minutt={};xsim={:.4};toposplit={}",
        cfg.gap_secs,
        cfg.utterance_merge_max_gap_secs,
        cfg.alternation_max_secs,
        cfg.reply_sim_floor,
        cfg.topic_attract_sim,
        cfg.topic_repel_sim,
        cfg.speaker_link_min,
        cfg.min_cluster_utterances,
        cfg.split_max_cross_sim,
        cfg.topic_only_split,
    );
    let digest = Sha256::digest(canonical.as_bytes());
    hex::encode(&digest[..8])
}

/// One transcript sentence as the threader sees it (one device's timeline only).
/// `embedding` is the 1024-d text vector, already L2-normalized at write time; None for
/// rows written without an embedding. `prior_conversation_id` is the current assignment
/// (None = unassigned), used only for stable-id reconciliation.
#[derive(Debug, Clone)]
pub struct SentenceIn {
    pub id: i64,
    pub speaker_id: Option<Uuid>,
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
    pub embedding: Option<Vec<f32>>,
    pub prior_conversation_id: Option<Uuid>,
}

/// New assignment for one sentence. The orchestrator diffs against the prior state and
/// UPDATEs only changed rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentOut {
    pub sentence_id: i64,
    pub conversation_id: Uuid,
    pub turn_index: i32,
}

/// One computed conversation (bounds + participants recomputed from members every pass).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationOut {
    pub conversation_id: Uuid,
    pub started_at_unix_nanos: i64,
    pub ended_at_unix_nanos: i64,
    /// Distinct non-NULL speakers, sorted (deterministic array for the catalog row).
    pub speaker_ids: Vec<Uuid>,
    pub sentence_count: i64,
    /// True when stage E minted the id this pass (row must be INSERTed, not UPDATEd).
    pub minted: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ThreadResult {
    pub assignments: Vec<AssignmentOut>,
    pub conversations: Vec<ConversationOut>,
}

// ---------------------------------------------------------------------------------------------
// Stage A — utterances
// ---------------------------------------------------------------------------------------------

/// A run of consecutive same-speaker sentences (gap ≤ utterance_merge_max_gap_secs),
/// the unit of stage B–D work. Embedding = L2-normalized mean of member embeddings.
#[derive(Debug, Clone)]
struct Utterance {
    /// Indices into the (sorted) sentence slice.
    members: Vec<usize>,
    speaker_id: Option<Uuid>,
    start_ns: i64,
    end_ns: i64,
    embedding: Option<Vec<f32>>,
}

fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Cosine similarity of two already-normalized vectors; None on missing/mismatched input.
fn cos_sim(a: Option<&Vec<f32>>, b: Option<&Vec<f32>>) -> Option<f32> {
    let (a, b) = (a?, b?);
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    Some(a.iter().zip(b.iter()).map(|(x, y)| x * y).sum())
}

fn build_utterances(sentences: &[SentenceIn], cfg: &ThreaderCfg) -> Vec<Utterance> {
    let max_gap_ns = (cfg.utterance_merge_max_gap_secs * NANOS_PER_SEC) as i64;
    let mut utterances: Vec<Utterance> = Vec::new();
    for (idx, s) in sentences.iter().enumerate() {
        let merged = match utterances.last_mut() {
            Some(u)
                if u.speaker_id == s.speaker_id
                    && s.start_unix_nanos.saturating_sub(u.end_ns) <= max_gap_ns =>
            {
                u.members.push(idx);
                u.end_ns = u.end_ns.max(s.end_unix_nanos);
                true
            }
            _ => false,
        };
        if !merged {
            utterances.push(Utterance {
                members: vec![idx],
                speaker_id: s.speaker_id,
                start_ns: s.start_unix_nanos,
                end_ns: s.end_unix_nanos,
                embedding: None,
            });
        }
    }
    // Mean-then-renormalize member embeddings (missing members simply don't contribute).
    for u in &mut utterances {
        let vecs: Vec<&Vec<f32>> = u
            .members
            .iter()
            .filter_map(|&i| sentences[i].embedding.as_ref())
            .collect();
        if let Some(first) = vecs.first() {
            let mut mean = vec![0.0f32; first.len()];
            let mut n = 0usize;
            for v in &vecs {
                if v.len() == mean.len() {
                    for (m, x) in mean.iter_mut().zip(v.iter()) {
                        *m += x;
                    }
                    n += 1;
                }
            }
            if n > 0 {
                for m in mean.iter_mut() {
                    *m /= n as f32;
                }
                l2_normalize(&mut mean);
                u.embedding = Some(mean);
            }
        }
    }
    utterances
}

// ---------------------------------------------------------------------------------------------
// Stage B — temporal blocks
// ---------------------------------------------------------------------------------------------

/// Split the utterance sequence on hard silence gaps. Returns ranges into the utterance vec.
fn split_temporal_blocks(utterances: &[Utterance], cfg: &ThreaderCfg) -> Vec<std::ops::Range<usize>> {
    let gap_ns = (cfg.gap_secs * NANOS_PER_SEC) as i64;
    let mut blocks = Vec::new();
    let mut block_start = 0usize;
    for i in 1..utterances.len() {
        if utterances[i].start_ns.saturating_sub(utterances[i - 1].end_ns) > gap_ns {
            blocks.push(block_start..i);
            block_start = i;
        }
    }
    if block_start < utterances.len() {
        blocks.push(block_start..utterances.len());
    }
    blocks
}

// ---------------------------------------------------------------------------------------------
// Stage C — disentanglement within a block (speaker-pair graph + split gate)
// ---------------------------------------------------------------------------------------------

/// Union-find over speaker indices (tiny n; path halving is plenty).
struct Dsu(Vec<usize>);
impl Dsu {
    fn new(n: usize) -> Self {
        Dsu((0..n).collect())
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.0[x] != x {
            self.0[x] = self.0[self.0[x]];
            x = self.0[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            // Deterministic: smaller root wins.
            let (lo, hi) = (ra.min(rb), ra.max(rb));
            self.0[hi] = lo;
        }
    }
}

/// Partition a block's utterance indices into candidate sub-conversations. Returns one or
/// more clusters, each a sorted list of utterance indices (into the full utterance vec).
fn disentangle_block(
    utterances: &[Utterance],
    block: std::ops::Range<usize>,
    cfg: &ThreaderCfg,
) -> Vec<Vec<usize>> {
    let idxs: Vec<usize> = block.collect();
    let one_cluster = |idxs: &[usize]| vec![idxs.to_vec()];

    // Distinct speakers in first-appearance order (deterministic node numbering).
    let mut speakers: Vec<Uuid> = Vec::new();
    for &i in &idxs {
        if let Some(sp) = utterances[i].speaker_id {
            if !speakers.contains(&sp) {
                speakers.push(sp);
            }
        }
    }
    if speakers.len() < 2 {
        return one_cluster(&idxs);
    }
    let speaker_pos: HashMap<Uuid, usize> =
        speakers.iter().enumerate().map(|(p, s)| (*s, p)).collect();

    // Edge weights: reply-shaped adjacency + per-pair topic affinity.
    let n = speakers.len();
    let mut weight = vec![0.0f32; n * n];
    let alternation_ns = (cfg.alternation_max_secs * NANOS_PER_SEC) as i64;
    for w in idxs.windows(2) {
        let (a, b) = (&utterances[w[0]], &utterances[w[1]]);
        let (Some(sa), Some(sb)) = (a.speaker_id, b.speaker_id) else {
            continue;
        };
        if sa == sb || b.start_ns.saturating_sub(a.end_ns) > alternation_ns {
            continue;
        }
        let sim = cos_sim(a.embedding.as_ref(), b.embedding.as_ref());
        let ev = match sim {
            Some(s) if s >= cfg.reply_sim_floor => 1.0,
            _ => 0.25,
        };
        let (pa, pb) = (speaker_pos[&sa], speaker_pos[&sb]);
        weight[pa * n + pb] += ev;
        weight[pb * n + pa] += ev;
    }
    // Topic affinity: mean pairwise sim between each speaker pair's utterances.
    let by_speaker: Vec<Vec<usize>> = speakers
        .iter()
        .map(|sp| {
            idxs.iter()
                .copied()
                .filter(|&i| utterances[i].speaker_id == Some(*sp))
                .collect()
        })
        .collect();
    for pa in 0..n {
        for pb in (pa + 1)..n {
            let mut sum = 0.0f32;
            let mut cnt = 0usize;
            for &ia in &by_speaker[pa] {
                for &ib in &by_speaker[pb] {
                    if let Some(s) =
                        cos_sim(utterances[ia].embedding.as_ref(), utterances[ib].embedding.as_ref())
                    {
                        sum += s;
                        cnt += 1;
                    }
                }
            }
            if cnt > 0 {
                let mean = sum / cnt as f32;
                let bonus = if mean >= cfg.topic_attract_sim {
                    2.0
                } else if mean <= cfg.topic_repel_sim {
                    -2.0
                } else {
                    0.0
                };
                weight[pa * n + pb] += bonus;
                weight[pb * n + pa] += bonus;
            }
        }
    }

    // Connected components over edges clearing the link floor.
    let mut dsu = Dsu::new(n);
    for pa in 0..n {
        for pb in (pa + 1)..n {
            if weight[pa * n + pb] >= cfg.speaker_link_min {
                dsu.union(pa, pb);
            }
        }
    }
    let mut comp_of_speaker = vec![0usize; n];
    let mut roots: Vec<usize> = Vec::new();
    for p in 0..n {
        let r = dsu.find(p);
        let c = match roots.iter().position(|&x| x == r) {
            Some(c) => c,
            None => {
                roots.push(r);
                roots.len() - 1
            }
        };
        comp_of_speaker[p] = c;
    }
    let ncomp = roots.len();
    if ncomp < 2 {
        return one_cluster(&idxs);
    }

    // Candidate clusters: speaker-attributed utterances only (orphans attach in stage D).
    let mut clusters: Vec<Vec<usize>> = vec![Vec::new(); ncomp];
    for &i in &idxs {
        if let Some(sp) = utterances[i].speaker_id {
            clusters[comp_of_speaker[speaker_pos[&sp]]].push(i);
        }
    }
    clusters.retain(|c| !c.is_empty());
    if clusters.len() < 2 {
        return one_cluster(&idxs);
    }

    // --- Split gate (conservatism/hysteresis) ---
    // (1) size floor per component;
    if clusters.iter().any(|c| c.len() < cfg.min_cluster_utterances) {
        return one_cluster(&idxs);
    }
    // (2) temporal interleave: every pair of components' [start,end] spans must overlap —
    //     otherwise the block is sequential topic drift, one conversation (unless the
    //     operator opted into topic-only splitting);
    if !cfg.topic_only_split {
        let spans: Vec<(i64, i64)> = clusters
            .iter()
            .map(|c| {
                let lo = c.iter().map(|&i| utterances[i].start_ns).min().unwrap();
                let hi = c.iter().map(|&i| utterances[i].end_ns).max().unwrap();
                (lo, hi)
            })
            .collect();
        for a in 0..spans.len() {
            for b in (a + 1)..spans.len() {
                if spans[a].0 >= spans[b].1 || spans[b].0 >= spans[a].1 {
                    return one_cluster(&idxs);
                }
            }
        }
    }
    // (3) topical divergence: mean cross-component similarity must be low.
    let mut cross_sum = 0.0f32;
    let mut cross_cnt = 0usize;
    for a in 0..clusters.len() {
        for b in (a + 1)..clusters.len() {
            for &ia in &clusters[a] {
                for &ib in &clusters[b] {
                    if let Some(s) =
                        cos_sim(utterances[ia].embedding.as_ref(), utterances[ib].embedding.as_ref())
                    {
                        cross_sum += s;
                        cross_cnt += 1;
                    }
                }
            }
        }
    }
    if cross_cnt > 0 && cross_sum / cross_cnt as f32 > cfg.split_max_cross_sim {
        return one_cluster(&idxs);
    }

    // Accepted split. Attach NULL-speaker orphans (stage D), then order clusters by
    // earliest utterance for deterministic downstream numbering.
    let clustered: Vec<Vec<usize>> = clusters;
    let orphans: Vec<usize> = idxs
        .iter()
        .copied()
        .filter(|&i| utterances[i].speaker_id.is_none())
        .collect();
    let mut out = clustered;
    assign_orphans(utterances, &orphans, &mut out);
    for c in &mut out {
        c.sort_unstable();
    }
    out.sort_by_key(|c| {
        let first = c[0];
        (utterances[first].start_ns, first)
    });
    out
}

// ---------------------------------------------------------------------------------------------
// Stage D — orphan assignment
// ---------------------------------------------------------------------------------------------

/// Attach unattributed utterances to the cluster maximizing
/// `0.6 * topic_sim_to_centroid + 0.4 * temporal_proximity`. Deterministic tie-break:
/// lower cluster index (clusters are built in first-speaker-appearance order).
fn assign_orphans(utterances: &[Utterance], orphans: &[usize], clusters: &mut [Vec<usize>]) {
    for &o in orphans {
        let u = &utterances[o];
        let mut best: (f32, usize) = (f32::NEG_INFINITY, 0);
        for (ci, cluster) in clusters.iter().enumerate() {
            // Cluster centroid: mean of member embeddings, renormalized.
            let vecs: Vec<&Vec<f32>> = cluster
                .iter()
                .filter_map(|&i| utterances[i].embedding.as_ref())
                .collect();
            let topic = if let (Some(first), Some(ue)) = (vecs.first(), u.embedding.as_ref()) {
                let mut mean = vec![0.0f32; first.len()];
                let mut n = 0usize;
                for v in &vecs {
                    if v.len() == mean.len() {
                        for (m, x) in mean.iter_mut().zip(v.iter()) {
                            *m += x;
                        }
                        n += 1;
                    }
                }
                if n > 0 {
                    for m in mean.iter_mut() {
                        *m /= n as f32;
                    }
                    l2_normalize(&mut mean);
                    cos_sim(Some(&mean), Some(ue)).unwrap_or(0.0)
                } else {
                    0.0
                }
            } else {
                0.0
            };
            let min_gap_secs = cluster
                .iter()
                .map(|&i| {
                    let c = &utterances[i];
                    if c.end_ns < u.start_ns {
                        (u.start_ns - c.end_ns) as f64 / NANOS_PER_SEC
                    } else if u.end_ns < c.start_ns {
                        (c.start_ns - u.end_ns) as f64 / NANOS_PER_SEC
                    } else {
                        0.0
                    }
                })
                .fold(f64::INFINITY, f64::min);
            let proximity = (1.0 / (1.0 + min_gap_secs)) as f32;
            let score = 0.6 * topic + 0.4 * proximity;
            if score > best.0 {
                best = (score, ci);
            }
        }
        clusters[best.1].push(o);
    }
}

// ---------------------------------------------------------------------------------------------
// Stage E — stable id reconciliation
// ---------------------------------------------------------------------------------------------

/// Give each cluster a conversation_id: inherit the plurality prior id of its member
/// sentences (each id claimed by at most one cluster — the strongest claim wins), else
/// mint. `mint` is injected so the core stays deterministic under test.
fn reconcile_ids(
    sentences: &[SentenceIn],
    utterances: &[Utterance],
    clusters: &[Vec<usize>],
    mint: &mut dyn FnMut() -> Uuid,
) -> Vec<(Uuid, bool)> {
    // Per cluster: count prior ids over member SENTENCES (not utterances).
    let counts: Vec<HashMap<Uuid, usize>> = clusters
        .iter()
        .map(|cluster| {
            let mut m: HashMap<Uuid, usize> = HashMap::new();
            for &ui in cluster {
                for &si in &utterances[ui].members {
                    if let Some(pid) = sentences[si].prior_conversation_id {
                        *m.entry(pid).or_default() += 1;
                    }
                }
            }
            m
        })
        .collect();

    // Claims sorted by (count DESC, conversation_id ASC, cluster index ASC) — greedy
    // unique assignment, fully deterministic.
    let mut claims: Vec<(usize, Uuid, usize)> = Vec::new();
    for (ci, m) in counts.iter().enumerate() {
        for (id, cnt) in m {
            claims.push((*cnt, *id, ci));
        }
    }
    claims.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));

    let mut assigned: Vec<Option<Uuid>> = vec![None; clusters.len()];
    let mut used: Vec<Uuid> = Vec::new();
    for (_, id, ci) in claims {
        if assigned[ci].is_none() && !used.contains(&id) {
            assigned[ci] = Some(id);
            used.push(id);
        }
    }
    assigned
        .into_iter()
        .map(|a| match a {
            Some(id) => (id, false),
            None => (mint(), true),
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Entry point (stages A → F)
// ---------------------------------------------------------------------------------------------

/// Thread ONE device's sentence set. `sentences` may arrive unsorted; all clustering,
/// id reconciliation, and turn indexing happen here. The caller supplies `mint`
/// (production: `Uuid::now_v7`; tests: a counter) and applies the result transactionally.
pub fn thread_device(
    mut sentences: Vec<SentenceIn>,
    cfg: &ThreaderCfg,
    mint: &mut dyn FnMut() -> Uuid,
) -> ThreadResult {
    sentences.sort_by_key(|s| (s.start_unix_nanos, s.id));
    if sentences.is_empty() {
        return ThreadResult::default();
    }

    let utterances = build_utterances(&sentences, cfg);
    let blocks = split_temporal_blocks(&utterances, cfg);

    // Collect clusters across all blocks (a block yields 1..n clusters).
    let mut all_clusters: Vec<Vec<usize>> = Vec::new();
    for block in blocks {
        all_clusters.extend(disentangle_block(&utterances, block, cfg));
    }

    let ids = reconcile_ids(&sentences, &utterances, &all_clusters, mint);

    let mut result = ThreadResult::default();
    for (cluster, (conversation_id, minted)) in all_clusters.iter().zip(ids) {
        // Member sentence indices in time order.
        let mut member_sentences: Vec<usize> = cluster
            .iter()
            .flat_map(|&ui| utterances[ui].members.iter().copied())
            .collect();
        member_sentences.sort_by_key(|&si| (sentences[si].start_unix_nanos, sentences[si].id));

        // Stage F: turn_index increments on non-NULL speaker change; NULL inherits.
        let mut turn = 0i32;
        let mut last_speaker: Option<Uuid> = None;
        for &si in &member_sentences {
            if let Some(sp) = sentences[si].speaker_id {
                if let Some(prev) = last_speaker {
                    if prev != sp {
                        turn += 1;
                    }
                }
                last_speaker = Some(sp);
            }
            result.assignments.push(AssignmentOut {
                sentence_id: sentences[si].id,
                conversation_id,
                turn_index: turn,
            });
        }

        let started = member_sentences
            .iter()
            .map(|&si| sentences[si].start_unix_nanos)
            .min()
            .unwrap_or(0);
        let ended = member_sentences
            .iter()
            .map(|&si| sentences[si].end_unix_nanos)
            .max()
            .unwrap_or(0);
        let mut speaker_ids: Vec<Uuid> = member_sentences
            .iter()
            .filter_map(|&si| sentences[si].speaker_id)
            .collect();
        speaker_ids.sort();
        speaker_ids.dedup();

        result.conversations.push(ConversationOut {
            conversation_id,
            started_at_unix_nanos: started,
            ended_at_unix_nanos: ended,
            speaker_ids,
            sentence_count: member_sentences.len() as i64,
            minted,
        });
    }
    // Deterministic output order.
    result
        .conversations
        .sort_by_key(|c| (c.started_at_unix_nanos, c.conversation_id));
    result
        .assignments
        .sort_by_key(|a| a.sentence_id);
    result
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: i64 = 1_000_000_000;

    fn sp(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    /// Deterministic mint: 0xA000..., 0xA001..., ...
    fn minter() -> impl FnMut() -> Uuid {
        let mut n: u128 = 0xA000;
        move || {
            n += 1;
            Uuid::from_u128(n)
        }
    }

    /// Unit topic vectors: topic 0 = e0, topic 1 = e1, ... (orthogonal — cos 0).
    fn topic(t: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; 8];
        v[t] = 1.0;
        v
    }

    fn sent(
        id: i64,
        speaker: Option<u128>,
        start_s: i64,
        end_s: i64,
        t: Option<usize>,
        prior: Option<u128>,
    ) -> SentenceIn {
        SentenceIn {
            id,
            speaker_id: speaker.map(sp),
            start_unix_nanos: start_s * SEC,
            end_unix_nanos: end_s * SEC,
            embedding: t.map(topic),
            prior_conversation_id: prior.map(Uuid::from_u128),
        }
    }

    /// Two speakers alternating, one topic, one conversation. 8 sentences over 40s.
    fn dialogue(base_id: i64, start_s: i64, spk_a: u128, spk_b: u128, t: usize) -> Vec<SentenceIn> {
        (0..8)
            .map(|i| {
                let speaker = if i % 2 == 0 { spk_a } else { spk_b };
                sent(
                    base_id + i,
                    Some(speaker),
                    start_s + i * 5,
                    start_s + i * 5 + 3,
                    Some(t),
                    None,
                )
            })
            .collect()
    }

    fn distinct_convo_count(r: &ThreadResult) -> usize {
        r.conversations.len()
    }

    #[test]
    fn empty_input_is_empty() {
        let r = thread_device(vec![], &ThreaderCfg::default(), &mut minter());
        assert!(r.assignments.is_empty() && r.conversations.is_empty());
    }

    #[test]
    fn monologue_is_one_conversation() {
        let rows: Vec<SentenceIn> =
            (0..5).map(|i| sent(i, Some(1), i * 4, i * 4 + 3, Some(0), None)).collect();
        let r = thread_device(rows, &ThreaderCfg::default(), &mut minter());
        assert_eq!(distinct_convo_count(&r), 1);
        assert_eq!(r.conversations[0].sentence_count, 5);
        assert_eq!(r.conversations[0].speaker_ids, vec![sp(1)]);
    }

    #[test]
    fn gap_over_threshold_splits() {
        let mut rows = dialogue(0, 0, 1, 2, 0);
        rows.extend(dialogue(100, 600, 1, 2, 1)); // +600s ≫ 300s gap
        let r = thread_device(rows, &ThreaderCfg::default(), &mut minter());
        assert_eq!(distinct_convo_count(&r), 2);
    }

    #[test]
    fn pause_within_gap_stays_one() {
        let mut rows = dialogue(0, 0, 1, 2, 0);
        rows.extend(dialogue(100, 100, 1, 2, 0)); // resumes after ~60s pause < 300s
        let r = thread_device(rows, &ThreaderCfg::default(), &mut minter());
        assert_eq!(distinct_convo_count(&r), 1);
    }

    #[test]
    fn interleaved_disjoint_groups_split() {
        // Groups (1,2) topic 0 and (3,4) topic 1, strictly interleaved on one mic:
        // A1 B1 A2 B2 ... Each group's utterances alternate within its turns.
        let mut rows = Vec::new();
        let mut id = 0i64;
        for round in 0..4 {
            let t0 = round * 24;
            // Group A turn (two sentences, speakers 1 then 2).
            rows.push(sent(id, Some(1), t0, t0 + 4, Some(0), None));
            id += 1;
            rows.push(sent(id, Some(2), t0 + 5, t0 + 9, Some(0), None));
            id += 1;
            // Group B turn (speakers 3 then 4).
            rows.push(sent(id, Some(3), t0 + 12, t0 + 16, Some(1), None));
            id += 1;
            rows.push(sent(id, Some(4), t0 + 17, t0 + 21, Some(1), None));
            id += 1;
        }
        let r = thread_device(rows.clone(), &ThreaderCfg::default(), &mut minter());
        assert_eq!(distinct_convo_count(&r), 2, "disjoint interleaved groups must split");
        // No cross-group contamination: speakers 1,2 in one conversation, 3,4 in the other.
        for c in &r.conversations {
            let has_a = c.speaker_ids.contains(&sp(1)) || c.speaker_ids.contains(&sp(2));
            let has_b = c.speaker_ids.contains(&sp(3)) || c.speaker_ids.contains(&sp(4));
            assert!(has_a ^ has_b, "groups merged: {:?}", c.speaker_ids);
        }
    }

    #[test]
    fn sequential_topic_drift_does_not_split() {
        // Same two speakers, topic changes mid-block, NO temporal interleave -> one convo.
        let mut rows = dialogue(0, 0, 1, 2, 0);
        rows.extend(dialogue(100, 45, 1, 2, 1)); // 5s after first block ends, new topic
        let r = thread_device(rows, &ThreaderCfg::default(), &mut minter());
        assert_eq!(distinct_convo_count(&r), 1, "topic drift alone must not split");
    }

    #[test]
    fn small_components_refuse_split() {
        // Interleaved groups but each has only 3 utterances < min_cluster_utterances=4.
        let mut rows = Vec::new();
        let mut id = 0i64;
        for round in 0..3 {
            let t0 = round * 20;
            rows.push(sent(id, Some(1), t0, t0 + 4, Some(0), None));
            id += 1;
            rows.push(sent(id, Some(3), t0 + 10, t0 + 14, Some(1), None));
            id += 1;
        }
        let r = thread_device(rows, &ThreaderCfg::default(), &mut minter());
        assert_eq!(distinct_convo_count(&r), 1, "size floor must refuse the split");
    }

    #[test]
    fn null_speaker_orphans_attach_by_topic() {
        // Two interleaved groups + one NULL-speaker sentence topically on topic 1.
        let mut rows = Vec::new();
        let mut id = 0i64;
        for round in 0..4 {
            let t0 = round * 24;
            rows.push(sent(id, Some(1), t0, t0 + 4, Some(0), None));
            id += 1;
            rows.push(sent(id, Some(2), t0 + 5, t0 + 9, Some(0), None));
            id += 1;
            rows.push(sent(id, Some(3), t0 + 12, t0 + 16, Some(1), None));
            id += 1;
            rows.push(sent(id, Some(4), t0 + 17, t0 + 21, Some(1), None));
            id += 1;
        }
        rows.push(sent(500, None, 40, 42, Some(1), None)); // orphan, topic 1
        let r = thread_device(rows, &ThreaderCfg::default(), &mut minter());
        assert_eq!(distinct_convo_count(&r), 2);
        let orphan_convo = r
            .assignments
            .iter()
            .find(|a| a.sentence_id == 500)
            .expect("orphan assigned")
            .conversation_id;
        let convo_b = r
            .conversations
            .iter()
            .find(|c| c.speaker_ids.contains(&sp(3)))
            .unwrap();
        assert_eq!(orphan_convo, convo_b.conversation_id, "orphan must join the topic-1 group");
    }

    #[test]
    fn rerun_with_prior_ids_is_stable_and_mints_nothing() {
        let rows = dialogue(0, 0, 1, 2, 0);
        let first = thread_device(rows.clone(), &ThreaderCfg::default(), &mut minter());
        let cid = first.conversations[0].conversation_id;
        assert!(first.conversations[0].minted);

        let rows2: Vec<SentenceIn> = rows
            .into_iter()
            .map(|mut s| {
                s.prior_conversation_id = Some(cid);
                s
            })
            .collect();
        let mut panic_mint = || -> Uuid { panic!("must not mint on a stable rerun") };
        let second = thread_device(rows2, &ThreaderCfg::default(), &mut panic_mint);
        assert_eq!(second.conversations[0].conversation_id, cid);
        assert!(!second.conversations[0].minted);
        assert_eq!(first.assignments, second.assignments);
    }

    #[test]
    fn plurality_id_conflict_resolves_uniquely() {
        // Two gap-separated conversations whose sentences ALL carry the same prior id
        // (e.g. the gap knob was tightened). The larger block keeps the id; the other mints.
        let prior = Uuid::from_u128(0xBEEF);
        let mut rows: Vec<SentenceIn> = dialogue(0, 0, 1, 2, 0)
            .into_iter()
            .map(|mut s| {
                s.prior_conversation_id = Some(prior);
                s
            })
            .collect();
        // Second block: fewer sentences, same prior.
        let mut tail: Vec<SentenceIn> = dialogue(100, 600, 1, 2, 1).into_iter().take(4).collect();
        for s in &mut tail {
            s.prior_conversation_id = Some(prior);
        }
        rows.extend(tail);
        let r = thread_device(rows, &ThreaderCfg::default(), &mut minter());
        assert_eq!(distinct_convo_count(&r), 2);
        let ids: Vec<Uuid> = r.conversations.iter().map(|c| c.conversation_id).collect();
        assert_ne!(ids[0], ids[1], "one id must not cover two conversations");
        // The larger (first, 8-sentence) block inherits; the 4-sentence block minted.
        let keeper = r.conversations.iter().find(|c| c.sentence_count == 8).unwrap();
        let minted = r.conversations.iter().find(|c| c.sentence_count == 4).unwrap();
        assert_eq!(keeper.conversation_id, prior);
        assert!(!keeper.minted);
        assert!(minted.minted);
    }

    #[test]
    fn turn_index_increments_on_speaker_change_and_null_inherits() {
        let rows = vec![
            sent(0, Some(1), 0, 2, Some(0), None),
            sent(1, Some(1), 3, 5, Some(0), None), // same speaker, same turn
            sent(2, None, 6, 7, Some(0), None),    // NULL inherits turn 0
            sent(3, Some(2), 8, 10, Some(0), None), // speaker change -> turn 1
            sent(4, Some(1), 11, 13, Some(0), None), // back -> turn 2
        ];
        let r = thread_device(rows, &ThreaderCfg::default(), &mut minter());
        let turn_of = |id: i64| {
            r.assignments
                .iter()
                .find(|a| a.sentence_id == id)
                .unwrap()
                .turn_index
        };
        assert_eq!(turn_of(0), 0);
        assert_eq!(turn_of(1), 0);
        assert_eq!(turn_of(2), 0);
        assert_eq!(turn_of(3), 1);
        assert_eq!(turn_of(4), 2);
    }

    #[test]
    fn determinism_same_input_same_output() {
        let mut rows = dialogue(0, 0, 1, 2, 0);
        rows.extend(dialogue(100, 600, 3, 4, 1));
        rows.push(sent(500, None, 610, 612, Some(1), None));
        let a = thread_device(rows.clone(), &ThreaderCfg::default(), &mut minter());
        let b = thread_device(rows, &ThreaderCfg::default(), &mut minter());
        assert_eq!(a.assignments, b.assignments);
        assert_eq!(a.conversations, b.conversations);
    }

    #[test]
    fn all_null_speaker_block_is_one_conversation() {
        // The physical Tier-2 reality: room audio mints zero speakers.
        let rows: Vec<SentenceIn> =
            (0..6).map(|i| sent(i, None, i * 4, i * 4 + 3, Some(0), None)).collect();
        let r = thread_device(rows, &ThreaderCfg::default(), &mut minter());
        assert_eq!(distinct_convo_count(&r), 1);
        assert!(r.conversations[0].speaker_ids.is_empty());
        assert_eq!(r.conversations[0].sentence_count, 6);
    }

    #[test]
    fn config_hash_changes_with_knobs() {
        let a = config_hash(&ThreaderCfg::default());
        let b = config_hash(&ThreaderCfg {
            gap_secs: 90.0,
            ..ThreaderCfg::default()
        });
        assert_ne!(a, b);
        assert_eq!(a, config_hash(&ThreaderCfg::default()));
    }
}
