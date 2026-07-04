//! pgvector nearest-neighbour retrieval over `transcript_sentences`.
//!
//! Uses cosine distance (`<=>`) to match the HNSW `vector_cosine_ops` index and the
//! normalized mxbai/bge embeddings. Optional filters (device, time window) are
//! appended dynamically with a `QueryBuilder` and applied on `transcript_sentences`'
//! own denormalized `device_id` / `start_unix_nanos` columns — i.e. the same table as
//! the HNSW index, so the planner can combine the filter with the vector scan instead
//! of post-filtering a global ANN walk (the metadata-filter recall cliff).

use pgvector::Vector;
use sqlx::{AssertSqlSafe, PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

/// A retrieved sentence + its similarity, returned to the client as a citation.
/// `Deserialize` so persisted citations (chat_messages.sources jsonb) round-trip back.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Source {
    pub segment_id: Uuid,
    pub device_id: String,
    pub text: String,
    pub start_unix_nanos: i64,
    pub distance: f64,
    /// Speaker uuid as text (matches the denormalized `transcript_sentences.speaker_id`
    /// column type). `None` = unattributed. The display name is resolved at prompt time.
    pub speaker_id: Option<String>,
    /// Resolved speaker display name (`None` = unattributed or not yet named). Populated by
    /// `enrich_for_display` for the LLM prompt and the UI citation chip. `serde(default)`
    /// keeps citations persisted before this field (chat_messages.sources jsonb) readable.
    #[serde(default)]
    pub speaker_name: Option<String>,
    /// Human-readable time of the utterance, e.g. "yesterday at 5:14 PM". Computed once by
    /// `enrich_for_display` so the prompt, the web citation, and the persisted transcript
    /// all show the same phrasing. Empty for citations persisted before this field.
    #[serde(default)]
    pub time_label: String,
    /// Cross-modal, same-segment vision context ("on camera: Bob; in view: car, backpack"),
    /// populated by `context::enrich_sources_with_vision` for transcript passages. `None` when
    /// enrichment is off / the segment had no vision detections / for non-transcript sources.
    /// `serde(default)` keeps citations persisted before this field (chat_messages.sources) readable.
    #[serde(default)]
    pub visual_context: Option<String>,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            segment_id: Uuid::nil(),
            device_id: String::new(),
            text: String::new(),
            start_unix_nanos: 0,
            distance: 0.0,
            speaker_id: None,
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
        }
    }
}

/// Optional narrowing of the search space.
#[derive(Debug, Default, Clone)]
pub struct Filters {
    pub device_id: Option<String>,
    pub after_unix_nanos: Option<i64>,
    pub before_unix_nanos: Option<i64>,
    /// Restrict to these speaker ids (uuids stringified). `Some(empty)` matches nothing
    /// (an unknown name resolves to this — empty sources, NOT unfiltered). NULL-speaker
    /// rows never match `= ANY(...)`, so unattributed sentences are auto-excluded.
    pub speaker_id: Option<Vec<String>>,
}

/// Per-query HNSW/timeout knobs applied via `SET LOCAL` on the retrieval transaction.
#[derive(Debug, Clone)]
pub struct Tuning {
    /// `hnsw.ef_search`. Effective value is raised to at least `top_k`.
    pub ef_search: i64,
    /// `statement_timeout` for the retrieval transaction, in milliseconds (0 = none).
    pub statement_timeout_ms: i64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            ef_search: 100,
            statement_timeout_ms: 10_000,
        }
    }
}

