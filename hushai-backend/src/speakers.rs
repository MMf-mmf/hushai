//! Authenticated read/admin surface over the derived speaker catalog.
//!
//! `GET /v1/speakers`                 — list discovered speakers + bounded sample utterances
//! `PATCH /v1/speakers/{id}`          — set display_name (name a voice)
//! `POST /v1/speakers/{id}/merge`     — merge two ids for the same person
//! `GET /v1/speakers/{id}/sample-audio` — a representative segment's audio (ID a voice by ear)
//!
//! These use RUNTIME sqlx (`query`/`query_as`/`query_scalar` + `.bind`/`try_get`), NOT the
//! `query!` macros in db.rs: the new `speakers`/`speaker_segments` tables aren't in the
//! committed `.sqlx/` cache, so macros would fail `cargo build` until the dev DB is migrated
//! and `cargo sqlx prepare` is re-run. Runtime queries sidestep that entirely.
//!
//! Type contract: `speakers.speaker_id` is `uuid`; `transcript_sentences.speaker_id` is
//! `text` — the LATERAL sample join casts `s.speaker_id::text`, and merge repoints the text
//! column with `$id::text`.

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, PgPool, Postgres, QueryBuilder, Row, Transaction};
use uuid::Uuid;

use crate::error::IngestError;
use crate::state::AppState;

/// Global speaker-space advisory lock key. MUST match the worker's speaker_match.rs so a
/// recluster serializes against the online matcher's match/mint (no concurrent mutation).
const SPEAKER_LOCK_KEY: i64 = 0x6873_7370_6b72; // "hsspkr"

#[derive(Debug, Serialize)]
pub struct SpeakerSummary {
    pub speaker_id: Uuid,
    pub display_name: Option<String>,
    pub n_samples: i64,
    pub sample_utterances: Vec<String>,
}

