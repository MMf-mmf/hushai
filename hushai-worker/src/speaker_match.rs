//! Online, global, serialized, idempotent speaker match-or-mint — executed INSIDE the
//! `write_transcript` transaction so the idempotency read, the centroid recompute, and the
//! transcript write all share one lock and one commit.
//!
//! The duplicate-voice problem this guards against: a single drifting running-mean centroid
//! plus "mint on any miss" fragments one person (especially under static) into many
//! "unknown speaker" rows. The fix here is threefold:
//!
//!   1. MULTI-VECTOR k-NN. Match against the k nearest RAW embeddings in `speaker_segments`
//!      (a voice's natural spread), not one centroid. A confident plurality vote wins. The
//!      centroid is kept only as a cheap fallback (cold-start; or after retention drops the
//!      raw vectors).
//!   2. MINT-GUARD HYSTERESIS. Two thresholds: at/under `match_threshold` is a confident
//!      same-speaker; beyond `mint_distance_floor` is a new identity; the gray zone in
//!      between ATTACHES to the nearest existing speaker and never mints. Crucially, a NEW
//!      identity may only be born from CLEAN audio (the quality flag from the VAD stage) —
//!      marginal/noisy audio that is far from everyone yields NULL, never a duplicate.
//!   3. SELF-HEALING CENTROID. The centroid is recomputed from the most recent N *clean*
//!      segments, so a bad fold ages out instead of permanently corrupting the running mean.
//!
//! Ordering (correctness-critical):
//!   1. `pg_advisory_xact_lock(KEY)` — single GLOBAL lock (identity is cross-device).
//!   2. Idempotency: if a `speaker_segments` row already exists for this segment, reuse its
//!      exact outcome (Some(id) or NULL) and touch nothing, so reprocessing is byte-identical.
//!   3. `SET LOCAL` HNSW/timeout GUCs (tx-scoped, never leak onto the pooled connection).
//!   4. k-NN vote -> centroid fallback -> hysteresis decision -> match / attach / mint / NULL.
//!   5. UPSERT the raw 192-d vector to `speaker_segments` (delete-then-insert; partitioned
//!      table can't carry a UNIQUE(segment_id)), tagging quality = 'clean' only for rows that
//!      should feed the centroid (match/mint), 'marginal' otherwise.
//!   6. On match/mint, recompute the centroid from recent clean rows.

use anyhow::Context;
use sqlx::{AssertSqlSafe, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::vad::SpeakerQuality;

/// Constant key for the global speaker-space advisory lock. Distinctive so it cannot
/// collide with any other advisory lock the system might add later. Shared with the
/// backend's merge/recluster endpoints.
const SPEAKER_LOCK_KEY: i64 = 0x6873_7370_6b72; // "hsspkr"

/// Tuning for `assign_speaker` (bundled so the call takes one borrow, not eight args).
#[derive(Debug, Clone, Copy)]
pub struct SpeakerMatchConfig {
    pub match_threshold: f32,
    pub mint_distance_floor: f32,
    pub knn_k: i64,
    pub knn_neighbor_ceiling: f32,
    pub knn_min_neighbors: i64,
    pub knn_ef_search: i64,
    pub knn_statement_timeout_ms: i64,
    pub centroid_window: i64,
}

/// A computed speaker embedding for one segment, ready to assign + persist.
pub struct SpeakerWrite {
    /// L2-normalized 192-d embedding of the segment's voiced speech.
    pub embedding: Vec<f32>,
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
    /// Input-audio quality from the VAD/SNR stage. Only `Mint` may create a new identity.
    /// (`Reject` never reaches here — the caller leaves speaker_id NULL upstream.)
    pub quality: SpeakerQuality,
}

/// Winner of a k-NN vote.
#[derive(Debug, Clone, Copy)]
pub struct VoteWin {
    pub speaker_id: Uuid,
    pub min_dist: f32,
}

/// Decide a winning speaker from k-NN neighbors `(speaker_id, distance)`, or `None` if no
/// speaker is confidently the owner. Confident = owns a majority (>=60%) of neighbors, OR
/// owns a strong plurality (>= k/3) with a tight nearest neighbor (<= match_threshold) and a
/// >=2 margin over the runner-up. Pure, so it unit-tests without a DB.
pub fn vote(neighbors: &[(Uuid, f32)], k: i64, match_threshold: f32) -> Option<VoteWin> {
    if neighbors.is_empty() {
        return None;
    }
    use std::collections::HashMap;
    let mut counts: HashMap<Uuid, (usize, f32)> = HashMap::new(); // (count, min_dist)
    for (id, d) in neighbors {
        let e = counts.entry(*id).or_insert((0, f32::INFINITY));
        e.0 += 1;
        if *d < e.1 {
            e.1 = *d;
        }
    }
    let n = neighbors.len();
    let mut sorted: Vec<(Uuid, usize, f32)> =
        counts.iter().map(|(id, (c, d))| (*id, *c, *d)).collect();
    // Most neighbors first; tie-break on the closest single neighbor.
    sorted.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then(a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
    });
    let (wid, wcount, wmin) = sorted[0];
    let runner = sorted.get(1).map(|x| x.1).unwrap_or(0);

    let majority = wcount as f32 >= 0.6 * n as f32;
    let plurality_floor = (k as f32 / 3.0).ceil() as usize;
    let plurality = wcount >= plurality_floor && wmin <= match_threshold && wcount >= runner + 2;

    if majority || plurality {
        Some(VoteWin {
            speaker_id: wid,
            min_dist: wmin,
        })
    } else {
        None
    }
}