/// Return the `top_k` nearest sentences to `query_embedding`, closest first.
pub async fn nearest(
    pool: &PgPool,
    query_embedding: &[f32],
    top_k: i64,
    tuning: &Tuning,
    filters: &Filters,
) -> anyhow::Result<Vec<Source>> {
    // Bind the query embedding as a native pgvector::Vector (sqlx 0.9 binary protocol)
    // instead of a `[..]::vector` decimal text literal — no client-side decimal
    // formatting and no server-side text re-parse. Cloned because it's bound twice
    // (the SELECT distance projection and the ORDER BY).
    let qvec = Vector::from(query_embedding.to_vec());

    // `SET LOCAL` is transaction-scoped, so these GUCs never leak onto a pooled
    // connection. iterative_scan lets a *filtered* ANN walk keep probing the HNSW
    // graph until it fills `top_k` (fixing the metadata-filter recall cliff);
    // strict_order keeps results in exact cosine-distance order.
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL hnsw.iterative_scan = 'strict_order'")
        .execute(&mut *tx)
        .await?;
    // ef_search / statement_timeout are GUCs and can't be bound — format validated ints.
    // sqlx 0.9 requires a non-'static query string be asserted injection-safe; these are
    // built only from our own i64s (no user input), so AssertSqlSafe is sound here.
    // When a speaker filter is present the ANN walk is doubly filtered (device ∧ speaker),
    // so raise the ef_search floor. iterative_scan already refills top_k; this is a
    // MITIGATION of the sparse-speaker recall cliff, NOT an elimination — a speaker who
    // spoke very rarely can still under-fill. Exhaustive listing uses list_by_speaker.
    let speaker_filtered = filters.speaker_id.as_ref().is_some_and(|v| !v.is_empty());
    let ef_floor = if speaker_filtered {
        400
    } else {
        tuning.ef_search
    };
    let ef_search = ef_floor.max(tuning.ef_search).max(top_k).max(1);
    sqlx::query(AssertSqlSafe(format!(
        "SET LOCAL hnsw.ef_search = {ef_search}"
    )))
    .execute(&mut *tx)
    .await?;
    let timeout_ms = tuning.statement_timeout_ms.max(0);
    sqlx::query(AssertSqlSafe(format!(
        "SET LOCAL statement_timeout = {timeout_ms}"
    )))
    .execute(&mut *tx)
    .await?;

    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT ts.segment_id, ts.device_id, ts.text, ts.start_unix_nanos, ts.speaker_id, \
         (ts.embedding <=> ",
    );
    qb.push_bind(qvec.clone());
    qb.push(
        ") AS distance \
         FROM transcript_sentences ts \
         WHERE ts.embedding IS NOT NULL",
    );

    if let Some(device_id) = &filters.device_id {
        qb.push(" AND ts.device_id = ").push_bind(device_id.clone());
    }
    if let Some(speaker_ids) = &filters.speaker_id {
        // text[] match against the denormalized text column. ANY('{}') matches nothing
        // (unknown-name contract); NULL speaker_id rows never match (auto-excluded).
        qb.push(" AND ts.speaker_id = ANY(")
            .push_bind(speaker_ids.clone())
            .push("::text[])");
    }
    if let Some(after) = filters.after_unix_nanos {
        qb.push(" AND ts.start_unix_nanos >= ").push_bind(after);
    }
    if let Some(before) = filters.before_unix_nanos {
        qb.push(" AND ts.start_unix_nanos < ").push_bind(before);
    }

    qb.push(" ORDER BY ts.embedding <=> ").push_bind(qvec);
    qb.push(" LIMIT ").push_bind(top_k);

    let rows = qb.build().fetch_all(&mut *tx).await?;
    tx.commit().await?;

    let mut sources = Vec::with_capacity(rows.len());
    for row in rows {
        sources.push(Source {
            segment_id: row.try_get("segment_id")?,
            device_id: row
                .try_get::<Option<String>, _>("device_id")?
                .unwrap_or_default(),
            text: row
                .try_get::<Option<String>, _>("text")?
                .unwrap_or_default(),
            start_unix_nanos: row.try_get("start_unix_nanos")?,
            distance: row.try_get("distance")?,
            speaker_id: row.try_get::<Option<String>, _>("speaker_id")?,
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
        });
    }
    Ok(sources)
}

/// Populate the human-readable display fields (`speaker_name`, `time_label`) on retrieved
/// sources before they are handed to the LLM prompt, serialized to the UI, or persisted.
/// `names` maps speaker-id strings to display names (see `speakers::name_map`).
///
/// Every source ends up with `speaker_name = Some(..)`: a named speaker gets its name, a
/// distinct-but-unnamed speaker gets `unidentified speaker N` (numbered per distinct id in
/// retrieval order, so the prompt can tell two unidentified people apart and rank them),
/// and a NULL-speaker passage gets `unattributed audio`. This is what stops the LLM from
/// collapsing several different unidentified people into one. `now_unix_nanos` anchors the
/// relative time and `tz_offset_secs` is the fixed local offset (`ANALYSIS_TZ_OFFSET_SECS`).
/// Computing these strings here (once) keeps the spoken answer and the citation chip
/// identical and free of timezone/DST drift.
pub fn enrich_for_display(
    sources: &mut [Source],
    names: &std::collections::HashMap<String, String>,
    now_unix_nanos: i64,
    tz_offset_secs: i64,
) {
    let ordinals = crate::speakers::assign_unnamed_ordinals(
        sources.iter().map(|s| s.speaker_id.as_deref()),
        names,
    );
    for s in sources.iter_mut() {
        let ordinal = s
            .speaker_id
            .as_deref()
            .and_then(|id| ordinals.get(id).copied());
        s.speaker_name = Some(crate::speakers::display_label(
            s.speaker_id.as_deref(),
            names,
            ordinal,
        ));
        s.time_label =
            crate::humanize::humanize_time(s.start_unix_nanos, now_unix_nanos, tz_offset_secs);
    }
}

/// Non-semantic, exhaustive listing of everything attributed to `speaker_ids`, in time
/// order. The path for "everything Bob said" / attribution: no vector ranking, no distance
/// prune, so it is never truncated by the recall cliff or the 0.6 threshold. Backed by
/// `transcript_sentences_speaker_time_idx (speaker_id, start_unix_nanos)`. `distance` is
/// 0.0 (so it survives the caller's `retain(distance <= threshold)` unchanged).
pub async fn list_by_speaker(
    pool: &PgPool,
    speaker_ids: &[String],
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
    limit: i64,
) -> anyhow::Result<Vec<Source>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT ts.segment_id, ts.device_id, ts.text, ts.start_unix_nanos, ts.speaker_id \
         FROM transcript_sentences ts \
         WHERE ts.speaker_id = ANY(",
    );
    qb.push_bind(speaker_ids.to_vec()).push("::text[])");
    if let Some(d) = device_id {
        qb.push(" AND ts.device_id = ").push_bind(d.to_string());
    }
    if let Some(a) = after {
        qb.push(" AND ts.start_unix_nanos >= ").push_bind(a);
    }
    if let Some(b) = before {
        qb.push(" AND ts.start_unix_nanos < ").push_bind(b);
    }
    qb.push(" ORDER BY ts.start_unix_nanos ASC LIMIT ")
        .push_bind(limit);

    let rows = qb.build().fetch_all(pool).await?;
    let mut sources = Vec::with_capacity(rows.len());
    for row in rows {
        sources.push(Source {
            segment_id: row.try_get("segment_id")?,
            device_id: row
                .try_get::<Option<String>, _>("device_id")?
                .unwrap_or_default(),
            text: row
                .try_get::<Option<String>, _>("text")?
                .unwrap_or_default(),
            start_unix_nanos: row.try_get("start_unix_nanos")?,
            distance: 0.0,
            speaker_id: row.try_get::<Option<String>, _>("speaker_id")?,
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
        });
    }
    Ok(sources)
}