/// `GET /v1/speakers` — the global (cross-device) catalog with up to 3 sample utterances
/// per speaker (LATERAL correlated LIMIT, so it stays cheap on the partitioned table).
pub async fn list_speakers(
    State(st): State<AppState>,
) -> Result<Json<Vec<SpeakerSummary>>, IngestError> {
    let rows = sqlx::query(
        r#"
        SELECT s.speaker_id,
               s.display_name,
               s.n_samples,
               COALESCE(samp.utts, ARRAY[]::text[]) AS sample_utterances
        FROM speakers s
        LEFT JOIN LATERAL (
            SELECT array_agg(q.text ORDER BY q.start_unix_nanos) AS utts
            FROM (
                SELECT text, start_unix_nanos
                FROM transcript_sentences
                WHERE speaker_id = s.speaker_id::text   -- uuid -> text (REQUIRED cast)
                  AND text IS NOT NULL
                ORDER BY start_unix_nanos
                LIMIT 3
            ) q
        ) samp ON true
        ORDER BY s.n_samples DESC
        "#,
    )
    .fetch_all(&st.pool)
    .await?;

    let out = rows
        .into_iter()
        .map(|r| SpeakerSummary {
            speaker_id: r.get("speaker_id"),
            display_name: r
                .try_get::<Option<String>, _>("display_name")
                .unwrap_or(None),
            n_samples: r.get("n_samples"),
            sample_utterances: r
                .try_get::<Vec<String>, _>("sample_utterances")
                .unwrap_or_default(),
        })
        .collect();
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct RenameReq {
    pub display_name: String,
}

#[derive(Debug, Serialize)]
pub struct SpeakerRow {
    pub speaker_id: Uuid,
    pub display_name: Option<String>,
    pub n_samples: i64,
}

/// `PATCH /v1/speakers/{id}` — name a voice (idempotent). 404 if the id is unknown.
pub async fn rename_speaker(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<RenameReq>,
) -> Result<Json<SpeakerRow>, IngestError> {
    let name = req.display_name.trim();
    if name.is_empty() {
        return Err(IngestError::BadRequest(
            "display_name must not be empty".into(),
        ));
    }
    let row = sqlx::query(
        "UPDATE speakers SET display_name = $1, updated_at = now() \
         WHERE speaker_id = $2 RETURNING speaker_id, display_name, n_samples",
    )
    .bind(name)
    .bind(id)
    .fetch_optional(&st.pool)
    .await?
    .ok_or(IngestError::NotFound("speaker"))?;

    Ok(Json(SpeakerRow {
        speaker_id: row.get("speaker_id"),
        display_name: row
            .try_get::<Option<String>, _>("display_name")
            .unwrap_or(None),
        n_samples: row.get("n_samples"),
    }))
}

#[derive(Debug, Deserialize)]
pub struct MergeReq {
    pub into: Uuid,
}

/// `POST /v1/speakers/{id}/merge` — fold the path id (the loser) into `into` (the survivor):
/// repoint transcript_sentences + speaker_segments, combine centroids weighted by n_samples,
/// preserve the survivor's display_name, delete the loser. Over-splitting is guaranteed by
/// the conservative matcher, so humans hit duplicate ids and need this.
pub async fn merge_speaker(
    State(st): State<AppState>,
    Path(loser): Path<Uuid>,
    Json(req): Json<MergeReq>,
) -> Result<StatusCode, IngestError> {
    let into = req.into;
    if loser == into {
        return Err(IngestError::BadRequest(
            "cannot merge a speaker into itself".into(),
        ));
    }

    let mut tx = st.pool.begin().await?;

    // Lock both rows in a stable id order (deadlock-safe) and read their centroids + counts.
    let (lo, hi) = if loser < into {
        (loser, into)
    } else {
        (into, loser)
    };
    let rows = sqlx::query(
        "SELECT speaker_id, centroid, n_samples FROM speakers \
         WHERE speaker_id IN ($1, $2) FOR UPDATE",
    )
    .bind(lo)
    .bind(hi)
    .fetch_all(&mut *tx)
    .await?;
    if rows.len() != 2 {
        return Err(IngestError::NotFound("speaker (loser or survivor)"));
    }

    let mut loser_c: Option<pgvector::Vector> = None;
    let mut loser_n: i64 = 0;
    let mut into_c: Option<pgvector::Vector> = None;
    let mut into_n: i64 = 0;
    for r in &rows {
        let id: Uuid = r.get("speaker_id");
        let c = r
            .try_get::<Option<pgvector::Vector>, _>("centroid")
            .unwrap_or(None);
        let n: i64 = r.get("n_samples");
        if id == loser {
            loser_c = c;
            loser_n = n;
        } else {
            into_c = c;
            into_n = n;
        }
    }

    // Repoint children. transcript_sentences.speaker_id is text; speaker_segments is uuid.
    sqlx::query("UPDATE transcript_sentences SET speaker_id = $1 WHERE speaker_id = $2")
        .bind(into.to_string())
        .bind(loser.to_string())
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE speaker_segments SET speaker_id = $1 WHERE speaker_id = $2")
        .bind(into)
        .bind(loser)
        .execute(&mut *tx)
        .await?;

    // Weighted, renormalized centroid on the survivor; sum the sample counts. (Keeps the
    // survivor's display_name — never overwritten.)
    let merged = weighted_renorm(into_c.as_ref(), into_n, loser_c.as_ref(), loser_n);
    sqlx::query(
        "UPDATE speakers SET centroid = $1, n_samples = $2, updated_at = now() WHERE speaker_id = $3",
    )
    .bind(merged)
    .bind(into_n + loser_n)
    .bind(into)
    .execute(&mut *tx)
    .await?;

    sqlx::query("DELETE FROM speakers WHERE speaker_id = $1")
        .bind(loser)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(StatusCode::OK)
}

/// Weighted mean of two (optional) centroids, L2-renormalized. Falls back to whichever is
/// present; `None` if both are absent.
fn weighted_renorm(
    a: Option<&pgvector::Vector>,
    na: i64,
    b: Option<&pgvector::Vector>,
    nb: i64,
) -> Option<pgvector::Vector> {
    match (a, b) {
        (Some(a), Some(b)) if a.as_slice().len() == b.as_slice().len() => {
            let (wa, wb) = (na.max(0) as f32, nb.max(0) as f32);
            let denom = (wa + wb).max(1.0);
            let mut v: Vec<f32> = a
                .as_slice()
                .iter()
                .zip(b.as_slice())
                .map(|(x, y)| (x * wa + y * wb) / denom)
                .collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in &mut v {
                    *x /= norm;
                }
            }
            Some(pgvector::Vector::from(v))
        }
        (Some(a), _) => Some(a.clone()),
        (_, Some(b)) => Some(b.clone()),
        (None, None) => None,
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ReclusterReq {
    /// Cosine-distance ceiling: speaker centroids closer than this are treated as the same
    /// person and merged. Defaults to 0.5. Use a value the offline centroids separate at.
    pub threshold: Option<f32>,
}

#[derive(Debug, Serialize)]
pub struct ReclusterResult {
    /// Clusters that contained >1 speaker and were merged.
    pub clusters_merged: usize,
    /// Speaker rows removed (folded into a canonical id).
    pub ids_removed: usize,
    /// Clusters left untouched because they contained >=2 distinct human names (merging
    /// would destroy a label) — surfaced rather than silently merged.
    pub skipped_name_conflicts: usize,
}

/// `POST /v1/speakers/recluster` — Phase C accuracy heal. Agglomerates over-split anonymous
/// ids whose offline centroids are within `threshold`, recomputes the merged centroid,
/// bulk-remaps `transcript_sentences.speaker_id`, and PRESERVES any human-assigned name on
/// the surviving canonical id. Honestly bounded: same raw-cosine metric (no PLDA), and it
/// cannot recover blended/sub-second embeddings — it only heals clean over-splits.
///
/// Conservative on names: a cluster with two different display_names is left untouched (we
/// never silently collapse two named people), and a named speaker is always the canonical
/// survivor of its cluster.
pub async fn recluster(
    State(st): State<AppState>,
    Json(req): Json<ReclusterReq>,
) -> Result<Json<ReclusterResult>, IngestError> {
    let threshold = req.threshold.unwrap_or(0.5);
    let mut tx = st.pool.begin().await?;

    // Serialize against the online matcher (same global lock it takes per segment).
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SPEAKER_LOCK_KEY)
        .execute(&mut *tx)
        .await?;

    struct Spk {
        id: Uuid,
        centroid: Option<Vec<f32>>,
        n: i64,
        name: Option<String>,
    }
    let rows = sqlx::query(
        "SELECT speaker_id, centroid, n_samples, display_name FROM speakers ORDER BY speaker_id FOR UPDATE",
    )
    .fetch_all(&mut *tx)
    .await?;
    let spks: Vec<Spk> = rows
        .iter()
        .map(|r| Spk {
            id: r.get("speaker_id"),
            centroid: r
                .try_get::<Option<pgvector::Vector>, _>("centroid")
                .unwrap_or(None)
                .map(|v| v.as_slice().to_vec()),
            n: r.get("n_samples"),
            name: r
                .try_get::<Option<String>, _>("display_name")
                .unwrap_or(None),
        })
        .collect();

    // Union-Find over speakers by centroid cosine distance (single-linkage).
    let n = spks.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], i: usize) -> usize {
        let mut r = i;
        while parent[r] != r {
            r = parent[r];
        }
        let mut c = i;
        while parent[c] != r {
            let next = parent[c];
            parent[c] = r;
            c = next;
        }
        r
    }
    for i in 0..n {
        for j in (i + 1)..n {
            if let (Some(a), Some(b)) = (&spks[i].centroid, &spks[j].centroid) {
                if cosine_distance(a, b) <= threshold {
                    let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                    if ri != rj {
                        parent[ri] = rj;
                    }
                }
            }
        }
    }

    // Group indices by root.
    let mut clusters: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        clusters.entry(r).or_default().push(i);
    }

    let mut clusters_merged = 0usize;
    let mut ids_removed = 0usize;
    let mut skipped_name_conflicts = 0usize;

    for members in clusters.values() {
        if members.len() < 2 {
            continue;
        }
        // Conservative: never merge across two distinct human names.
        let names: std::collections::HashSet<&str> = members
            .iter()
            .filter_map(|&i| spks[i].name.as_deref())
            .collect();
        if names.len() >= 2 {
            skipped_name_conflicts += 1;
            continue;
        }
        // Canonical = the (single) named member if any, else the most-sampled.
        let canonical = *members
            .iter()
            .max_by_key(|&&i| (spks[i].name.is_some() as i64, spks[i].n))
            .unwrap();
        let canon_id = spks[canonical].id;

        // Weighted mean of all member centroids, renormalized; sum of sample counts.
        let mut acc: Option<Vec<f32>> = None;
        let mut total_n: i64 = 0;
        for &i in members {
            if let Some(c) = &spks[i].centroid {
                let w = spks[i].n.max(0) as f32;
                acc = Some(match acc {
                    None => c.iter().map(|x| x * w).collect(),
                    Some(mut a) => {
                        for (x, y) in a.iter_mut().zip(c) {
                            *x += y * w;
                        }
                        a
                    }
                });
            }
            total_n += spks[i].n;
        }
        if let Some(mut v) = acc {
            let denom = (total_n.max(1)) as f32;
            for x in &mut v {
                *x /= denom;
            }
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in &mut v {
                    *x /= norm;
                }
            }
            sqlx::query("UPDATE speakers SET centroid = $1, n_samples = $2, updated_at = now() WHERE speaker_id = $3")
                .bind(pgvector::Vector::from(v))
                .bind(total_n)
                .bind(canon_id)
                .execute(&mut *tx)
                .await?;
        }

        // Repoint + delete every non-canonical member.
        for &i in members {
            if i == canonical {
                continue;
            }
            let loser = spks[i].id;
            sqlx::query("UPDATE transcript_sentences SET speaker_id = $1 WHERE speaker_id = $2")
                .bind(canon_id.to_string())
                .bind(loser.to_string())
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE speaker_segments SET speaker_id = $1 WHERE speaker_id = $2")
                .bind(canon_id)
                .bind(loser)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM speakers WHERE speaker_id = $1")
                .bind(loser)
                .execute(&mut *tx)
                .await?;
            ids_removed += 1;
        }
        clusters_merged += 1;
    }

    tx.commit().await?;
    Ok(Json(ReclusterResult {
        clusters_merged,
        ids_removed,
        skipped_name_conflicts,
    }))
}