/// What to do with the segment's speaker assignment.
enum Action {
    /// Confident same speaker, clean audio: attach + fold the embedding into the centroid.
    Match(Uuid),
    /// Attach to an existing speaker but do NOT touch its centroid (gray zone or marginal).
    Attach(Uuid),
    /// Clean audio, far from everyone: a new identity.
    Mint,
    /// Refuse: leave speaker_id NULL (marginal audio with no confident home).
    Null,
}

/// Resolve (and persist) the speaker_id for one segment inside an open transaction. Returns
/// the resolved `speaker_id`, or `None` when the segment is left unattributed (NULL). All
/// cosine values are DISTANCE (1 - similarity) over L2-normalized vectors.
pub async fn assign_speaker(
    tx: &mut Transaction<'_, Postgres>,
    segment_id: Uuid,
    device_id: &str,
    sp: &SpeakerWrite,
    cfg: &SpeakerMatchConfig,
) -> anyhow::Result<Option<Uuid>> {
    // 1. Global serialization for the whole match/mint step.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SPEAKER_LOCK_KEY)
        .execute(&mut **tx)
        .await
        .context("taking speaker advisory lock")?;

    // 2. Idempotency: if this segment already has a REAL prior speaker result (a voiceprint
    //    was computed — speaker_id set, or a deliberate marginal NULL with an embedding),
    //    reuse its exact outcome and touch nothing, so reprocessing is byte-identical. A
    //    `quality='reject'` tombstone is NOT a real result (the prior run produced no
    //    voiceprint, e.g. a too-short clip before windowing); fall through so a better run
    //    can now attribute the segment — the delete-then-insert below replaces the tombstone.
    if let Some(row) = sqlx::query(
        "SELECT speaker_id, quality FROM speaker_segments WHERE segment_id = $1 LIMIT 1",
    )
    .bind(segment_id)
    .fetch_optional(&mut **tx)
    .await
    .context("reading prior speaker assignment")?
    {
        let quality: Option<String> = row.try_get("quality")?;
        if quality.as_deref() != Some("reject") {
            let prior: Option<Uuid> = row.try_get("speaker_id")?;
            return Ok(prior);
        }
    }

    // 3. Per-tx HNSW/timeout GUCs (after the early-return so reprocess stays untouched).
    //    iterative_scan keeps the filtered ANN walk probing until it fills k (the
    //    metadata-filter recall-cliff mitigation); the timeout is generous so it never
    //    aborts the transcript write that shares this transaction.
    sqlx::query("SET LOCAL hnsw.iterative_scan = 'strict_order'")
        .execute(&mut **tx)
        .await
        .context("set iterative_scan")?;
    let ef_search = cfg.knn_ef_search.max(cfg.knn_k).max(1);
    sqlx::query(AssertSqlSafe(format!(
        "SET LOCAL hnsw.ef_search = {ef_search}"
    )))
    .execute(&mut **tx)
    .await
    .context("set ef_search")?;
    let timeout_ms = cfg.knn_statement_timeout_ms.max(0);
    sqlx::query(AssertSqlSafe(format!(
        "SET LOCAL statement_timeout = {timeout_ms}"
    )))
    .execute(&mut **tx)
    .await
    .context("set statement_timeout")?;

    // 4a. k-NN over the raw per-segment voiceprints. $1 is referenced three times (one bind).
    let qvec = pgvector::Vector::from(sp.embedding.clone());
    let neighbor_rows = sqlx::query(
        "SELECT speaker_id, (embedding <=> $1) AS dist \
         FROM speaker_segments \
         WHERE speaker_id IS NOT NULL AND (embedding <=> $1) <= $2 \
         ORDER BY embedding <=> $1 LIMIT $3",
    )
    .bind(qvec.clone())
    .bind(cfg.knn_neighbor_ceiling as f64)
    .bind(cfg.knn_k)
    .fetch_all(&mut **tx)
    .await
    .context("k-NN over speaker_segments")?;

    let mut neighbors: Vec<(Uuid, f32)> = Vec::with_capacity(neighbor_rows.len());
    for r in &neighbor_rows {
        let id: Uuid = r.get("speaker_id");
        let d: f64 = r.try_get("dist")?;
        neighbors.push((id, d as f32));
    }

    // 4b. Vote wins only when confident AND we have enough neighbors (cold-start otherwise
    //     falls back to the centroid catalog scan).
    let decided: Option<(Uuid, f32)> = match vote(&neighbors, cfg.knn_k, cfg.match_threshold) {
        Some(w) if neighbors.len() as i64 >= cfg.knn_min_neighbors => {
            Some((w.speaker_id, w.min_dist))
        }
        _ => nearest_centroid(tx, &sp.embedding).await?,
    };

    // 4c. Mint-guard hysteresis (only clean audio may mint; gray zone always attaches).
    let action = match decided {
        Some((id, d)) if d <= cfg.match_threshold => {
            if sp.quality == SpeakerQuality::Mint {
                Action::Match(id)
            } else {
                Action::Attach(id)
            }
        }
        Some((id, d)) if d <= cfg.mint_distance_floor => Action::Attach(id),
        // d > mint_floor, or no candidate at all:
        _ => {
            if sp.quality == SpeakerQuality::Mint {
                Action::Mint
            } else {
                Action::Null
            }
        }
    };

    // 5. Resolve the speaker_id + the stored centroid-eligibility tag. 'clean' rows (match /
    //    mint) feed the centroid recompute; 'marginal' rows (attach / null) never do — so a
    //    noisy attach can identify a speaker without ever drifting its centroid.
    let (speaker_id, quality_tag): (Option<Uuid>, &str) = match action {
        Action::Match(id) => (Some(id), "clean"),
        Action::Attach(id) => (Some(id), "marginal"),
        Action::Mint => {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO speakers (speaker_id, centroid, n_samples, first_seen_device_id) \
                 VALUES ($1, $2, 1, $3)",
            )
            .bind(id)
            .bind(qvec.clone())
            .bind(device_id)
            .execute(&mut **tx)
            .await
            .context("minting speaker")?;
            tracing::info!(speaker_id = %id, "minted new speaker (clean + far from all known voices)");
            (Some(id), "clean")
        }
        Action::Null => (None, "marginal"),
    };

    // 6. UPSERT the raw per-segment vector (delete-then-insert; partitioned, no UNIQUE).
    sqlx::query("DELETE FROM speaker_segments WHERE segment_id = $1")
        .bind(segment_id)
        .execute(&mut **tx)
        .await
        .context("clearing prior speaker_segment")?;
    sqlx::query(
        "INSERT INTO speaker_segments \
         (segment_id, device_id, speaker_id, start_unix_nanos, end_unix_nanos, embedding, quality) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(segment_id)
    .bind(device_id)
    .bind(speaker_id)
    .bind(sp.start_unix_nanos)
    .bind(sp.end_unix_nanos)
    .bind(qvec)
    .bind(quality_tag)
    .execute(&mut **tx)
    .await
    .context("inserting speaker_segment")?;

    // 7. On a clean MATCH, recompute the centroid from recent clean rows (now including the
    //    row just written). A MINT already seeded centroid = this embedding, n_samples = 1.
    if let Action::Match(id) = action {
        recompute_centroid(tx, id, cfg.centroid_window).await?;
    }

    Ok(speaker_id)
}