/// One row per DISTINCT speaker heard in a time window (earliest utterance as the sample),
/// re-sorted chronologically. The deterministic "who was speaking in this clip" path: a roster
/// question over a short window is a set question, not a similarity question — embedding
/// "who was speaking" retrieves nothing useful. NULL speaker_ids collapse to a single
/// "unattributed" row (`DISTINCT ON` treats NULLs as equal), so the caller can tell
/// "speech but unattributed" from "no speech at all" (empty). `distance` is 0.0. Backed by
/// `transcript_sentences_speaker_time_idx (speaker_id, start_unix_nanos)`.
pub async fn list_speakers_in_window(
    pool: &PgPool,
    device_id: Option<&str>,
    after: i64,
    before: i64,
    limit: i64,
) -> anyhow::Result<Vec<Source>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT DISTINCT ON (ts.speaker_id) \
             ts.segment_id, ts.device_id, ts.text, ts.start_unix_nanos, ts.speaker_id \
         FROM transcript_sentences ts \
         WHERE ts.text IS NOT NULL AND ts.start_unix_nanos >= ",
    );
    qb.push_bind(after);
    qb.push(" AND ts.start_unix_nanos < ").push_bind(before);
    if let Some(d) = device_id {
        qb.push(" AND ts.device_id = ").push_bind(d.to_string());
    }
    qb.push(" ORDER BY ts.speaker_id, ts.start_unix_nanos ASC LIMIT ")
        .push_bind(limit);

    let rows = qb.build().fetch_all(pool).await?;
    let mut sources = Vec::with_capacity(rows.len());
    for row in rows {
        sources.push(Source {
            segment_id: row.try_get("segment_id")?,
            device_id: row
                .try_get::<Option<String>, _>("device_id")?
                .unwrap_or_default(),
            text: row
                .try_get::<Option<String>, _>("text")?
                .unwrap_or_default(),
            start_unix_nanos: row.try_get("start_unix_nanos")?,
            distance: 0.0,
            speaker_id: row.try_get::<Option<String>, _>("speaker_id")?,
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
        });
    }
    sources.sort_by_key(|s| s.start_unix_nanos);
    Ok(sources)
}

/// Is there ANY captured footage overlapping `[after, before)` (optionally one device)? Lets an
/// empty speaker roster distinguish "no speech in this clip" from "nothing recorded / not yet
/// processed for the moment you're watching".
pub async fn window_has_footage(
    pool: &PgPool,
    device_id: Option<&str>,
    after: i64,
    before: i64,
) -> anyhow::Result<bool> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT EXISTS(SELECT 1 FROM segments WHERE capture_start_unix_nanos < ",
    );
    qb.push_bind(before);
    qb.push(" AND capture_start_unix_nanos + duration_nanos > ")
        .push_bind(after);
    if let Some(d) = device_id {
        qb.push(" AND device_id = ").push_bind(d.to_string());
    }
    qb.push(")");
    let exists: bool = qb.build_query_scalar().fetch_one(pool).await?;
    Ok(exists)
}

/// One sentence row for the recency backscan (carries `end` for the gap boundary, which `Source`
/// doesn't). Internal to [`latest_conversation`] / [`take_latest_conversation`].
#[derive(Debug, Clone)]
pub struct ConvoSentence {
    pub segment_id: Uuid,
    pub device_id: String,
    pub text: String,
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
    pub speaker_id: Option<String>,
}