// ============================================================================
// Raw-embedding healing: cluster + collapse over speaker_segments (not centroids).
//
// The centroid `recluster` above clusters over the running-mean centroids, which are
// themselves drifted by the noisy segments that caused the over-split — so it under-merges.
// These helpers cluster at the RAW embedding level (speaker_segments, backed by the HNSW
// index from migration 0007): two speakers are linked when several of their raw segments are
// mutual near-neighbors. That dense substrate links duplicates the centroids miss.
//
// Shared by: POST /v1/speakers/recluster-deep (manual/whole-catalog), GET
// /v1/speakers/duplicates (suggestions), POST /v1/speakers/merge-group (one-tap), and the
// worker's auto_merge_recent (going-forward tight auto-merge). All mutating paths hold the
// global SPEAKER_LOCK_KEY advisory lock, so they never race the online matcher.
// ============================================================================

/// One candidate link between two distinct speakers, with the closest cross-pair distance.
struct Edge {
    lo: Uuid,
    hi: Uuid,
    dist: f32,
}

/// A connected cluster of speakers that should be one identity, with the loosest internal
/// edge distance (a confidence hint — smaller = more certain).
struct Cluster {
    members: Vec<Uuid>,
    max_dist: f32,
}

/// Per-speaker metadata loaded for a merge decision.
struct SpkMeta {
    centroid: Option<pgvector::Vector>,
    n: i64,
    name: Option<String>,
}

fn uf_find(parent: &mut [usize], i: usize) -> usize {
    let mut r = i;
    while parent[r] != r {
        r = parent[r];
    }
    let mut c = i;
    while parent[c] != r {
        let next = parent[c];
        parent[c] = r;
        c = next;
    }
    r
}

