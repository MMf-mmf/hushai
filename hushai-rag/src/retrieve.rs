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
    /// Persisted conversation assignment (migration 0025). `None` = unthreaded (pre-feature
    /// history or the threader's lag tail) — consumers fall back to the gap heuristic.
    /// `serde(default)` keeps citations persisted before this field readable.
    #[serde(default)]
    pub conversation_id: Option<Uuid>,
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
            conversation_id: None,
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
         ts.conversation_id, (ts.embedding <=> ",
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
            conversation_id: row.try_get::<Option<Uuid>, _>("conversation_id")?,
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
        "SELECT ts.segment_id, ts.device_id, ts.text, ts.start_unix_nanos, ts.speaker_id, \
         ts.conversation_id \
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
            conversation_id: row.try_get::<Option<Uuid>, _>("conversation_id")?,
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
             ts.segment_id, ts.device_id, ts.text, ts.start_unix_nanos, ts.speaker_id, \
             ts.conversation_id \
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
            conversation_id: row.try_get::<Option<Uuid>, _>("conversation_id")?,
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
    /// Persisted threading assignment (0025). `None` = unthreaded → gap-heuristic fallback.
    pub conversation_id: Option<Uuid>,
}

impl ConvoSentence {
    fn to_source(&self) -> Source {
        Source {
            segment_id: self.segment_id,
            device_id: self.device_id.clone(),
            text: self.text.clone(),
            start_unix_nanos: self.start_unix_nanos,
            distance: 0.0,
            speaker_id: self.speaker_id.clone(),
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
            conversation_id: self.conversation_id,
        }
    }
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
        "SELECT segment_id, device_id, text, start_unix_nanos, end_unix_nanos, speaker_id, \
         conversation_id \
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
                conversation_id: row.try_get::<Option<Uuid>, _>("conversation_id")?,
            })
        })
        .collect::<Result<_, _>>()?;

    Ok(take_latest_conversation(&desc, gap_nanos, max_sentences, max_chars))
}