/// The most recent recorded CONVERSATION, chronologically — the "what did we last discuss" path.
/// Semantic top-k is wrong for a recency question (it returns keyword-similar 2s snippets from
/// anywhere in the archive); this instead ANCHORS on the newest transcript sentence (within the
/// optional device / time-window filters), scans back over that device's timeline, and keeps the
/// trailing run of sentences with no silence gap longer than `gap_nanos` — the same conversation
/// boundary rule as `analytics::fetch_convos`. Bounded by `max_sentences` / `max_chars` so the
/// spoken summary and the small-model context stay small. `distance` is 0.0 (survives the caller's
/// threshold retain). Empty when nothing was recorded in scope.
///
/// When a time window is given (`after`/`before` from `timeparse`), the anchor is the newest
/// sentence INSIDE it, so "what did we discuss yesterday" summarizes yesterday's last conversation.
#[allow(clippy::too_many_arguments)]
pub async fn latest_conversation(
    pool: &PgPool,
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
    gap_nanos: i64,
    scan_limit: i64,
    max_sentences: usize,
    max_chars: usize,
) -> anyhow::Result<Vec<Source>> {
    // 1. Anchor: the newest sentence with text, honoring the caller's filters. Its device pins the
    //    timeline (a conversation lives on one device); its start caps the backscan.
    let mut anchor_qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT device_id, start_unix_nanos FROM transcript_sentences \
         WHERE text IS NOT NULL",
    );
    if let Some(d) = device_id {
        anchor_qb.push(" AND device_id = ").push_bind(d.to_string());
    }
    if let Some(a) = after {
        anchor_qb.push(" AND start_unix_nanos >= ").push_bind(a);
    }
    if let Some(b) = before {
        anchor_qb.push(" AND start_unix_nanos < ").push_bind(b);
    }
    anchor_qb.push(" ORDER BY start_unix_nanos DESC LIMIT 1");
    let Some(anchor) = anchor_qb.build().fetch_optional(pool).await? else {
        return Ok(vec![]);
    };
    let anchor_device: String = anchor
        .try_get::<Option<String>, _>("device_id")?
        .unwrap_or_default();
    let anchor_start: i64 = anchor.try_get("start_unix_nanos")?;

    // 2. Backscan that device's timeline at/before the anchor (respect an explicit `after` floor),
    //    newest first, capped at scan_limit rows.
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT segment_id, device_id, text, start_unix_nanos, end_unix_nanos, speaker_id \
         FROM transcript_sentences \
         WHERE text IS NOT NULL AND device_id = ",
    );
    qb.push_bind(anchor_device);
    qb.push(" AND start_unix_nanos <= ").push_bind(anchor_start);
    if let Some(a) = after {
        qb.push(" AND start_unix_nanos >= ").push_bind(a);
    }
    qb.push(" ORDER BY start_unix_nanos DESC LIMIT ")
        .push_bind(scan_limit.max(1));

    let rows = qb.build().fetch_all(pool).await?;
    let desc: Vec<ConvoSentence> = rows
        .into_iter()
        .map(|row| {
            Ok::<_, sqlx::Error>(ConvoSentence {
                segment_id: row.try_get("segment_id")?,
                device_id: row
                    .try_get::<Option<String>, _>("device_id")?
                    .unwrap_or_default(),
                text: row.try_get::<Option<String>, _>("text")?.unwrap_or_default(),
                start_unix_nanos: row.try_get("start_unix_nanos")?,
                end_unix_nanos: row
                    .try_get::<Option<i64>, _>("end_unix_nanos")?
                    .unwrap_or_else(|| row.try_get("start_unix_nanos").unwrap_or(0)),
                speaker_id: row.try_get::<Option<String>, _>("speaker_id")?,
            })
        })
        .collect::<Result<_, _>>()?;

    Ok(take_latest_conversation(&desc, gap_nanos, max_sentences, max_chars))
}

/// Pure core of [`latest_conversation`]: given sentences in DESC start order, keep the trailing
/// contiguous conversation (walking back until a silence gap > `gap_nanos`), bounded by
/// `max_sentences` and cumulative `max_chars`, then return them chronologically as `Source`s.
/// Split out so the gap/cap logic is unit-testable without a database.
pub fn take_latest_conversation(
    desc: &[ConvoSentence],
    gap_nanos: i64,
    max_sentences: usize,
    max_chars: usize,
) -> Vec<Source> {
    let mut kept: Vec<&ConvoSentence> = Vec::new();
    let mut chars = 0usize;
    let mut prev_start: Option<i64> = None; // the newer (already-kept) sentence's start
    for s in desc {
        if let Some(newer_start) = prev_start {
            // Silence between this (older) sentence's end and the newer sentence's start.
            let gap = newer_start - s.end_unix_nanos;
            if gap > gap_nanos {
                break; // conversation boundary — everything older belongs to a prior convo
            }
        }
        if kept.len() >= max_sentences {
            break;
        }
        let add = s.text.trim().chars().count();
        // Always keep at least the anchor sentence, even if it alone exceeds max_chars.
        if !kept.is_empty() && chars + add > max_chars {
            break;
        }
        chars += add;
        kept.push(s);
        prev_start = Some(s.start_unix_nanos);
    }
    // Kept is newest→oldest; emit chronological.
    kept.reverse();
    kept.into_iter()
        .map(|s| Source {
            segment_id: s.segment_id,
            device_id: s.device_id.clone(),
            text: s.text.clone(),
            start_unix_nanos: s.start_unix_nanos,
            distance: 0.0,
            speaker_id: s.speaker_id.clone(),
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
        })
        .collect()
}