/// L2-normalize in place (no-op for a zero vector).
fn l2_normalize_vec(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// n_samples-weighted mean of N (optional) centroids, L2-renormalized. `None` if all absent.
fn weighted_mean_n(members: &[(Option<&pgvector::Vector>, i64)]) -> Option<pgvector::Vector> {
    let mut acc: Option<Vec<f32>> = None;
    let mut total = 0f32;
    for (c, n) in members {
        if let Some(c) = c {
            let w = (*n).max(0) as f32;
            acc = Some(match acc {
                None => c.as_slice().iter().map(|x| x * w).collect(),
                Some(mut a) => {
                    for (x, y) in a.iter_mut().zip(c.as_slice()) {
                        *x += y * w;
                    }
                    a
                }
            });
            total += w;
        }
    }
    let mut v = acc?;
    let denom = total.max(1.0);
    for x in &mut v {
        *x /= denom;
    }
    l2_normalize_vec(&mut v);
    Some(pgvector::Vector::from(v))
}

/// Find candidate inter-speaker links from the raw embeddings. For each segment (optionally
/// only those of recently-active speakers), probe its k nearest *different*-speaker segments
/// via the HNSW index; emit a speaker-pair edge when at least `min_links` such cross-pairs
/// fall within `edge_distance`. The `min_links` gate avoids linking on a single coincidence.
async fn compute_edges(
    tx: &mut Transaction<'_, Postgres>,
    edge_distance: f32,
    knn_k: i64,
    min_links: i64,
    recent_secs: Option<f64>,
) -> Result<Vec<Edge>, sqlx::Error> {
    // The cross-speaker `<>` filter on the ANN walk is the metadata-filter recall cliff;
    // iterative_scan + a raised ef_search let it fill k. tx-scoped, so never leaks.
    sqlx::query("SET LOCAL hnsw.iterative_scan = 'strict_order'")
        .execute(&mut **tx)
        .await?;
    let ef = (knn_k * 4).max(100);
    sqlx::query(AssertSqlSafe(format!("SET LOCAL hnsw.ef_search = {ef}")))
        .execute(&mut **tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = 60000")
        .execute(&mut **tx)
        .await?;

    // Reference speaker_segments directly inside the LATERAL (NOT via a CTE, which Postgres
    // would materialize and lose the HNSW index for the inner kNN).
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT LEAST(a.speaker_id, b.speaker_id) AS lo, \
                GREATEST(a.speaker_id, b.speaker_id) AS hi, \
                min(b.d) AS min_dist \
         FROM speaker_segments a \
         CROSS JOIN LATERAL ( \
             SELECT s.speaker_id, (s.embedding <=> a.embedding) AS d \
             FROM speaker_segments s \
             WHERE s.speaker_id IS NOT NULL AND s.speaker_id <> a.speaker_id \
             ORDER BY s.embedding <=> a.embedding LIMIT ",
    );
    qb.push_bind(knn_k);
    qb.push(") b WHERE a.speaker_id IS NOT NULL AND a.embedding IS NOT NULL AND b.d <= ");
    qb.push_bind(edge_distance as f64);
    if let Some(secs) = recent_secs {
        qb.push(" AND a.created_at >= now() - make_interval(secs => ");
        qb.push_bind(secs);
        qb.push(")");
    }
    qb.push(" GROUP BY lo, hi HAVING count(*) >= ");
    qb.push_bind(min_links);

    let rows = qb.build().fetch_all(&mut **tx).await?;
    Ok(rows
        .iter()
        .map(|r| Edge {
            lo: r.get("lo"),
            hi: r.get("hi"),
            dist: r.get::<f64, _>("min_dist") as f32,
        })
        .collect())
}

/// Union-find the edges into connected speaker clusters (size >= 2), each tagged with its
/// loosest internal edge distance.
fn build_clusters(edges: &[Edge]) -> Vec<Cluster> {
    let mut idx: HashMap<Uuid, usize> = HashMap::new();
    let mut ids: Vec<Uuid> = Vec::new();
    for e in edges {
        for id in [e.lo, e.hi] {
            if !idx.contains_key(&id) {
                idx.insert(id, ids.len());
                ids.push(id);
            }
        }
    }
    let n = ids.len();
    let mut parent: Vec<usize> = (0..n).collect();
    for e in edges {
        let (ri, rj) = (
            uf_find(&mut parent, idx[&e.lo]),
            uf_find(&mut parent, idx[&e.hi]),
        );
        if ri != rj {
            parent[ri] = rj;
        }
    }
    let mut groups: HashMap<usize, Vec<Uuid>> = HashMap::new();
    for i in 0..n {
        let r = uf_find(&mut parent, i);
        groups.entry(r).or_default().push(ids[i]);
    }
    let mut max_dist: HashMap<usize, f32> = HashMap::new();
    for e in edges {
        let r = uf_find(&mut parent, idx[&e.lo]);
        let m = max_dist.entry(r).or_insert(0.0);
        if e.dist > *m {
            *m = e.dist;
        }
    }
    groups
        .into_iter()
        .filter(|(_, m)| m.len() >= 2)
        .map(|(r, members)| Cluster {
            max_dist: *max_dist.get(&r).unwrap_or(&0.0),
            members,
        })
        .collect()
}

/// Load `(centroid, n_samples, display_name)` for the given speaker ids (FOR UPDATE; the
/// global advisory lock is the real mutex, but stable id order keeps it deadlock-clean).
async fn load_speakers(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, SpkMeta>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        "SELECT speaker_id, centroid, n_samples, display_name FROM speakers \
         WHERE speaker_id = ANY($1) ORDER BY speaker_id FOR UPDATE",
    )
    .bind(ids.to_vec())
    .fetch_all(&mut **tx)
    .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for r in &rows {
        let id: Uuid = r.get("speaker_id");
        out.insert(
            id,
            SpkMeta {
                centroid: r
                    .try_get::<Option<pgvector::Vector>, _>("centroid")
                    .unwrap_or(None),
                n: r.get("n_samples"),
                name: r
                    .try_get::<Option<String>, _>("display_name")
                    .unwrap_or(None),
            },
        );
    }
    Ok(out)
}

/// Choose the surviving canonical id of a cluster: the named member if any, else the
/// most-sampled. `Err(())` when the cluster spans >=2 distinct names (never auto-merge over a
/// name boundary — surfaced as a conflict instead).
fn pick_canonical(members: &[Uuid], meta: &HashMap<Uuid, SpkMeta>) -> Result<Option<Uuid>, ()> {
    let names: HashSet<&str> = members
        .iter()
        .filter_map(|id| meta.get(id).and_then(|s| s.name.as_deref()))
        .collect();
    if names.len() >= 2 {
        return Err(());
    }
    Ok(members
        .iter()
        .max_by_key(|id| {
            let s = meta.get(id);
            (
                s.map(|s| s.name.is_some()).unwrap_or(false) as i64,
                s.map(|s| s.n).unwrap_or(0),
            )
        })
        .copied())
}

/// Fold every non-canonical member into `canon_id`: repoint both child tables, recompute the
/// canonical centroid from the RAW segments now under it (discarding the contaminated running
/// mean), set n_samples to the raw count, and delete the losers. Returns the number removed.
/// Falls back to a weighted centroid mean only if retention has dropped all raw vectors.
async fn collapse_cluster(
    tx: &mut Transaction<'_, Postgres>,
    canon_id: Uuid,
    member_ids: &[Uuid],
    meta: &HashMap<Uuid, SpkMeta>,
) -> Result<usize, sqlx::Error> {
    let mut removed = 0usize;
    for &loser in member_ids {
        if loser == canon_id {
            continue;
        }
        // transcript_sentences.speaker_id is text; speaker_segments is uuid.
        sqlx::query("UPDATE transcript_sentences SET speaker_id = $1 WHERE speaker_id = $2")
            .bind(canon_id.to_string())
            .bind(loser.to_string())
            .execute(&mut **tx)
            .await?;
        sqlx::query("UPDATE speaker_segments SET speaker_id = $1 WHERE speaker_id = $2")
            .bind(canon_id)
            .bind(loser)
            .execute(&mut **tx)
            .await?;
        removed += 1;
    }

    // Recompute the centroid from ALL raw segments now under canon (the heal).
    let row = sqlx::query(
        "SELECT avg(embedding) AS mean, count(*) AS cnt \
         FROM speaker_segments WHERE speaker_id = $1 AND embedding IS NOT NULL",
    )
    .bind(canon_id)
    .fetch_one(&mut **tx)
    .await?;
    let raw_mean: Option<pgvector::Vector> = row.try_get("mean").unwrap_or(None);
    let raw_cnt: i64 = row.get("cnt");

    let (centroid, n_samples) = match raw_mean {
        Some(m) => {
            let mut v = m.to_vec();
            l2_normalize_vec(&mut v);
            (Some(pgvector::Vector::from(v)), raw_cnt)
        }
        None => {
            // Retention dropped the raw vectors: fall back to the weighted centroid mean.
            let parts: Vec<(Option<&pgvector::Vector>, i64)> = member_ids
                .iter()
                .filter_map(|id| meta.get(id).map(|s| (s.centroid.as_ref(), s.n)))
                .collect();
            let total: i64 = parts.iter().map(|(_, n)| *n).sum();
            (weighted_mean_n(&parts), total)
        }
    };

    if let Some(c) = centroid {
        sqlx::query("UPDATE speakers SET centroid = $1, n_samples = $2, updated_at = now() WHERE speaker_id = $3")
            .bind(c)
            .bind(n_samples)
            .bind(canon_id)
            .execute(&mut **tx)
            .await?;
    } else {
        sqlx::query("UPDATE speakers SET n_samples = $1, updated_at = now() WHERE speaker_id = $2")
            .bind(n_samples)
            .bind(canon_id)
            .execute(&mut **tx)
            .await?;
    }

    for &loser in member_ids {
        if loser == canon_id {
            continue;
        }
        sqlx::query("DELETE FROM speakers WHERE speaker_id = $1")
            .bind(loser)
            .execute(&mut **tx)
            .await?;
    }
    Ok(removed)
}

#[derive(Debug, Deserialize, Default)]
pub struct ReclusterDeepReq {
    /// Per-edge cosine-distance ceiling for linking two speakers' raw segments (default 0.25,
    /// tighter than the centroid recluster since raw-to-raw is noisier per pair).
    pub edge_distance: Option<f32>,
    /// Nearest cross-speaker neighbors probed per segment (default 5).
    pub knn_k: Option<i64>,
    /// Minimum agreeing cross-pairs to link two speakers (default 2 — not one coincidence).
    pub min_link_count: Option<i64>,
}

/// `POST /v1/speakers/recluster-deep` — heal over-splits by clustering RAW embeddings rather
/// than the drifted centroids. Re-runnable + idempotent (once duplicates are collapsed no
/// edges survive). Preserves names and skips name-conflict clusters, like `recluster`.
pub async fn recluster_deep(
    State(st): State<AppState>,
    Json(req): Json<ReclusterDeepReq>,
) -> Result<Json<ReclusterResult>, IngestError> {
    let edge_distance = req.edge_distance.unwrap_or(0.25);
    let knn_k = req.knn_k.unwrap_or(5);
    let min_links = req.min_link_count.unwrap_or(2);

    let mut tx = st.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SPEAKER_LOCK_KEY)
        .execute(&mut *tx)
        .await?;

    let edges = compute_edges(&mut tx, edge_distance, knn_k, min_links, None).await?;
    let clusters = build_clusters(&edges);
    let all_ids: Vec<Uuid> = clusters
        .iter()
        .flat_map(|c| c.members.iter().copied())
        .collect();
    let meta = load_speakers(&mut tx, &all_ids).await?;

    let mut clusters_merged = 0usize;
    let mut ids_removed = 0usize;
    let mut skipped_name_conflicts = 0usize;
    for c in &clusters {
        match pick_canonical(&c.members, &meta) {
            Err(()) => skipped_name_conflicts += 1,
            Ok(None) => {}
            Ok(Some(canon)) => {
                let removed = collapse_cluster(&mut tx, canon, &c.members, &meta).await?;
                if removed > 0 {
                    clusters_merged += 1;
                    ids_removed += removed;
                }
            }
        }
    }

    tx.commit().await?;
    Ok(Json(ReclusterResult {
        clusters_merged,
        ids_removed,
        skipped_name_conflicts,
    }))
}