/// Pure core of [`latest_conversation`]: given sentences in DESC start order, keep the trailing
/// contiguous conversation (walking back until a silence gap > `gap_nanos`), bounded by
/// `max_sentences` and cumulative `max_chars`, then return them chronologically as `Source`s.
/// Split out so the gap/cap logic is unit-testable without a database.
///
/// THREADED-FIRST (0025): the backscan also stops when the persisted `conversation_id`
/// CHANGES from the first non-NULL id seen — two back-to-back different conversations
/// closer than the gap no longer glue together (the pre-threading contamination hole).
/// NULL rows (the threader's lag tail / pre-feature history) splice into the adjacent
/// threaded conversation exactly as before, on the gap rule alone.
pub fn take_latest_conversation(
    desc: &[ConvoSentence],
    gap_nanos: i64,
    max_sentences: usize,
    max_chars: usize,
) -> Vec<Source> {
    let mut kept: Vec<&ConvoSentence> = Vec::new();
    let mut chars = 0usize;
    let mut prev_start: Option<i64> = None; // the newer (already-kept) sentence's start
    let mut thread_id: Option<Uuid> = None; // first persisted conversation_id on the walk
    for s in desc {
        if let Some(newer_start) = prev_start {
            // Silence between this (older) sentence's end and the newer sentence's start.
            let gap = newer_start - s.end_unix_nanos;
            if gap > gap_nanos {
                break; // conversation boundary — everything older belongs to a prior convo
            }
        }
        if let Some(cid) = s.conversation_id {
            match thread_id {
                Some(t) if t != cid => break, // a DIFFERENT threaded conversation — stop
                Some(_) => {}
                None => thread_id = Some(cid), // the NULL tail spliced onto this conversation
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
    kept.into_iter().map(ConvoSentence::to_source).collect()
}

/// ALL conversations in a window ("what have we spoken about today"), grouped per device by the
/// same silence-gap rule as [`take_latest_conversation`], newest conversations preferred, bounded
/// by `max_convos` / `max_sentences_per` / a global `max_total_chars` budget. Returned OLDEST
/// conversation first (a day summary reads chronologically); sentences within each conversation
/// are chronological too. One SQL fetch; empty when nothing was recorded in scope.
#[allow(clippy::too_many_arguments)]
pub async fn conversations_in_window(
    pool: &PgPool,
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
    gap_nanos: i64,
    scan_limit: i64,
    max_convos: usize,
    max_sentences_per: usize,
    max_total_chars: usize,
) -> anyhow::Result<Vec<Vec<Source>>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT segment_id, device_id, text, start_unix_nanos, end_unix_nanos, speaker_id, \
         conversation_id \
         FROM transcript_sentences \
         WHERE text IS NOT NULL",
    );
    if let Some(d) = device_id {
        qb.push(" AND device_id = ").push_bind(d.to_string());
    }
    if let Some(a) = after {
        qb.push(" AND start_unix_nanos >= ").push_bind(a);
    }
    if let Some(b) = before {
        qb.push(" AND start_unix_nanos < ").push_bind(b);
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
                conversation_id: row.try_get::<Option<Uuid>, _>("conversation_id")?,
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(group_conversations(&desc, gap_nanos, max_convos, max_sentences_per, max_total_chars))
}

/// Pure core of [`conversations_in_window`]: split DESC-ordered sentences into conversations —
/// per DEVICE (a conversation lives on one camera's timeline; interleaved devices must not
/// fragment each other) — then keep the `max_convos` most RECENT conversations across all
/// devices under a global char budget, and emit them oldest-first with chronological sentences.
///
/// THREADED-FIRST (0025): rows carrying a persisted `conversation_id` group EXACTLY by that
/// id (this is what separates two concurrent group conversations interleaved on ONE mic —
/// the gap rule alone cannot). Only the NULL remainder (threader lag tail / pre-feature
/// history) falls back to the silence-gap split; a NULL run then splices into the threaded
/// conversation it is time-adjacent to (within `gap_nanos`) on the same device, so an open
/// conversation and its unthreaded tail read as one.
pub fn group_conversations(
    desc: &[ConvoSentence],
    gap_nanos: i64,
    max_convos: usize,
    max_sentences_per: usize,
    max_total_chars: usize,
) -> Vec<Vec<Source>> {
    // Partition by device, preserving DESC order within each.
    let mut per_device: std::collections::HashMap<&str, Vec<&ConvoSentence>> =
        std::collections::HashMap::new();
    for s in desc {
        per_device.entry(s.device_id.as_str()).or_default().push(s);
    }
    let mut convos: Vec<Vec<&ConvoSentence>> = Vec::new();
    for sentences in per_device.into_values() {
        // 1. Threaded rows bucket by id (DESC order preserved; first-seen = newest first).
        let mut threaded: Vec<(Uuid, Vec<&ConvoSentence>)> = Vec::new();
        let mut nulls: Vec<&ConvoSentence> = Vec::new();
        for s in sentences {
            match s.conversation_id {
                Some(cid) => match threaded.iter_mut().find(|(id, _)| *id == cid) {
                    Some((_, group)) => group.push(s),
                    None => threaded.push((cid, vec![s])),
                },
                None => nulls.push(s),
            }
        }
        // 2. Gap-split the NULL remainder (adjacent gaps within the NULL subsequence).
        let mut null_runs: Vec<Vec<&ConvoSentence>> = Vec::new();
        let mut current: Vec<&ConvoSentence> = Vec::new();
        for s in nulls {
            if let Some(prev) = current.last() {
                // `prev` is the NEWER sentence (DESC walk); silence between them:
                let gap = prev.start_unix_nanos - s.end_unix_nanos;
                if gap > gap_nanos {
                    null_runs.push(std::mem::take(&mut current));
                }
            }
            current.push(s);
        }
        if !current.is_empty() {
            null_runs.push(current);
        }
        // 3. Splice each NULL run into the time-nearest threaded group within the gap
        //    (deterministic: minimal distance, then smaller conversation_id). Runs with no
        //    adjacent threaded group stand alone (pre-feature history keeps working).
        for run in null_runs {
            let run_newest = run.first().map(|s| s.start_unix_nanos).unwrap_or(0);
            let run_oldest_end = run.last().map(|s| s.end_unix_nanos).unwrap_or(0);
            let mut best: Option<(i64, Uuid)> = None;
            for (cid, group) in &threaded {
                let g_newest = group.first().map(|s| s.start_unix_nanos).unwrap_or(0);
                let g_oldest_end = group.last().map(|s| s.end_unix_nanos).unwrap_or(0);
                // Distance between the run's span and the group's span (0 if overlapping).
                let dist = if run_oldest_end > g_newest {
                    run_oldest_end - g_newest
                } else if g_oldest_end > run_newest {
                    g_oldest_end - run_newest
                } else {
                    0
                };
                if dist <= gap_nanos
                    && best.is_none_or(|(bd, bid)| dist < bd || (dist == bd && *cid < bid))
                {
                    best = Some((dist, *cid));
                }
            }
            match best {
                Some((_, cid)) => {
                    let (_, group) = threaded.iter_mut().find(|(id, _)| *id == cid).unwrap();
                    group.extend(run);
                    // Restore DESC order after the splice.
                    group.sort_by_key(|s| std::cmp::Reverse((s.start_unix_nanos, s.segment_id)));
                }
                None => convos.push(run),
            }
        }
        convos.extend(threaded.into_iter().map(|(_, g)| g));
    }
    // Most recent conversations first (by their newest sentence), keep max_convos.
    convos.sort_by_key(|c| std::cmp::Reverse(c.first().map(|s| s.start_unix_nanos).unwrap_or(0)));
    convos.truncate(max_convos.max(1));
    // Apply the per-convo sentence cap + the global char budget (favouring recent convos, which
    // are first at this point), then emit oldest conversation first, chronological inside.
    let mut budget = max_total_chars;
    let mut out: Vec<Vec<Source>> = Vec::new();
    for convo in &convos {
        let mut kept: Vec<&ConvoSentence> = Vec::new();
        for s in convo.iter().take(max_sentences_per) {
            let add = s.text.trim().chars().count();
            // Always keep at least one sentence of the first conversation.
            if (!kept.is_empty() || !out.is_empty()) && add > budget {
                break;
            }
            budget = budget.saturating_sub(add);
            kept.push(s);
        }
        if kept.is_empty() {
            break; // out of budget — older conversations are dropped entirely
        }
        kept.reverse(); // newest→oldest becomes chronological
        out.push(kept.into_iter().map(ConvoSentence::to_source).collect());
    }
    out.reverse(); // recent-first selection becomes oldest-first narration order
    out
}

/// Relative-margin prune on semantic hits: keep only hits with `distance <= best + margin`.
/// Complements the absolute `distance_threshold` — an unrelated conversation's hit can sit
/// just under the absolute cutoff, and one such hit is enough for `expand_to_conversations`
/// to drag that whole conversation into the grounded prompt. Runs BEFORE expansion.
/// Deterministic rows carry `distance 0.0`, so `best` is 0.0 there and they all survive.
/// A `margin <= 0` disables the prune. Pure and order-preserving.
pub fn prune_rel_margin(sources: &mut Vec<Source>, margin: f64) {
    if margin <= 0.0 || sources.is_empty() {
        return;
    }
    let best = sources.iter().map(|s| s.distance).fold(f64::INFINITY, f64::min);
    let cutoff = best + margin;
    sources.retain(|s| s.distance <= cutoff);
}

/// Expand pruned semantic hits into CONVERSATION-scoped groups (0025). Hits sharing a
/// persisted `conversation_id` become one group, widened with that conversation's own
/// sentences within ±`window_nanos` of the hits (one indexed fetch per conversation on
/// `transcript_sentences_conversation_time_idx` — never a blind device time-window, so a
/// concurrent conversation can never bleed in). Unthreaded (NULL) hits stay single-sentence
/// groups — today's exact behavior. Groups come back most-relevant-first (best hit
/// distance); neighbors carry `distance 0.0` (the deterministic-path convention) and are
/// deduped against the hits. `max_total_chars` bounds the whole expansion.
pub async fn expand_to_conversations(
    pool: &PgPool,
    hits: &[Source],
    window_nanos: i64,
    max_sentences_per: usize,
    max_total_chars: usize,
) -> anyhow::Result<Vec<Vec<Source>>> {
    // Distinct conversation ids in best-distance-first order; NULL hits pass through.
    let mut order: Vec<Option<Uuid>> = Vec::new();
    for h in hits {
        if !order.contains(&h.conversation_id) {
            order.push(h.conversation_id);
        }
    }
    let mut budget = max_total_chars;
    let mut groups: Vec<Vec<Source>> = Vec::new();
    for cid in order {
        let members: Vec<&Source> = hits
            .iter()
            .filter(|h| h.conversation_id == cid)
            .collect();
        let Some(cid) = cid else {
            // Unthreaded hits: one single-sentence group each (fallback path).
            for h in members {
                budget = budget.saturating_sub(h.text.trim().chars().count());
                groups.push(vec![h.clone()]);
            }
            continue;
        };
        let lo = members.iter().map(|h| h.start_unix_nanos).min().unwrap_or(0) - window_nanos;
        let hi = members.iter().map(|h| h.start_unix_nanos).max().unwrap_or(0) + window_nanos;
        let rows = sqlx::query(
            "SELECT segment_id, device_id, text, start_unix_nanos, speaker_id, conversation_id \
             FROM transcript_sentences \
             WHERE conversation_id = $1 AND start_unix_nanos >= $2 AND start_unix_nanos <= $3 \
               AND text IS NOT NULL \
             ORDER BY start_unix_nanos, segment_id LIMIT $4",
        )
        .bind(cid)
        .bind(lo)
        .bind(hi)
        .bind(max_sentences_per.max(1) as i64)
        .fetch_all(pool)
        .await?;
        let mut group: Vec<Source> = Vec::new();
        for row in rows {
            let seg: Uuid = row.try_get("segment_id")?;
            let start: i64 = row.try_get("start_unix_nanos")?;
            // The hit row (with its real distance) wins over its neighbor duplicate.
            if let Some(hit) = members
                .iter()
                .find(|h| h.segment_id == seg && h.start_unix_nanos == start)
            {
                group.push((*hit).clone());
                continue;
            }
            let text: String = row.try_get::<Option<String>, _>("text")?.unwrap_or_default();
            let add = text.trim().chars().count();
            if add > budget && !group.is_empty() {
                continue; // budget spent — keep the hits, skip further neighbors
            }
            budget = budget.saturating_sub(add);
            group.push(Source {
                segment_id: seg,
                device_id: row
                    .try_get::<Option<String>, _>("device_id")?
                    .unwrap_or_default(),
                text,
                start_unix_nanos: start,
                distance: 0.0,
                speaker_id: row.try_get::<Option<String>, _>("speaker_id")?,
                speaker_name: None,
                time_label: String::new(),
                visual_context: None,
                conversation_id: Some(cid),
            });
        }
        // Any hit the window fetch missed (e.g. LIMIT) still must appear.
        for h in members {
            if !group
                .iter()
                .any(|s| s.segment_id == h.segment_id && s.start_unix_nanos == h.start_unix_nanos)
            {
                group.push(h.clone());
            }
        }
        group.sort_by_key(|s| (s.start_unix_nanos, s.segment_id));
        groups.push(group);
    }
    Ok(groups)
}

/// Rebuild conversation groups from a flattened source list + per-group lengths (the
/// flatten order is the render order, so citations line up). Used by the chat path, which
/// must enrich the FLAT list (global unnamed-speaker ordinals) before re-grouping.
pub fn regroup_sources(flat: &[Source], lens: &[usize]) -> Vec<Vec<Source>> {
    let mut out = Vec::with_capacity(lens.len());
    let mut i = 0usize;
    for &n in lens {
        let end = (i + n).min(flat.len());
        out.push(flat[i..end].to_vec());
        i = end;
    }
    out
}

/// Conversation catalog row for the list endpoint / participants queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationMeta {
    pub conversation_id: Uuid,
    pub device_id: Option<String>,
    pub started_at_unix_nanos: i64,
    pub ended_at_unix_nanos: i64,
    pub status: String,
    /// Speaker uuids as text (display names resolve at the caller via `speakers::name_map`).
    pub speaker_ids: Vec<String>,
    pub sentence_count: i32,
}

/// List conversations newest-first, optionally filtered by device / window / a participant
/// set (every id in `speaker_ids` must have spoken: `speaker_ids @> $n`, GIN-backed).
pub async fn list_conversations(
    pool: &PgPool,
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
    speaker_ids: Option<&[Uuid]>,
    limit: i64,
) -> anyhow::Result<Vec<ConversationMeta>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT conversation_id, primary_device_id, started_at_unix_nanos, ended_at_unix_nanos, \
         status, speaker_ids, sentence_count FROM conversations WHERE TRUE",
    );
    if let Some(d) = device_id {
        qb.push(" AND primary_device_id = ").push_bind(d.to_string());
    }
    if let Some(a) = after {
        qb.push(" AND ended_at_unix_nanos >= ").push_bind(a);
    }
    if let Some(b) = before {
        qb.push(" AND started_at_unix_nanos < ").push_bind(b);
    }
    if let Some(ids) = speaker_ids {
        qb.push(" AND speaker_ids @> ").push_bind(ids.to_vec());
    }
    qb.push(" ORDER BY started_at_unix_nanos DESC LIMIT ")
        .push_bind(limit.clamp(1, 500));
    let rows = qb.build().fetch_all(pool).await?;
    rows.into_iter()
        .map(|row| {
            Ok(ConversationMeta {
                conversation_id: row.try_get("conversation_id")?,
                device_id: row.try_get::<Option<String>, _>("primary_device_id")?,
                started_at_unix_nanos: row.try_get("started_at_unix_nanos")?,
                ended_at_unix_nanos: row.try_get("ended_at_unix_nanos")?,
                status: row.try_get("status")?,
                speaker_ids: row
                    .try_get::<Vec<Uuid>, _>("speaker_ids")?
                    .into_iter()
                    .map(|u| u.to_string())
                    .collect(),
                sentence_count: row.try_get("sentence_count")?,
            })
        })
        .collect()
}