/// Open-vocabulary OBJECT retrieval (Phase B query side): the `top_k` nearest `scene_objects` rows
/// to `query_embedding` (a CLIP TEXT-tower vector), closest first, deduped to one sighting per
/// segment. The "when did I see a car / a red mug" path. The matched `object_label` is carried in
/// `Source.text`; `speaker_id` is always `None` (objects aren't people).
///
/// CRITICAL — the two-512-d-spaces rule (migration 0009): this searches ONLY `scene_objects` (the
/// OpenCLIP image/text space) over `scene_objects_embedding_hnsw`. It must NEVER touch
/// `person_segments` (the ArcFace face space) — cross-space nearest-neighbours are meaningless.
///
/// `include_frame_rows`: when true, also searches the whole-frame `'__frame__'` open-vocab rows so
/// arbitrary non-COCO phrases ("a red mug") still match; when false, only labeled region rows.
pub async fn nearest_objects(
    pool: &PgPool,
    query_embedding: &[f32],
    top_k: i64,
    tuning: &Tuning,
    filters: &Filters,
    include_frame_rows: bool,
) -> anyhow::Result<Vec<Source>> {
    let qvec = Vector::from(query_embedding.to_vec());

    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL hnsw.iterative_scan = 'strict_order'")
        .execute(&mut *tx)
        .await?;
    let ef_search = tuning.ef_search.max(top_k).max(1);
    sqlx::query(AssertSqlSafe(format!(
        "SET LOCAL hnsw.ef_search = {ef_search}"
    )))
    .execute(&mut *tx)
    .await?;
    let timeout_ms = tuning.statement_timeout_ms.max(0);
    sqlx::query(AssertSqlSafe(format!(
        "SET LOCAL statement_timeout = {timeout_ms}"
    )))
    .execute(&mut *tx)
    .await?;

    // Oversample, then dedup by segment in Rust (one object spans many frames/rows), keeping the
    // closest row per segment since rows arrive in cosine-distance order.
    let fetch = (top_k.max(1) * 5).min(500);
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT so.segment_id, so.device_id, so.object_label, so.start_unix_nanos, (so.embedding <=> ",
    );
    qb.push_bind(qvec.clone());
    qb.push(") AS distance FROM scene_objects so WHERE so.embedding IS NOT NULL");
    if !include_frame_rows {
        qb.push(" AND so.object_label <> '__frame__'");
    }
    if let Some(device_id) = &filters.device_id {
        qb.push(" AND so.device_id = ").push_bind(device_id.clone());
    }
    if let Some(after) = filters.after_unix_nanos {
        qb.push(" AND so.start_unix_nanos >= ").push_bind(after);
    }
    if let Some(before) = filters.before_unix_nanos {
        qb.push(" AND so.start_unix_nanos < ").push_bind(before);
    }
    qb.push(" ORDER BY so.embedding <=> ").push_bind(qvec);
    qb.push(" LIMIT ").push_bind(fetch);

    let rows = qb.build().fetch_all(&mut *tx).await?;
    tx.commit().await?;

    let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    let mut sources = Vec::new();
    for row in rows {
        let segment_id: Uuid = row.try_get("segment_id")?;
        if !seen.insert(segment_id) {
            continue; // closest-per-segment (rows are distance-ordered)
        }
        sources.push(Source {
            segment_id,
            device_id: row
                .try_get::<Option<String>, _>("device_id")?
                .unwrap_or_default(),
            text: row
                .try_get::<Option<String>, _>("object_label")?
                .unwrap_or_default(),
            start_unix_nanos: row.try_get("start_unix_nanos")?,
            distance: row.try_get("distance")?,
            speaker_id: None,
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
        });
        if sources.len() as i64 >= top_k {
            break;
        }
    }
    Ok(sources)
}

/// Exhaustive, non-semantic listing of every segment containing an exact object class (a COCO
/// label, e.g. "car"), in time order — the "every time I saw a car" path, deduped to one sighting
/// per (label, segment). Backed by `scene_objects_label_time_idx (object_label, start_unix_nanos)`.
/// `distance` is 0.0 so it survives the caller's threshold retain. Like `list_by_speaker`, never
/// truncated by the recall cliff.
pub async fn list_by_object_class(
    pool: &PgPool,
    labels: &[String],
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
    limit: i64,
) -> anyhow::Result<Vec<Source>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT DISTINCT ON (so.object_label, so.segment_id) \
         so.segment_id, so.device_id, so.object_label, so.start_unix_nanos \
         FROM scene_objects so WHERE so.object_label = ANY(",
    );
    qb.push_bind(labels.to_vec()).push("::text[])");
    if let Some(d) = device_id {
        qb.push(" AND so.device_id = ").push_bind(d.to_string());
    }
    if let Some(a) = after {
        qb.push(" AND so.start_unix_nanos >= ").push_bind(a);
    }
    if let Some(b) = before {
        qb.push(" AND so.start_unix_nanos < ").push_bind(b);
    }
    // DISTINCT ON needs the matching leading ORDER BY keys; we re-sort by time after the fetch.
    qb.push(" ORDER BY so.object_label, so.segment_id, so.start_unix_nanos ASC LIMIT ")
        .push_bind(limit);

    let rows = qb.build().fetch_all(pool).await?;
    let mut sources = Vec::with_capacity(rows.len());
    for row in rows {
        sources.push(Source {
            segment_id: row.try_get("segment_id")?,
            device_id: row
                .try_get::<Option<String>, _>("device_id")?
                .unwrap_or_default(),
            text: row
                .try_get::<Option<String>, _>("object_label")?
                .unwrap_or_default(),
            start_unix_nanos: row.try_get("start_unix_nanos")?,
            distance: 0.0,
            speaker_id: None,
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
        });
    }
    sources.sort_by_key(|s| s.start_unix_nanos);
    Ok(sources)
}

/// Exhaustive, non-semantic listing of every segment a given PERSON (face) appeared in, in time
/// order — the "when did I see Bob / every time I saw X" path. Deduped to one sighting per
/// (person, segment) since `person_segments` holds many rows per ~2s appearance (one per frame).
/// Backed by `person_segments_person_time_idx (person_id, start_unix_nanos)`. `person_ids` are uuid
/// strings → bound `::uuid[]` (the 0009 type contract — NOT the text[] speaker filter).
///
/// Rows carry `person_id::text` in `Source.speaker_id` and a sentinel `Source.text` ("(seen on
/// camera)") so the existing `enrich_for_display`/`build_prompt` plumbing renders them; `distance`
/// is 0.0 so they survive the caller's threshold retain.
pub async fn list_by_person(
    pool: &PgPool,
    person_ids: &[String],
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
    limit: i64,
) -> anyhow::Result<Vec<Source>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT DISTINCT ON (ps.person_id, ps.segment_id) \
         ps.segment_id, ps.device_id, ps.person_id, ps.start_unix_nanos \
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
    // DISTINCT ON needs the matching leading ORDER BY keys; we re-sort by time after the fetch.
    qb.push(" ORDER BY ps.person_id, ps.segment_id, ps.start_unix_nanos ASC LIMIT ")
        .push_bind(limit);

    let rows = qb.build().fetch_all(pool).await?;
    let mut sources = person_rows_to_sources(rows)?;
    sources.sort_by_key(|s| s.start_unix_nanos);
    Ok(sources)
}