/// Nearest speaker centroid by cosine distance over the full (small) catalog. The fallback
/// used at cold-start (too few raw neighbors to vote) and after retention drops raw vectors.
/// Deliberately includes ARCHIVED speakers (0021): archiving is display-level only — excluding
/// a disregarded voice here would just re-mint a duplicate that reappears under "Unidentified".
async fn nearest_centroid(
    tx: &mut Transaction<'_, Postgres>,
    embedding: &[f32],
) -> anyhow::Result<Option<(Uuid, f32)>> {
    let rows = sqlx::query("SELECT speaker_id, centroid FROM speakers")
        .fetch_all(&mut **tx)
        .await
        .context("loading speaker catalog")?;
    let mut best: Option<(Uuid, f32)> = None;
    for r in &rows {
        let Ok(Some(centroid)) = r.try_get::<Option<pgvector::Vector>, _>("centroid") else {
            continue; // a speaker with no centroid can't be matched
        };
        let d = crate::vad::cosine_distance(embedding, centroid.as_slice());
        if best.as_ref().is_none_or(|(_, bd)| d < *bd) {
            let id: Uuid = r.get("speaker_id");
            best = Some((id, d));
        }
    }
    Ok(best)
}

/// Recompute a speaker's centroid as the L2-normalized mean of its most recent `window`
/// CLEAN segments, and set n_samples to its total clean-row count. Self-healing: a bad fold
/// ages out of the window instead of permanently corrupting an online running mean.
async fn recompute_centroid(
    tx: &mut Transaction<'_, Postgres>,
    speaker_id: Uuid,
    window: i64,
) -> anyhow::Result<()> {
    let mean_row = sqlx::query(
        "SELECT avg(embedding) AS mean FROM ( \
            SELECT embedding FROM speaker_segments \
            WHERE speaker_id = $1 AND quality = 'clean' \
            ORDER BY created_at DESC LIMIT $2 \
         ) r",
    )
    .bind(speaker_id)
    .bind(window.max(1))
    .fetch_one(&mut **tx)
    .await
    .context("recomputing centroid mean")?;

    let Ok(Some(mean)) = mean_row.try_get::<Option<pgvector::Vector>, _>("mean") else {
        // No clean rows (shouldn't happen for a Match, which just wrote one) — leave as-is.
        return Ok(());
    };
    let mut centroid = mean.to_vec();
    crate::vad::l2_normalize(&mut centroid);

    let count: i64 = sqlx::query(
        "SELECT count(*) AS n FROM speaker_segments WHERE speaker_id = $1 AND quality = 'clean'",
    )
    .bind(speaker_id)
    .fetch_one(&mut **tx)
    .await
    .context("counting clean segments")?
    .try_get("n")?;

    sqlx::query(
        "UPDATE speakers SET centroid = $1, n_samples = $2, updated_at = now() WHERE speaker_id = $3",
    )
    .bind(pgvector::Vector::from(centroid))
    .bind(count)
    .bind(speaker_id)
    .execute(&mut **tx)
    .await
    .context("updating speaker centroid")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn vote_majority_wins() {
        let a = id(1);
        let nb = vec![(a, 0.2), (a, 0.25), (a, 0.3), (id(2), 0.5)];
        let w = vote(&nb, 15, 0.5).expect("majority winner");
        assert_eq!(w.speaker_id, a);
        assert!((w.min_dist - 0.2).abs() < 1e-6);
    }

    #[test]
    fn vote_none_when_split() {
        // Even split between two speakers, neither a tight anchor -> no confident winner.
        let nb = vec![(id(1), 0.45), (id(1), 0.46), (id(2), 0.47), (id(2), 0.48)];
        assert!(vote(&nb, 15, 0.4).is_none());
    }

    #[test]
    fn vote_plurality_without_majority() {
        // a owns 5 of 12 (< 60%, so NOT a majority) but meets the plurality floor (k/3=5),
        // has a tight nearest neighbor (<= threshold), and leads the runner-up by >=2 -> wins.
        let a = id(1);
        let mut nb = vec![(a, 0.1), (a, 0.2), (a, 0.25), (a, 0.3), (a, 0.35)];
        nb.extend([(id(2), 0.5), (id(2), 0.5), (id(2), 0.5)]); // runner-up: 3
        nb.extend([(id(3), 0.52), (id(3), 0.52)]); // 2
        nb.extend([(id(4), 0.54), (id(4), 0.54)]); // 2  => total 12
        let w = vote(&nb, 15, 0.5).expect("plurality winner");
        assert_eq!(w.speaker_id, a);
        assert!((w.min_dist - 0.1).abs() < 1e-6);
    }

    #[test]
    fn vote_thin_plurality_is_none() {
        // a leads but with only 3 votes (< k/3 = 5) and no majority -> not confident.
        let a = id(1);
        let nb = vec![
            (a, 0.1),
            (a, 0.2),
            (a, 0.3),
            (id(2), 0.5),
            (id(3), 0.5),
            (id(4), 0.5),
            (id(5), 0.5),
        ];
        assert!(vote(&nb, 15, 0.5).is_none());
    }

    #[test]
    fn vote_empty_is_none() {
        assert!(vote(&[], 15, 0.5).is_none());
    }
}