/// The full ordered transcript of ONE conversation, as `Source`s (distance 0.0), ready for
/// `enrich_for_display`. Bounded by `max_sentences`.
pub async fn conversation_transcript(
    pool: &PgPool,
    conversation_id: Uuid,
    max_sentences: usize,
) -> anyhow::Result<Vec<Source>> {
    let rows = sqlx::query(
        "SELECT segment_id, device_id, text, start_unix_nanos, speaker_id, conversation_id \
         FROM transcript_sentences \
         WHERE conversation_id = $1 AND text IS NOT NULL \
         ORDER BY start_unix_nanos, segment_id LIMIT $2",
    )
    .bind(conversation_id)
    .bind(max_sentences.max(1) as i64)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(Source {
                segment_id: row.try_get("segment_id")?,
                device_id: row
                    .try_get::<Option<String>, _>("device_id")?
                    .unwrap_or_default(),
                text: row.try_get::<Option<String>, _>("text")?.unwrap_or_default(),
                start_unix_nanos: row.try_get("start_unix_nanos")?,
                distance: 0.0,
                speaker_id: row.try_get::<Option<String>, _>("speaker_id")?,
                speaker_name: None,
                time_label: String::new(),
                visual_context: None,
                conversation_id: row.try_get::<Option<Uuid>, _>("conversation_id")?,
            })
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
            conversation_id: None,
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
            conversation_id: None,
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
            conversation_id: None,
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
            conversation_id: None,
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
            conversation_id: None,
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
            conversation_id: None,
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
            conversation_id: None,
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
            conversation_id: None,
        }
    }

    /// Same as [`sent`] but carrying a persisted conversation id (threaded row).
    fn tsent(start_secs: i64, dur_secs: i64, text: &str, convo: u128) -> ConvoSentence {
        ConvoSentence {
            conversation_id: Some(Uuid::from_u128(convo)),
            ..sent(start_secs, dur_secs, text)
        }
    }

    const GAP: i64 = 300 * 1_000_000_000; // 5 min, the default conversation gap

    /// A semantic hit at the given cosine distance (only `distance` matters to the prune).
    fn hit(distance: f64) -> Source {
        Source { distance, ..sent(0, 2, "hit").to_source() }
    }

    #[test]
    fn prune_rel_margin_drops_hits_far_from_best() {
        // best 0.286 + margin 0.25 = 0.536: the 0.597 cross-conversation straggler dies,
        // on-topic spread survives.
        let mut s: Vec<Source> = [0.286, 0.302, 0.41, 0.521, 0.597].map(hit).into();
        prune_rel_margin(&mut s, 0.25);
        let kept: Vec<f64> = s.iter().map(|x| x.distance).collect();
        assert_eq!(kept, vec![0.286, 0.302, 0.41, 0.521]);
    }

    #[test]
    fn prune_rel_margin_keeps_deterministic_rows_and_respects_disable() {
        // Exhaustive/deterministic rows all carry 0.0 — nothing is dropped.
        let mut det: Vec<Source> = [0.0, 0.0, 0.0].map(hit).into();
        prune_rel_margin(&mut det, 0.25);
        assert_eq!(det.len(), 3);
        // margin <= 0 disables entirely.
        let mut off: Vec<Source> = [0.1, 0.9].map(hit).into();
        prune_rel_margin(&mut off, 0.0);
        assert_eq!(off.len(), 2);
        // Empty input is a no-op, not a panic.
        let mut empty: Vec<Source> = vec![];
        prune_rel_margin(&mut empty, 0.25);
        assert!(empty.is_empty());
    }

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

    #[test]
    fn backscan_stops_when_threaded_conversation_changes() {
        // Two back-to-back threaded conversations only 10s apart (< GAP): the pre-0025
        // gap rule glued them; the id change must now stop the walk.
        let desc = vec![
            tsent(200, 2, "convo-b two", 0xB),
            tsent(196, 2, "convo-b one", 0xB),
            tsent(186, 2, "convo-a tail", 0xA), // 8s gap — inside GAP, different convo
        ];
        let out = take_latest_conversation(&desc, GAP, 40, 4000);
        let texts: Vec<&str> = out.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["convo-b one", "convo-b two"], "id change is a hard stop");
    }

    #[test]
    fn null_lag_tail_splices_onto_the_open_conversation() {
        // The newest rows are unthreaded (threader lag); they splice onto the threaded
        // conversation they're gap-contiguous with, and the walk still stops at the
        // OLDER different conversation.
        let desc = vec![
            sent(300, 2, "lag two"),
            sent(296, 2, "lag one"),
            tsent(290, 2, "open convo", 0xB),
            tsent(280, 2, "previous convo", 0xA),
        ];
        let out = take_latest_conversation(&desc, GAP, 40, 4000);
        let texts: Vec<&str> = out.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["open convo", "lag one", "lag two"]);
    }

    #[test]
    fn interleaved_threaded_conversations_group_by_id_not_gap() {
        // Two concurrent group conversations interleaved on ONE device — the 0025 case
        // the gap rule cannot separate. Rows carry their persisted ids.
        let desc = vec![
            tsent(118, 2, "b three", 0xB),
            tsent(112, 2, "a three", 0xA),
            tsent(106, 2, "b two", 0xB),
            tsent(100, 2, "a two", 0xA),
            tsent(94, 2, "b one", 0xB),
            tsent(88, 2, "a one", 0xA),
        ];
        let convos = group_conversations(&desc, GAP, 8, 40, 4000);
        assert_eq!(convos.len(), 2, "one group per conversation_id");
        for c in &convos {
            let cid = c[0].conversation_id;
            assert!(c.iter().all(|s| s.conversation_id == cid), "no cross-id mixing");
            assert_eq!(c.len(), 3);
        }
    }

    #[test]
    fn null_run_splices_into_adjacent_threaded_group() {
        // A threaded conversation with an unthreaded lag tail 4s after it: one group.
        // A far-away NULL run (an hour earlier) stands alone.
        let desc = vec![
            sent(204, 2, "tail two"),
            sent(200, 2, "tail one"),
            tsent(194, 2, "threaded two", 0xC),
            tsent(190, 2, "threaded one", 0xC),
            sent(-3600, 2, "ancient history"),
        ];
        let convos = group_conversations(&desc, GAP, 8, 40, 4000);
        assert_eq!(convos.len(), 2);
        let spliced = convos
            .iter()
            .find(|c| c.iter().any(|s| s.text == "threaded one"))
            .unwrap();
        let texts: Vec<&str> = spliced.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["threaded one", "threaded two", "tail one", "tail two"]);
        let standalone = convos
            .iter()
            .find(|c| c.iter().any(|s| s.text == "ancient history"))
            .unwrap();
        assert_eq!(standalone.len(), 1);
    }

    /// Same as [`sent`] but on a chosen device (group_conversations partitions per device).
    fn dsent(device: &str, start_secs: i64, dur_secs: i64, text: &str) -> ConvoSentence {
        ConvoSentence {
            device_id: device.into(),
            ..sent(start_secs, dur_secs, text)
        }
    }

    #[test]
    fn groups_split_on_gaps_and_read_oldest_first() {
        // Two conversations on one device, 10 min of silence between them. Input DESC.
        let desc = vec![
            sent(1000, 2, "evening two"),
            sent(996, 2, "evening one"),
            sent(100, 2, "morning two"),
            sent(96, 2, "morning one"),
        ];
        let convos = group_conversations(&desc, GAP, 8, 40, 4000);
        assert_eq!(convos.len(), 2);
        let texts: Vec<Vec<&str>> = convos
            .iter()
            .map(|c| c.iter().map(|s| s.text.as_str()).collect())
            .collect();
        // Oldest conversation first; chronological inside each.
        assert_eq!(texts, vec![vec!["morning one", "morning two"], vec!["evening one", "evening two"]]);
    }

    #[test]
    fn groups_do_not_fragment_across_devices() {
        // Interleaved devices within the same minutes: each device's run is ONE conversation,
        // not four fragments.
        let desc = vec![
            dsent("cam-B", 102, 2, "b two"),
            dsent("cam-A", 100, 2, "a two"),
            dsent("cam-B", 98, 2, "b one"),
            dsent("cam-A", 96, 2, "a one"),
        ];
        let convos = group_conversations(&desc, GAP, 8, 40, 4000);
        assert_eq!(convos.len(), 2, "one conversation per device");
        for c in &convos {
            let dev = &c[0].device_id;
            assert!(c.iter().all(|s| &s.device_id == dev), "no cross-device mixing");
            assert_eq!(c.len(), 2);
        }
    }

    #[test]
    fn groups_keep_most_recent_convos_and_respect_budget() {
        // Three conversations; max_convos = 2 keeps the two most recent, narrated oldest-first.
        let desc = vec![
            sent(2000, 2, "third"),
            sent(1000, 2, "second"),
            sent(100, 2, "first"),
        ];
        let convos = group_conversations(&desc, GAP, 2, 40, 4000);
        let texts: Vec<&str> = convos.iter().map(|c| c[0].text.as_str()).collect();
        assert_eq!(texts, vec!["second", "third"], "oldest of the kept pair first");
        // A tiny char budget still keeps at least the newest conversation's first sentence.
        let convos = group_conversations(&desc, GAP, 3, 40, 1);
        assert_eq!(convos.len(), 1);
        assert_eq!(convos[0][0].text, "third");
        assert!(group_conversations(&[], GAP, 8, 40, 4000).is_empty());
    }
}