/// Exhaustive, non-semantic listing of every segment a given license PLATE appeared in, in time
/// order — the "when did I see the car with plate ABC123 / every time I saw it" path. Deduped to one
/// sighting per (plate, segment) since `plate_detections` holds many rows per segment (one per OCR
/// read). Backed by `plate_detections_plate_time_idx (plate_id, start_unix_nanos)`. `plate_ids` are
/// uuid strings → bound `::uuid[]` (the 0013 contract — like the person filter, NOT the text[]
/// speaker filter).
///
/// Rows carry `plate_id::text` in `Source.speaker_id` (the display chokepoint expects strings) and a
/// sentinel `Source.text` ("(license plate seen on camera)"); `distance` is 0.0 so they survive the
/// caller's threshold retain. The plate label is resolved at prompt time via `plates::label_map`.
pub async fn list_by_plate(
    pool: &PgPool,
    plate_ids: &[String],
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
    limit: i64,
) -> anyhow::Result<Vec<Source>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT DISTINCT ON (pd.plate_id, pd.segment_id) \
         pd.segment_id, pd.device_id, pd.plate_id, pd.start_unix_nanos \
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
    // DISTINCT ON needs the matching leading ORDER BY keys; we re-sort by time after the fetch.
    qb.push(" ORDER BY pd.plate_id, pd.segment_id, pd.start_unix_nanos ASC LIMIT ")
        .push_bind(limit);

    let rows = qb.build().fetch_all(pool).await?;
    let mut sources = Vec::with_capacity(rows.len());
    for row in rows {
        let plate_id: Uuid = row.try_get("plate_id")?;
        sources.push(Source {
            segment_id: row.try_get("segment_id")?,
            device_id: row
                .try_get::<Option<String>, _>("device_id")?
                .unwrap_or_default(),
            text: "(license plate seen on camera)".to_string(),
            start_unix_nanos: row.try_get("start_unix_nanos")?,
            distance: 0.0,
            speaker_id: Some(plate_id.to_string()),
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
        });
    }
    sources.sort_by_key(|s| s.start_unix_nanos);
    Ok(sources)
}

/// "Who was I with": persons present in the SAME segments the owner appeared in (exact same-segment
/// co-presence — cheap + precise on `person_segments_segment_id_idx`), excluding the owner. One row
/// per co-present person (earliest co-sighting), in time order. `owner_person_ids` are the owner's
/// person id(s) as uuid strings.
pub async fn list_co_occurring_persons(
    pool: &PgPool,
    owner_person_ids: &[String],
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
    limit: i64,
) -> anyhow::Result<Vec<Source>> {
    if owner_person_ids.is_empty() {
        return Ok(Vec::new());
    }
    // The segments the owner is in, time-bounded — the co-presence anchor.
    let mut seg_qb: QueryBuilder<Postgres> =
        QueryBuilder::new("SELECT DISTINCT segment_id FROM person_segments WHERE person_id = ANY(");
    seg_qb
        .push_bind(owner_person_ids.to_vec())
        .push("::uuid[]) AND segment_id IS NOT NULL");
    if let Some(d) = device_id {
        seg_qb.push(" AND device_id = ").push_bind(d.to_string());
    }
    if let Some(a) = after {
        seg_qb.push(" AND start_unix_nanos >= ").push_bind(a);
    }
    if let Some(b) = before {
        seg_qb.push(" AND start_unix_nanos < ").push_bind(b);
    }
    let seg_rows = seg_qb.build().fetch_all(pool).await?;
    let owner_segments: Vec<Uuid> = seg_rows
        .into_iter()
        .map(|r| r.try_get::<Uuid, _>("segment_id"))
        .collect::<Result<_, _>>()?;
    if owner_segments.is_empty() {
        return Ok(Vec::new());
    }

    // Other named/unnamed persons in those same segments (not the owner), earliest sighting first.
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT ps.person_id, MIN(ps.start_unix_nanos) AS first_seen, \
                (array_agg(ps.device_id))[1] AS device_id, \
                (array_agg(ps.segment_id))[1] AS segment_id \
         FROM person_segments ps WHERE ps.segment_id = ANY(",
    );
    qb.push_bind(owner_segments)
        .push("::uuid[]) AND ps.person_id IS NOT NULL AND ps.person_id <> ALL(");
    qb.push_bind(owner_person_ids.to_vec()).push("::uuid[])");
    qb.push(" GROUP BY ps.person_id ORDER BY first_seen ASC LIMIT ")
        .push_bind(limit);

    let rows = qb.build().fetch_all(pool).await?;
    let mut sources = Vec::with_capacity(rows.len());
    for row in rows {
        let person_id: Uuid = row.try_get("person_id")?;
        sources.push(Source {
            segment_id: row.try_get("segment_id")?,
            device_id: row
                .try_get::<Option<String>, _>("device_id")?
                .unwrap_or_default(),
            text: "(seen on camera)".to_string(),
            start_unix_nanos: row.try_get("first_seen")?,
            distance: 0.0,
            speaker_id: Some(person_id.to_string()),
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
        });
    }
    Ok(sources)
}