#[derive(Debug, Serialize)]
pub struct DuplicateMember {
    pub speaker_id: Uuid,
    pub display_name: Option<String>,
    pub n_samples: i64,
    pub sample_utterances: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DuplicateGroup {
    /// Loosest internal edge distance (confidence hint; smaller = more certain).
    pub max_distance: f32,
    /// True when the group spans >=2 distinct names — never one-tap merge (would lose a label).
    pub name_conflict: bool,
    /// Suggested survivor: the named member if any, else the most-sampled.
    pub suggested_into: Uuid,
    pub members: Vec<DuplicateMember>,
}

/// `GET /v1/speakers/duplicates` — propose (do not merge) groups of likely-duplicate voices,
/// detected at the raw-embedding level at a looser distance than the auto-merge so a human
/// reviews the gray zone. Read-only (no lock). Most-confident groups first.
pub async fn list_duplicates(
    State(st): State<AppState>,
) -> Result<Json<Vec<DuplicateGroup>>, IngestError> {
    const SUGGEST_DISTANCE: f32 = 0.35;
    let mut tx = st.pool.begin().await?;
    let edges = compute_edges(&mut tx, SUGGEST_DISTANCE, 5, 2, None).await?;
    let clusters = build_clusters(&edges);
    let all_ids: Vec<Uuid> = clusters
        .iter()
        .flat_map(|c| c.members.iter().copied())
        .collect();
    let summaries = load_summaries(&mut tx, &all_ids).await?;
    tx.commit().await?; // read-only; nothing written

    let mut out: Vec<DuplicateGroup> = Vec::with_capacity(clusters.len());
    for c in &clusters {
        let names: HashSet<&str> = c
            .members
            .iter()
            .filter_map(|id| summaries.get(id).and_then(|s| s.0.as_deref()))
            .collect();
        let name_conflict = names.len() >= 2;
        // Suggested survivor: named member if any, else most-sampled.
        let suggested_into = c
            .members
            .iter()
            .max_by_key(|id| {
                let s = summaries.get(id);
                (
                    s.map(|s| s.0.is_some()).unwrap_or(false) as i64,
                    s.map(|s| s.1).unwrap_or(0),
                )
            })
            .copied();
        let Some(suggested_into) = suggested_into else {
            continue;
        };
        let members = c
            .members
            .iter()
            .map(|id| {
                let s = summaries.get(id);
                DuplicateMember {
                    speaker_id: *id,
                    display_name: s.and_then(|s| s.0.clone()),
                    n_samples: s.map(|s| s.1).unwrap_or(0),
                    sample_utterances: s.map(|s| s.2.clone()).unwrap_or_default(),
                }
            })
            .collect();
        out.push(DuplicateGroup {
            max_distance: c.max_dist,
            name_conflict,
            suggested_into,
            members,
        });
    }
    // Most-confident (tightest) groups first.
    out.sort_by(|a, b| {
        a.max_distance
            .partial_cmp(&b.max_distance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(Json(out))
}

/// Load `(display_name, n_samples, sample_utterances)` for the given ids (LATERAL sample join
/// as in list_speakers, restricted to the cluster members).
async fn load_summaries(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, (Option<String>, i64, Vec<String>)>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        r#"
        SELECT s.speaker_id, s.display_name, s.n_samples,
               COALESCE(samp.utts, ARRAY[]::text[]) AS sample_utterances
        FROM speakers s
        LEFT JOIN LATERAL (
            SELECT array_agg(q.text ORDER BY q.start_unix_nanos) AS utts
            FROM (
                SELECT text, start_unix_nanos
                FROM transcript_sentences
                WHERE speaker_id = s.speaker_id::text AND text IS NOT NULL
                ORDER BY start_unix_nanos LIMIT 3
            ) q
        ) samp ON true
        WHERE s.speaker_id = ANY($1)
        "#,
    )
    .bind(ids.to_vec())
    .fetch_all(&mut **tx)
    .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for r in &rows {
        let id: Uuid = r.get("speaker_id");
        out.insert(
            id,
            (
                r.try_get::<Option<String>, _>("display_name")
                    .unwrap_or(None),
                r.get::<i64, _>("n_samples"),
                r.try_get::<Vec<String>, _>("sample_utterances")
                    .unwrap_or_default(),
            ),
        );
    }
    Ok(out)
}

// ============================================================================
// Unattributed-audio surfacing: cluster the speaker_segments the online matcher left
// NULL (marginal audio it refused to mint, but whose voiceprint it still stored) into
// candidate voices a human can name. This is the safety net for a speaker who is *only
// ever* marginal-quality: the mint-guard correctly never auto-mints them, so without this
// their audio could never reach the catalog (and they'd be un-nameable). Reuses the same
// HNSW substrate as the duplicate clustering above, but groups NULL segments among
// themselves — keyed by speaker_segments.id, since they share no speaker_id to group on.
// ============================================================================

/// One candidate link between two distinct NULL-speaker segments (by speaker_segments.id).
struct SegEdge {
    lo: i64,
    hi: i64,
    dist: f32,
}

/// A connected cluster of NULL segments that look like one voice.
struct SegCluster {
    members: Vec<i64>,
    max_dist: f32,
}

/// Cluster the unattributed (speaker_id IS NULL) segments by mutual raw-embedding
/// proximity — the NULL-on-NULL analogue of `compute_edges`, keyed by segment row id. The
/// inner `speaker_id IS NULL` filter is the metadata-filter recall cliff the GUCs mitigate.
async fn compute_null_segment_edges(
    tx: &mut Transaction<'_, Postgres>,
    edge_distance: f32,
    knn_k: i64,
    min_links: i64,
) -> Result<Vec<SegEdge>, sqlx::Error> {
    sqlx::query("SET LOCAL hnsw.iterative_scan = 'strict_order'")
        .execute(&mut **tx)
        .await?;
    let ef = (knn_k * 4).max(100);
    sqlx::query(AssertSqlSafe(format!("SET LOCAL hnsw.ef_search = {ef}")))
        .execute(&mut **tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = 60000")
        .execute(&mut **tx)
        .await?;

    // Reference speaker_segments directly inside the LATERAL (NOT via a CTE) so the HNSW
    // index drives the inner kNN.
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT LEAST(a.id, b.sid) AS lo, GREATEST(a.id, b.sid) AS hi, min(b.d) AS min_dist \
         FROM speaker_segments a \
         CROSS JOIN LATERAL ( \
             SELECT s.id AS sid, (s.embedding <=> a.embedding) AS d \
             FROM speaker_segments s \
             WHERE s.speaker_id IS NULL AND s.embedding IS NOT NULL AND s.id <> a.id \
             ORDER BY s.embedding <=> a.embedding LIMIT ",
    );
    qb.push_bind(knn_k);
    qb.push(") b WHERE a.speaker_id IS NULL AND a.embedding IS NOT NULL AND b.d <= ");
    qb.push_bind(edge_distance as f64);
    qb.push(" GROUP BY lo, hi HAVING count(*) >= ");
    qb.push_bind(min_links);

    let rows = qb.build().fetch_all(&mut **tx).await?;
    Ok(rows
        .iter()
        .map(|r| SegEdge {
            lo: r.get("lo"),
            hi: r.get("hi"),
            dist: r.get::<f64, _>("min_dist") as f32,
        })
        .collect())
}

/// Union-find segment-id edges into clusters (size >= 2), each with its loosest internal
/// edge distance. Mirrors `build_clusters` over i64 segment ids instead of speaker Uuids.
fn build_segment_clusters(edges: &[SegEdge]) -> Vec<SegCluster> {
    let mut idx: HashMap<i64, usize> = HashMap::new();
    let mut ids: Vec<i64> = Vec::new();
    for e in edges {
        for id in [e.lo, e.hi] {
            if !idx.contains_key(&id) {
                idx.insert(id, ids.len());
                ids.push(id);
            }
        }
    }
    let n = ids.len();
    let mut parent: Vec<usize> = (0..n).collect();
    for e in edges {
        let (ri, rj) = (
            uf_find(&mut parent, idx[&e.lo]),
            uf_find(&mut parent, idx[&e.hi]),
        );
        if ri != rj {
            parent[ri] = rj;
        }
    }
    let mut groups: HashMap<usize, Vec<i64>> = HashMap::new();
    for i in 0..n {
        let r = uf_find(&mut parent, i);
        groups.entry(r).or_default().push(ids[i]);
    }
    let mut max_dist: HashMap<usize, f32> = HashMap::new();
    for e in edges {
        let r = uf_find(&mut parent, idx[&e.lo]);
        let m = max_dist.entry(r).or_insert(0.0);
        if e.dist > *m {
            *m = e.dist;
        }
    }
    groups
        .into_iter()
        .filter(|(_, m)| m.len() >= 2)
        .map(|(r, members)| SegCluster {
            max_dist: *max_dist.get(&r).unwrap_or(&0.0),
            members,
        })
        .collect()
}

#[derive(Debug, Serialize)]
pub struct UnattributedCluster {
    /// Stable-ish display handle: the smallest member segment_id.
    pub cluster_handle: Uuid,
    pub n_segments: i64,
    /// Loosest internal edge distance (confidence hint; smaller = tighter).
    pub max_distance: f32,
    /// The segments this voice would claim if named — passed back verbatim to the name call.
    pub segment_ids: Vec<Uuid>,
    pub sample_utterances: Vec<String>,
}

/// `GET /v1/speakers/unattributed` — propose candidate voices among the audio the matcher
/// left unattributed (speaker_id NULL), so a human can name a chronically-marginal speaker
/// the mint-guard refused to auto-create. Read-only (no lock). Tightest groups first.
pub async fn list_unattributed(
    State(st): State<AppState>,
) -> Result<Json<Vec<UnattributedCluster>>, IngestError> {
    // These are HUMAN-VERIFIED candidate voices (the user reads the utterances and names one),
    // so the grouping is deliberately LOOSER than the auto-merge duplicate detector (0.35 / 2):
    // real noisy/varied speech from one person spreads well past 0.35, so a tight threshold
    // leaves a chronically-marginal speaker unsurfaced. Env-tunable without a recompile.
    let distance: f32 = std::env::var("SPEAKER_UNATTRIBUTED_DISTANCE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.55);
    let min_links: i64 = std::env::var("SPEAKER_UNATTRIBUTED_MIN_LINKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let mut tx = st.pool.begin().await?;
    let edges = compute_null_segment_edges(&mut tx, distance, 8, min_links).await?;
    let clusters = build_segment_clusters(&edges);

    // Map every member segment-row id -> its segment_id (uuid) in one query.
    let all_ids: Vec<i64> = clusters
        .iter()
        .flat_map(|c| c.members.iter().copied())
        .collect();
    let mut seg_of: HashMap<i64, Uuid> = HashMap::new();
    if !all_ids.is_empty() {
        let rows = sqlx::query(
            "SELECT id, segment_id FROM speaker_segments \
             WHERE id = ANY($1) AND speaker_id IS NULL",
        )
        .bind(all_ids.clone())
        .fetch_all(&mut *tx)
        .await?;
        for r in &rows {
            seg_of.insert(r.get::<i64, _>("id"), r.get::<Uuid, _>("segment_id"));
        }
    }

    let mut out: Vec<UnattributedCluster> = Vec::with_capacity(clusters.len());
    for c in &clusters {
        let mut segment_ids: Vec<Uuid> = c
            .members
            .iter()
            .filter_map(|id| seg_of.get(id).copied())
            .collect();
        segment_ids.sort();
        segment_ids.dedup();
        if segment_ids.is_empty() {
            continue;
        }
        let cluster_handle = segment_ids[0];
        // Up to 3 sample utterances across the cluster's segments. Join on segment_id (these
        // rows have no speaker_id to join on).
        let sample_rows = sqlx::query(
            "SELECT text FROM transcript_sentences \
             WHERE segment_id = ANY($1) AND text IS NOT NULL \
             ORDER BY start_unix_nanos LIMIT 3",
        )
        .bind(segment_ids.clone())
        .fetch_all(&mut *tx)
        .await?;
        let sample_utterances: Vec<String> = sample_rows
            .iter()
            .map(|r| r.get::<String, _>("text"))
            .collect();
        out.push(UnattributedCluster {
            cluster_handle,
            n_segments: segment_ids.len() as i64,
            max_distance: c.max_dist,
            segment_ids,
            sample_utterances,
        });
    }
    tx.commit().await?; // read-only; nothing written

    out.sort_by(|a, b| {
        a.max_distance
            .partial_cmp(&b.max_distance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct NameUnattributedReq {
    pub display_name: String,
    /// The segments to claim (as returned by `list_unattributed`).
    pub segment_ids: Vec<Uuid>,
}

/// `POST /v1/speakers/unattributed/name` — mint a NEW named speaker from a set of
/// currently-unattributed segments and repoint them onto it. Takes the global speaker lock
/// (it mutates assignments, so it serializes against the online matcher). Idempotent /
/// race-safe: only segments STILL NULL are claimed under the lock, so a segment attributed
/// between the GET and this POST is silently skipped, never stolen from an existing speaker.
/// If none survive, 400 with a refresh hint (never mints an empty speaker). The claimed
/// segments keep their `quality='marginal'` tag — by definition they're marginal audio, so
/// they must never feed the worker's clean-only centroid recompute; the centroid is set here
/// directly from their mean as the best available estimate.
pub async fn name_unattributed(
    State(st): State<AppState>,
    Json(req): Json<NameUnattributedReq>,
) -> Result<Json<SpeakerRow>, IngestError> {
    let name = req.display_name.trim();
    if name.is_empty() {
        return Err(IngestError::BadRequest(
            "display_name must not be empty".into(),
        ));
    }
    if req.segment_ids.is_empty() {
        return Err(IngestError::BadRequest(
            "segment_ids must not be empty".into(),
        ));
    }

    let mut tx = st.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SPEAKER_LOCK_KEY)
        .execute(&mut *tx)
        .await?;

    // Claim only segments that are STILL unattributed (race guard under the lock).
    let rows = sqlx::query(
        "SELECT DISTINCT segment_id, device_id FROM speaker_segments \
         WHERE segment_id = ANY($1) AND speaker_id IS NULL AND embedding IS NOT NULL",
    )
    .bind(req.segment_ids.clone())
    .fetch_all(&mut *tx)
    .await?;
    if rows.is_empty() {
        return Err(IngestError::BadRequest(
            "these segments were attributed since the list was fetched; refresh and retry".into(),
        ));
    }
    let survivors: Vec<Uuid> = rows
        .iter()
        .map(|r| r.get::<Uuid, _>("segment_id"))
        .collect();
    let first_device: Option<String> = rows
        .first()
        .and_then(|r| r.try_get::<Option<String>, _>("device_id").unwrap_or(None));

    // Centroid = L2-normalized mean of the survivors' voiceprints; n_samples = their count.
    let agg = sqlx::query(
        "SELECT avg(embedding) AS mean, count(*) AS cnt FROM speaker_segments \
         WHERE segment_id = ANY($1) AND speaker_id IS NULL AND embedding IS NOT NULL",
    )
    .bind(survivors.clone())
    .fetch_one(&mut *tx)
    .await?;
    let mean: Option<pgvector::Vector> = agg.try_get("mean").unwrap_or(None);
    let n_samples: i64 = agg.get("cnt");
    let centroid = mean.map(|m| {
        let mut v = m.to_vec();
        l2_normalize_vec(&mut v);
        pgvector::Vector::from(v)
    });

    let new_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO speakers (speaker_id, centroid, n_samples, display_name, first_seen_device_id) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(new_id)
    .bind(centroid)
    .bind(n_samples)
    .bind(name)
    .bind(first_device)
    .execute(&mut *tx)
    .await?;

    // Repoint both child tables for the survivors only. transcript_sentences.speaker_id is
    // text; speaker_segments is uuid (the 0006 type contract).
    sqlx::query(
        "UPDATE speaker_segments SET speaker_id = $1 \
         WHERE segment_id = ANY($2) AND speaker_id IS NULL",
    )
    .bind(new_id)
    .bind(survivors.clone())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE transcript_sentences SET speaker_id = $1 \
         WHERE segment_id = ANY($2) AND speaker_id IS NULL",
    )
    .bind(new_id.to_string())
    .bind(survivors.clone())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Json(SpeakerRow {
        speaker_id: new_id,
        display_name: Some(name.to_string()),
        n_samples,
    }))
}

#[derive(Debug, Deserialize)]
pub struct MergeGroupReq {
    /// Surviving speaker id (must be one of `members`).
    pub into: Uuid,
    pub members: Vec<Uuid>,
}

/// `POST /v1/speakers/merge-group` — one atomic merge of an entire duplicate group into
/// `into`. Rejects (BadRequest) if any member carries a name different from the survivor's, so
/// a one-tap merge can never silently destroy a human label.
pub async fn merge_group(
    State(st): State<AppState>,
    Json(req): Json<MergeGroupReq>,
) -> Result<Json<ReclusterResult>, IngestError> {
    let into = req.into;
    let mut members = req.members.clone();
    members.sort();
    members.dedup();
    if !members.contains(&into) {
        return Err(IngestError::BadRequest(
            "`into` must be one of `members`".into(),
        ));
    }
    if members.len() < 2 {
        return Err(IngestError::BadRequest(
            "merge-group needs >= 2 members".into(),
        ));
    }

    let mut tx = st.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SPEAKER_LOCK_KEY)
        .execute(&mut *tx)
        .await?;

    let meta = load_speakers(&mut tx, &members).await?;
    if meta.len() != members.len() {
        return Err(IngestError::NotFound("speaker in group"));
    }
    // Never lose a label: reject if any member's name differs from the survivor's.
    let into_name = meta.get(&into).and_then(|s| s.name.as_deref());
    let conflict = members.iter().any(|id| {
        let nm = meta.get(id).and_then(|s| s.name.as_deref());
        nm.is_some() && nm != into_name
    });
    if conflict {
        return Err(IngestError::BadRequest(
            "group has members with names different from the target; merge those manually".into(),
        ));
    }

    let removed = collapse_cluster(&mut tx, into, &members, &meta).await?;
    tx.commit().await?;
    Ok(Json(ReclusterResult {
        clusters_merged: usize::from(removed > 0),
        ids_removed: removed,
        skipped_name_conflicts: 0,
    }))
}

/// Options for the worker's going-forward auto-merge (see `auto_merge_recent`).
#[derive(Debug, Clone, Copy)]
pub struct AutoMergeOpts {
    pub edge_distance: f32,
    pub knn_k: i64,
    pub min_link_count: i64,
    /// Only consider speakers active within this many seconds as merge candidates.
    pub recent_secs: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct AutoMergeStats {
    pub clusters_merged: usize,
    pub ids_removed: usize,
}

/// Auto-merge near-certain duplicate voices among RECENTLY-ACTIVE speakers. Called by the
/// worker after a backlog drains: new static-induced splits fold into the right (often older)
/// identity, without sweeping the historical backlog (the user's "fix going forward only").
/// Skips any name-conflict cluster. Holds the global advisory lock, so it never races the
/// online matcher. This is the engine behind "auto-merge tight + suggest the rest".
pub async fn auto_merge_recent(
    pool: &PgPool,
    opts: AutoMergeOpts,
) -> anyhow::Result<AutoMergeStats> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SPEAKER_LOCK_KEY)
        .execute(&mut *tx)
        .await?;

    let edges = compute_edges(
        &mut tx,
        opts.edge_distance,
        opts.knn_k,
        opts.min_link_count,
        Some(opts.recent_secs as f64),
    )
    .await?;
    let clusters = build_clusters(&edges);
    let all_ids: Vec<Uuid> = clusters
        .iter()
        .flat_map(|c| c.members.iter().copied())
        .collect();
    let meta = load_speakers(&mut tx, &all_ids).await?;

    let mut clusters_merged = 0usize;
    let mut ids_removed = 0usize;
    for c in &clusters {
        // Name-conflict clusters are skipped (never auto-merge across distinct names).
        if let Ok(Some(canon)) = pick_canonical(&c.members, &meta) {
            let removed = collapse_cluster(&mut tx, canon, &c.members, &meta).await?;
            if removed > 0 {
                clusters_merged += 1;
                ids_removed += removed;
            }
        }
    }

    tx.commit().await?;
    Ok(AutoMergeStats {
        clusters_merged,
        ids_removed,
    })
}

/// Cosine distance (1 - similarity) over two equal-length vectors; 1.0 if degenerate.
fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 1.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 1.0;
    }
    1.0 - dot / (na * nb)
}

/// `GET /v1/speakers/{id}/sample-audio` — return one representative segment's media so a
/// human can identify the voice by ear (2s sample text alone is too weak). Picks the
/// earliest speaker_segment for this speaker and reconstructs a PLAYABLE file: for an
/// `fmp4` blob (a bare moof+mdat fragment) the init (`ftyp`+`moov`) from
/// `codec_init_data` is prepended; an `mp4` blob is already self-contained (same rule as
/// the worker's media::extract_pcm and the viewer's remux — key off `container`, never the
/// source). Segments are ~2s and ~20 KB, so the reconstruction is held in memory.
pub async fn sample_audio(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Response, IngestError> {
    let row = sqlx::query(
        "SELECT seg.blob_uri, seg.container, seg.codec_init_data \
         FROM speaker_segments ss JOIN segments seg ON seg.segment_id = ss.segment_id \
         WHERE ss.speaker_id = $1 ORDER BY ss.created_at LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await?
    .ok_or(IngestError::NotFound("sample audio for speaker"))?;

    let blob_uri: String = row.get("blob_uri");
    let container: String = row.get("container");
    let codec_init_data: Option<Vec<u8>> = row.try_get("codec_init_data").unwrap_or(None);

    let raw = blob_uri
        .strip_prefix("file://")
        .ok_or(IngestError::BadRequest("unsupported blob scheme".into()))?;

    // Path-traversal guard: canonicalize (resolves .. / symlinks) and require the result to
    // live under the canonical blob_root. Return 404 off-root to avoid leaking existence.
    let path = tokio::fs::canonicalize(raw)
        .await
        .map_err(|_| IngestError::NotFound("blob"))?;
    if !path.starts_with(&*st.blob_root) {
        return Err(IngestError::NotFound("blob"));
    }

    let media = tokio::fs::read(&path)
        .await
        .map_err(|e| IngestError::Internal(e.into()))?;

    // Reconstruct a decodable file: only a bare fMP4 fragment needs the init prepended.
    let mut bytes = Vec::new();
    if container.eq_ignore_ascii_case("fmp4") {
        if let Some(init) = &codec_init_data {
            bytes.extend_from_slice(init);
        }
    }
    bytes.extend_from_slice(&media);
    let len = bytes.len();

    Response::builder()
        .header(CONTENT_TYPE, "video/mp4") // muxed (h264+aac); clients play the audio track
        .header(CONTENT_LENGTH, len)
        .body(Body::from(bytes))
        .map_err(|e| IngestError::Internal(anyhow::anyhow!("building response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_clusters_group_connected_components() {
        // Two components: {1,2,3} (chain) and {10,11}. The loosest internal edge is the
        // reported max_dist; singletons (none here) would be dropped.
        let edges = vec![
            SegEdge {
                lo: 1,
                hi: 2,
                dist: 0.1,
            },
            SegEdge {
                lo: 2,
                hi: 3,
                dist: 0.2,
            },
            SegEdge {
                lo: 10,
                hi: 11,
                dist: 0.05,
            },
        ];
        let mut clusters = build_segment_clusters(&edges);
        clusters.sort_by_key(|c| c.members.iter().copied().min().unwrap());
        assert_eq!(clusters.len(), 2);

        let mut a = clusters[0].members.clone();
        a.sort();
        assert_eq!(a, vec![1, 2, 3]);
        assert!((clusters[0].max_dist - 0.2).abs() < 1e-6);

        let mut b = clusters[1].members.clone();
        b.sort();
        assert_eq!(b, vec![10, 11]);
    }

    #[test]
    fn segment_clusters_drop_singletons() {
        assert!(build_segment_clusters(&[]).is_empty());
        // A lone edge still forms a size-2 cluster (the smallest worth surfacing).
        let one = vec![SegEdge {
            lo: 7,
            hi: 8,
            dist: 0.3,
        }];
        assert_eq!(build_segment_clusters(&one).len(), 1);
    }
}