/// "Who have you seen (so far)": the roster of DISTINCT persons seen across all recordings, most
/// recently seen first — the answer to a general "who's been around" question that names nobody and
/// isn't anchored on the owner (unlike `list_co_occurring_persons`). One row per person (their most
/// recent sighting, so a citation deep-links to where they were last seen). Time/device bounded like
/// the other person queries; backed by `person_segments`.
pub async fn list_recent_persons(
    pool: &PgPool,
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
    limit: i64,
) -> anyhow::Result<Vec<Source>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT ps.person_id, MAX(ps.start_unix_nanos) AS last_seen, \
                (array_agg(ps.device_id ORDER BY ps.start_unix_nanos DESC))[1] AS device_id, \
                (array_agg(ps.segment_id ORDER BY ps.start_unix_nanos DESC))[1] AS segment_id \
         FROM person_segments ps \
         WHERE ps.person_id IS NOT NULL AND ps.start_unix_nanos IS NOT NULL",
    );
    if let Some(d) = device_id {
        qb.push(" AND ps.device_id = ").push_bind(d.to_string());
    }
    if let Some(a) = after {
        qb.push(" AND ps.start_unix_nanos >= ").push_bind(a);
    }
    if let Some(b) = before {
        qb.push(" AND ps.start_unix_nanos < ").push_bind(b);
    }
    qb.push(" GROUP BY ps.person_id ORDER BY last_seen DESC LIMIT ")
        .push_bind(limit);

    let rows = qb.build().fetch_all(pool).await?;
    let mut sources = Vec::with_capacity(rows.len());
    for row in rows {
        let person_id: Uuid = row.try_get("person_id")?;
        sources.push(Source {
            segment_id: row.try_get("segment_id")?,
            device_id: row
                .try_get::<Option<String>, _>("device_id")?
                .unwrap_or_default(),
            text: "(seen on camera)".to_string(),
            start_unix_nanos: row.try_get("last_seen")?,
            distance: 0.0,
            speaker_id: Some(person_id.to_string()),
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
        });
    }
    Ok(sources)
}

/// "Was I with <name>?" — the named person's sightings ONLY in segments where the OWNER was also
/// present (co-presence intersection), one per shared segment, in time order. Empty ⇒ they were
/// never together. Distinct from `list_by_person` (which returns the name's solo sightings and would
/// mislead a "was I with X" question into implying co-presence).
pub async fn list_co_presence_pair(
    pool: &PgPool,
    owner_person_ids: &[String],
    other_person_ids: &[String],
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
    limit: i64,
) -> anyhow::Result<Vec<Source>> {
    if owner_person_ids.is_empty() || other_person_ids.is_empty() {
        return Ok(vec![]);
    }
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT DISTINCT ON (ps.segment_id) ps.segment_id, ps.device_id, ps.person_id, ps.start_unix_nanos \
         FROM person_segments ps WHERE ps.person_id = ANY(",
    );
    qb.push_bind(other_person_ids.to_vec())
        .push("::uuid[]) AND ps.start_unix_nanos IS NOT NULL AND ps.segment_id IN (\
               SELECT o.segment_id FROM person_segments o WHERE o.person_id = ANY(")
        .push_bind(owner_person_ids.to_vec())
        .push("::uuid[]))");
    if let Some(d) = device_id {
        qb.push(" AND ps.device_id = ").push_bind(d.to_string());
    }
    if let Some(a) = after {
        qb.push(" AND ps.start_unix_nanos >= ").push_bind(a);
    }
    if let Some(b) = before {
        qb.push(" AND ps.start_unix_nanos < ").push_bind(b);
    }
    qb.push(" ORDER BY ps.segment_id, ps.start_unix_nanos ASC LIMIT ").push_bind(limit);
    let rows = qb.build().fetch_all(pool).await?;
    let mut sources = person_rows_to_sources(rows)?;
    sources.sort_by_key(|s| s.start_unix_nanos);
    Ok(sources)
}

/// A plain-language description of an event for grounding — no ids/raw values. The worker emits
/// `speech` / `object_seen` / `plate_seen` / person events (known name or `unknown_person`) /
/// `plate_of_interest` / `alert_rule`, with the specific thing in `subject_label`.
fn describe_event(event_type: &str, subject_type: Option<&str>, subject_label: Option<&str>) -> String {
    let label = subject_label.unwrap_or("").trim();
    match (event_type, subject_type) {
        ("speech", _) if !label.is_empty() => format!("heard {label} speaking"),
        ("speech", _) => "heard someone speaking".to_string(),
        ("object_seen", _) if !label.is_empty() => format!("saw a {label}"),
        ("plate_seen", _) | ("plate_of_interest", _) if !label.is_empty() => format!("saw license plate {label}"),
        (_, Some("person")) if !label.is_empty() => format!("saw {label}"),
        (_, Some("person")) => "saw an unrecognized person".to_string(),
        ("alert_rule", _) if !label.is_empty() => format!("an alert fired: {label}"),
        (_, _) if !label.is_empty() => format!("{}: {label}", event_type.replace('_', " ")),
        (_, _) => event_type.replace('_', " "),
    }
}

/// Timeline of NOTABLE events the system flagged (`events` table), newest first — the "what happened
/// / any alerts / what did you notice" path (flaw F7). Optional `subject_type` narrows to a lane
/// ("person"/"object"/"plate"/"speaker"); `alerts_only` keeps just warning/critical severity. Rows
/// carry a plain-language `text` and `distance` 0.0 (survive the caller's threshold retain).
pub async fn list_events(
    pool: &PgPool,
    devices: &[String],
    after: Option<i64>,
    before: Option<i64>,
    subject_type: Option<&str>,
    alerts_only: bool,
    limit: i64,
) -> anyhow::Result<Vec<Source>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT device_id, event_type, subject_type, subject_label, segment_id, start_unix_nanos, severity \
         FROM events WHERE start_unix_nanos IS NOT NULL",
    );
    if !devices.is_empty() {
        qb.push(" AND device_id = ANY(").push_bind(devices.to_vec()).push(")");
    }
    if let Some(a) = after {
        qb.push(" AND start_unix_nanos >= ").push_bind(a);
    }
    if let Some(b) = before {
        qb.push(" AND start_unix_nanos < ").push_bind(b);
    }
    if let Some(st) = subject_type {
        qb.push(" AND subject_type = ").push_bind(st.to_string());
    }
    if alerts_only {
        qb.push(" AND severity IN ('warning','critical')");
    }
    qb.push(" ORDER BY start_unix_nanos DESC LIMIT ").push_bind(limit);

    let rows = qb.build().fetch_all(pool).await?;
    let mut sources = Vec::with_capacity(rows.len());
    for row in rows {
        let event_type: String = row.try_get::<Option<String>, _>("event_type")?.unwrap_or_default();
        let subject_type: Option<String> = row.try_get("subject_type")?;
        let subject_label: Option<String> = row.try_get("subject_label")?;
        sources.push(Source {
            segment_id: row.try_get::<Option<Uuid>, _>("segment_id")?.unwrap_or_else(Uuid::nil),
            device_id: row.try_get::<Option<String>, _>("device_id")?.unwrap_or_default(),
            text: describe_event(&event_type, subject_type.as_deref(), subject_label.as_deref()),
            start_unix_nanos: row.try_get("start_unix_nanos")?,
            distance: 0.0,
            speaker_id: None,
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
        });
    }
    Ok(sources)
}

/// Shared row→Source mapping for the person sighting queries: carry `person_id::text` in
/// `speaker_id` (the display chokepoint expects strings) + a humanized sentinel in `text`.
fn person_rows_to_sources(rows: Vec<sqlx::postgres::PgRow>) -> anyhow::Result<Vec<Source>> {
    let mut sources = Vec::with_capacity(rows.len());
    for row in rows {
        let person_id: Uuid = row.try_get("person_id")?;
        sources.push(Source {
            segment_id: row.try_get("segment_id")?,
            device_id: row
                .try_get::<Option<String>, _>("device_id")?
                .unwrap_or_default(),
            text: "(seen on camera)".to_string(),
            start_unix_nanos: row.try_get("start_unix_nanos")?,
            distance: 0.0,
            speaker_id: Some(person_id.to_string()),
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
        });
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sentence at [start, start+dur) with the given text (1s = 1e9 ns).
    fn sent(start_secs: i64, dur_secs: i64, text: &str) -> ConvoSentence {
        ConvoSentence {
            segment_id: Uuid::now_v7(),
            device_id: "cam-A".into(),
            text: text.into(),
            start_unix_nanos: start_secs * 1_000_000_000,
            end_unix_nanos: (start_secs + dur_secs) * 1_000_000_000,
            speaker_id: None,
        }
    }

    const GAP: i64 = 300 * 1_000_000_000; // 5 min, the default conversation gap

    #[test]
    fn keeps_single_contiguous_conversation_in_chronological_order() {
        // Three sentences 2s apart (well within GAP) — one conversation. Input is DESC.
        let desc = vec![sent(100, 2, "third"), sent(96, 2, "second"), sent(92, 2, "first")];
        let out = take_latest_conversation(&desc, GAP, 40, 4000);
        let texts: Vec<&str> = out.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["first", "second", "third"], "chronological");
        assert!(out.iter().all(|s| s.distance == 0.0));
    }

    #[test]
    fn stops_at_a_silence_gap_larger_than_threshold() {
        // Newest two are one convo; the third is 10 min earlier (a prior conversation).
        let desc = vec![
            sent(1000, 2, "latest"),
            sent(996, 2, "latest-1"),
            sent(300, 2, "old-convo"), // gap 996s-ish >> 5 min
        ];
        let out = take_latest_conversation(&desc, GAP, 40, 4000);
        let texts: Vec<&str> = out.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["latest-1", "latest"], "only the trailing conversation");
    }

    #[test]
    fn caps_by_sentence_count() {
        let desc: Vec<ConvoSentence> = (0..10)
            .rev()
            .map(|i| sent(1000 + i * 2, 2, "x"))
            .collect(); // 10 contiguous, DESC
        let out = take_latest_conversation(&desc, GAP, 3, 4000);
        assert_eq!(out.len(), 3, "sentence cap honored");
    }

    #[test]
    fn caps_by_chars_but_always_keeps_the_anchor() {
        // Each sentence is 10 chars; max_chars 15 keeps only the anchor (adding a 2nd would hit 20).
        let desc = vec![
            sent(100, 2, "0123456789"),
            sent(96, 2, "abcdefghij"),
        ];
        let out = take_latest_conversation(&desc, GAP, 40, 15);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "0123456789", "the anchor (newest) survives the char cap");

        // A single oversized anchor is still kept (never return empty when data exists).
        let big = vec![sent(100, 2, &"z".repeat(100))];
        let out = take_latest_conversation(&big, GAP, 40, 15);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn empty_input_yields_empty() {
        assert!(take_latest_conversation(&[], GAP, 40, 4000).is_empty());
    }
}
