//! Deterministic "life digest" — aggregate analytics over ONE target speaker's whole
//! recorded history, for the reflection coach agent.
//!
//! Why this exists: introspective questions ("how have my conversational skills been?",
//! "how have I been doing?") need aggregate statistics across the corpus, not the top-k
//! nearest snippets the grounded agent returns. A small local LLM also cannot do
//! arithmetic reliably, so EVERY number is computed here in SQL/Rust; the LLM only
//! narrates the rendered digest (see `render_digest`).
//!
//! Three schema realities shape every query (verified against the migrations + worker):
//!   1. `sentiment` and `speaker_id` are SEGMENT-level, denormalized onto every sentence
//!      of the segment. All aggregation collapses to one row per `segment_id` first, or a
//!      5-sentence segment would be counted 5×.
//!   2. There is NO conversations table / turn_id — conversations are derived on the fly by
//!      gap-grouping `start_unix_nanos` per device timeline (a silence > gap = a new convo).
//!   3. The table is partitioned by `created_at`, not `start_unix_nanos`, so time-window
//!      filters get no partition pruning; every query leads with an indexed predicate
//!      (`speaker_id`/`device_id` + `start_unix_nanos`).

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use sqlx::{PgPool, Row};

use crate::config::RagConfig;
use crate::retrieve::Source;

/// Tuning for a digest computation, derived from `RagConfig`.
#[derive(Debug, Clone)]
pub struct DigestConfig {
    pub gap_threshold_nanos: i64,
    pub top_interlocutors: usize,
    /// Fixed offset (seconds) applied before hour/day/week bucketing (nanos are UTC).
    pub tz_offset_secs: i64,
    /// Below this many of the target's segments (or < 3 active days) we decline to draw
    /// conclusions rather than narrate noise.
    pub min_segments: i64,
    pub max_weeks: usize,
    pub statement_timeout_ms: i64,
    pub excerpt_max_chars: usize,
}

impl DigestConfig {
    pub fn from_rag(cfg: &RagConfig) -> Self {
        Self {
            gap_threshold_nanos: cfg.conversation_gap_secs.max(1) * 1_000_000_000,
            top_interlocutors: 5,
            tz_offset_secs: cfg.analysis_tz_offset_secs,
            min_segments: 20,
            max_weeks: 13,
            statement_timeout_ms: cfg.query_timeout_ms,
            excerpt_max_chars: 200,
        }
    }
}

/// Absolute analysis window (UTC nanos), resolved before any query runs.
#[derive(Debug, Clone, Copy)]
pub struct AnalysisWindow {
    pub after_unix_nanos: i64,
    pub before_unix_nanos: i64,
}

impl AnalysisWindow {
    /// The last `days` ending now.
    pub fn last_days(days: i64) -> Self {
        let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
        let span = days
            .max(1)
            .saturating_mul(86_400)
            .saturating_mul(1_000_000_000);
        Self {
            after_unix_nanos: now.saturating_sub(span),
            before_unix_nanos: now,
        }
    }

    /// Resolve a window from optional request bounds, defaulting to the last `default_days`.
    pub fn resolve(after: Option<i64>, before: Option<i64>, default_days: i64) -> Self {
        let base = Self::last_days(default_days);
        Self {
            after_unix_nanos: after.unwrap_or(base.after_unix_nanos),
            before_unix_nanos: before.unwrap_or(base.before_unix_nanos),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Coverage {
    pub first_nanos: Option<i64>,
    pub last_nanos: Option<i64>,
    pub active_days: i64,
    pub utterances: i64,
    pub segments: i64,
    pub speaking_nanos: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TalkBalance {
    /// target speaking time / total elapsed conversation time (0..1).
    pub talk_ratio: f64,
    pub median_convo_ratio: f64,
    pub conversations: i64,
    /// share of spoken audio in your conversations that we couldn't attribute (confidence).
    pub unattributed_share: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SpeechStyle {
    pub avg_utterance_chars: f64,
    pub avg_utterance_secs: f64,
    pub longest_monologue_secs: f64,
    pub question_rate: f64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SentimentDist {
    pub pos: i64,
    pub neu: i64,
    pub neg: i64,
    pub n_null: i64,
}

impl SentimentDist {
    fn classified(&self) -> i64 {
        self.pos + self.neu + self.neg
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WeekPoint {
    pub week_start_nanos: i64,
    pub n: i64,
    pub pct_pos: f64,
    pub pct_neg: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Mood {
    pub own: SentimentDist,
    pub weekly: Vec<WeekPoint>,
    /// Sentiment of OTHER speakers within the target's conversations (correlation only).
    pub others_in_your_convos: SentimentDist,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Interlocutor {
    pub speaker_id: String,
    pub name: Option<String>,
    pub shared_convos: i64,
    pub shared_nanos: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Rhythm {
    pub by_hour: [i64; 24],
    pub by_dow: [i64; 7],
    pub convos_per_week: Vec<WeekPoint>,
    pub peak_hours: Vec<u8>,
    pub peak_dow: Option<u8>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LifeDigest {
    pub target_label: String,
    pub window: (i64, i64),
    pub enough_data: bool,
    pub coverage: Coverage,
    pub talk: TalkBalance,
    pub style: SpeechStyle,
    pub mood: Mood,
    pub interlocutors: Vec<Interlocutor>,
    pub rhythm: Rhythm,
    pub excerpts: Vec<Source>,
    /// The honesty block: what audio-only data can and cannot show. Handed to the LLM
    /// verbatim so it declines productivity and avoids overclaiming.
    pub limits: &'static str,
}

pub const LIMITS: &str = "LIMITS — what this audio-only analysis can and cannot show: \
It reflects recorded, transcribed, speaker-attributed speech only; conversations off-mic, on other \
devices, or where the speaker wasn't identified are absent or counted as unattributed. Sentiment is a \
coarse three-way classification of what was SAID, not how it felt or sounded; many segments are \
unclassified and are excluded, not guessed. \"Mood of others near you\" is a correlation within your \
conversations, never a claim you caused it. Hour-of-day uses a fixed timezone offset, not full DST. \
There is NO productivity, focus, task-completion, or output data here — audio transcripts do not measure \
productivity, so do not infer or comment on it; if asked, say plainly this can't speak to it.";

/// One row of the gap-grouped conversation rollup (Rust-side aggregation source).
struct ConvoRow {
    convo_start: i64,
    total_nanos: i64,
    target_spoken_nanos: i64,
    all_spoken_nanos: i64,
    unattributed_nanos: i64,
    max_target_run: i64,
    other_ids: Vec<String>,
    other_pos: i64,
    other_neu: i64,
    other_neg: i64,
}

/// Compute the digest for `target_ids` (speaker uuids as text, already resolved by the
/// caller). `question_embedding`, when present (chat path), pulls 1-2 on-topic excerpts.
pub async fn compute_digest(
    pool: &PgPool,
    target_ids: &[String],
    window: AnalysisWindow,
    cfg: &DigestConfig,
    question_embedding: Option<&[f32]>,
) -> anyhow::Result<LifeDigest> {
    let after = window.after_unix_nanos;
    let before = window.before_unix_nanos;
    let target = target_ids.to_vec();

    // Resolve a friendly label up front (used in the decline path too).
    let names = crate::speakers::name_map(pool, target_ids).await?;
    let target_label = names
        .values()
        .next()
        .cloned()
        .unwrap_or_else(|| "you".to_string());

    let mut tx = pool.begin().await?;
    let timeout = cfg.statement_timeout_ms.max(0);
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL statement_timeout = {timeout}"
    )))
    .execute(&mut *tx)
    .await?;

    // (1) Coverage + style in one indexed aggregate (sentence grain; '?'/length are
    // sentence properties, segment count via COUNT(DISTINCT)).
    let cov_row = sqlx::query(
        "SELECT MIN(start_unix_nanos) AS first_n, MAX(start_unix_nanos) AS last_n, \
                COUNT(*)::bigint AS utterances, \
                COUNT(DISTINCT segment_id)::bigint AS segments, \
                COUNT(DISTINCT ((start_unix_nanos + $4) / 86400000000000))::bigint AS active_days, \
                COALESCE(SUM(GREATEST(end_unix_nanos - start_unix_nanos, 0)), 0)::bigint AS speaking_nanos, \
                COALESCE(AVG(char_length(text)), 0)::float8 AS avg_chars, \
                COALESCE(AVG(GREATEST(end_unix_nanos - start_unix_nanos, 0)), 0)::float8 AS avg_dur, \
                COALESCE(AVG((position('?' in text) > 0)::int::float8), 0)::float8 AS question_rate \
         FROM transcript_sentences \
         WHERE speaker_id = ANY($1::text[]) AND start_unix_nanos >= $2 AND start_unix_nanos < $3",
    )
    .bind(&target)
    .bind(after)
    .bind(before)
    // $4: tz offset (ns) so active_days buckets by LOCAL civil day, matching bucket() used by every
    // other rollup — otherwise the decline gate (active_days < 3) disagrees near UTC midnight.
    .bind(cfg.tz_offset_secs * 1_000_000_000)
    .fetch_one(&mut *tx)
    .await?;

    let coverage = Coverage {
        first_nanos: cov_row.try_get::<Option<i64>, _>("first_n")?,
        last_nanos: cov_row.try_get::<Option<i64>, _>("last_n")?,
        active_days: cov_row.try_get("active_days")?,
        utterances: cov_row.try_get("utterances")?,
        segments: cov_row.try_get("segments")?,
        speaking_nanos: cov_row.try_get("speaking_nanos")?,
    };

    // Graceful-decline gate: not enough recorded conversation to draw patterns.
    if coverage.segments < cfg.min_segments || coverage.active_days < 3 {
        tx.commit().await?;
        return Ok(decline_digest(target_label, (after, before), coverage));
    }

    let style = SpeechStyle {
        avg_utterance_chars: cov_row.try_get("avg_chars")?,
        avg_utterance_secs: cov_row.try_get::<f64, _>("avg_dur")? / 1e9,
        longest_monologue_secs: 0.0, // filled from the convo rollup below
        question_rate: cov_row.try_get("question_rate")?,
    };

    // (2) Target sentiment at SEGMENT grain (collapse the denormalization) for the own
    // distribution + weekly trend (bucketed Rust-side with the tz offset).
    let sent_rows = sqlx::query(
        "SELECT MIN(start_unix_nanos) AS seg_start, MIN(sentiment) AS sentiment \
         FROM transcript_sentences \
         WHERE speaker_id = ANY($1::text[]) AND start_unix_nanos >= $2 AND start_unix_nanos < $3 \
         GROUP BY segment_id",
    )
    .bind(&target)
    .bind(after)
    .bind(before)
    .fetch_all(&mut *tx)
    .await?;

    let (own_dist, weekly_sentiment) = roll_up_sentiment(&sent_rows, cfg)?;

    // (3) The gap-grouped conversation rollup — one query, per-conversation rows.
    let convo_rows = fetch_convos(&mut tx, &target, after, before, cfg.gap_threshold_nanos).await?;

    // (4) Excerpts (most recent / most positive / most negative + optional on-topic).
    let excerpts = fetch_excerpts(&mut tx, &target, after, before, cfg).await?;

    tx.commit().await?;

    // On-topic semantic excerpts (chat path) — separate, reuses the normal retrieval tx.
    let mut excerpts = excerpts;
    if let Some(emb) = question_embedding {
        if let Ok(mut topical) = retrieve_topical(pool, emb, &target, after, before, cfg).await {
            excerpts.append(&mut topical);
        }
    }

    // ---- Rust-side rollups from the per-conversation rows ----
    let (talk, others_dist, interlocutors_raw, rhythm, longest_monologue_secs) =
        roll_up_convos(&convo_rows, cfg);

    let mut style = style;
    style.longest_monologue_secs = longest_monologue_secs;

    // Resolve interlocutor names (batched).
    let inter_ids: Vec<String> = interlocutors_raw
        .iter()
        .map(|(id, _, _)| id.clone())
        .collect();
    let inter_names = crate::speakers::name_map(pool, &inter_ids).await?;
    let interlocutors: Vec<Interlocutor> = interlocutors_raw
        .into_iter()
        .map(|(id, convos, nanos)| Interlocutor {
            name: inter_names.get(&id).cloned(),
            speaker_id: id,
            shared_convos: convos,
            shared_nanos: nanos,
        })
        .collect();

    Ok(LifeDigest {
        target_label,
        window: (after, before),
        enough_data: true,
        coverage,
        talk,
        style,
        mood: Mood {
            own: own_dist,
            weekly: weekly_sentiment,
            others_in_your_convos: others_dist,
        },
        interlocutors,
        rhythm,
        excerpts,
        limits: LIMITS,
    })
}

fn decline_digest(target_label: String, window: (i64, i64), coverage: Coverage) -> LifeDigest {
    LifeDigest {
        target_label,
        window,
        enough_data: false,
        coverage,
        talk: TalkBalance {
            talk_ratio: 0.0,
            median_convo_ratio: 0.0,
            conversations: 0,
            unattributed_share: 0.0,
        },
        style: SpeechStyle {
            avg_utterance_chars: 0.0,
            avg_utterance_secs: 0.0,
            longest_monologue_secs: 0.0,
            question_rate: 0.0,
        },
        mood: Mood {
            own: SentimentDist::default(),
            weekly: vec![],
            others_in_your_convos: SentimentDist::default(),
        },
        interlocutors: vec![],
        rhythm: Rhythm {
            by_hour: [0; 24],
            by_dow: [0; 7],
            convos_per_week: vec![],
            peak_hours: vec![],
            peak_dow: None,
        },
        excerpts: vec![],
        limits: LIMITS,
    }
}

/// The gap-grouping query: collapse to segment grain, flag conversation boundaries and
/// speaker-change (monologue) runs per device timeline, then roll up per conversation that
/// the target participates in. Returns one `ConvoRow` per such conversation.
// Grouping key (0025): rows carrying a persisted conversation_id group EXACTLY by it
// (concurrent same-device conversations stay separate); NULL rows keep the legacy
// per-device gap sequence. COALESCE(conversation_id, device:gap_seq) — the dual-path rule.
async fn fetch_convos(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    target: &[String],
    after: i64,
    before: i64,
    gap_nanos: i64,
) -> anyhow::Result<Vec<ConvoRow>> {
    let rows = sqlx::query(
        "WITH \
         target_devices AS ( \
             SELECT DISTINCT device_id FROM transcript_sentences \
             WHERE speaker_id = ANY($1::text[]) AND start_unix_nanos >= $2 AND start_unix_nanos < $3 \
         ), \
         segs AS ( \
             SELECT ts.device_id, ts.segment_id, ts.speaker_id, \
                    (ts.speaker_id = ANY($1::text[])) AS is_target, \
                    MIN(ts.sentiment) AS sentiment, \
                    MIN(ts.conversation_id::text) AS conversation_id, \
                    MIN(ts.start_unix_nanos) AS seg_start, \
                    MAX(ts.end_unix_nanos) AS seg_end, \
                    COALESCE(SUM(GREATEST(ts.end_unix_nanos - ts.start_unix_nanos, 0)), 0)::bigint AS spoken_nanos \
             FROM transcript_sentences ts \
             WHERE ts.device_id IN (SELECT device_id FROM target_devices) \
               AND ts.start_unix_nanos >= $2 AND ts.start_unix_nanos < $3 \
             GROUP BY ts.device_id, ts.segment_id, ts.speaker_id \
         ), \
         flagged AS ( \
             SELECT s.*, \
                 CASE WHEN LAG(seg_end) OVER w IS NULL THEN 1 \
                      WHEN seg_start - LAG(seg_end) OVER w > $4 THEN 1 ELSE 0 END AS is_new, \
                 CASE WHEN LAG(is_target) OVER w IS NULL THEN 1 \
                      WHEN is_target IS DISTINCT FROM LAG(is_target) OVER w THEN 1 ELSE 0 END AS spk_change \
             FROM segs s \
             WINDOW w AS (PARTITION BY device_id ORDER BY seg_start, segment_id) \
         ), \
         numbered AS ( \
             SELECT f.*, \
                 COALESCE(f.conversation_id, \
                          f.device_id || ':' || (SUM(is_new) OVER (PARTITION BY device_id ORDER BY seg_start, segment_id))::text \
                 ) AS convo_key, \
                 SUM(CASE WHEN is_new = 1 OR spk_change = 1 THEN 1 ELSE 0 END) \
                     OVER (PARTITION BY device_id ORDER BY seg_start, segment_id) AS run_seq \
             FROM flagged f \
         ), \
         run_rollup AS ( \
             SELECT convo_key, run_seq, bool_and(is_target) AS run_is_target, \
                    SUM(spoken_nanos)::bigint AS run_nanos \
             FROM numbered GROUP BY convo_key, run_seq \
         ), \
         convo_monologue AS ( \
             SELECT convo_key, \
                    COALESCE(MAX(run_nanos) FILTER (WHERE run_is_target), 0)::bigint AS max_target_run \
             FROM run_rollup GROUP BY convo_key \
         ), \
         convos AS ( \
             SELECT convo_key, \
                    MIN(seg_start) AS convo_start, \
                    (MAX(seg_end) - MIN(seg_start))::bigint AS total_nanos, \
                    COALESCE(SUM(spoken_nanos) FILTER (WHERE is_target), 0)::bigint AS target_spoken_nanos, \
                    COALESCE(SUM(spoken_nanos), 0)::bigint AS all_spoken_nanos, \
                    COALESCE(SUM(spoken_nanos) FILTER (WHERE speaker_id IS NULL), 0)::bigint AS unattributed_nanos, \
                    bool_or(is_target) AS has_target, \
                    array_agg(DISTINCT speaker_id) FILTER (WHERE speaker_id IS NOT NULL AND NOT is_target) AS other_ids, \
                    COUNT(*) FILTER (WHERE NOT is_target AND sentiment = 'positive')::bigint AS other_pos, \
                    COUNT(*) FILTER (WHERE NOT is_target AND sentiment = 'neutral')::bigint AS other_neu, \
                    COUNT(*) FILTER (WHERE NOT is_target AND sentiment = 'negative')::bigint AS other_neg \
             FROM numbered GROUP BY convo_key \
         ) \
         SELECT c.convo_start, c.total_nanos, c.target_spoken_nanos, c.all_spoken_nanos, \
                c.unattributed_nanos, c.other_ids, c.other_pos, c.other_neu, c.other_neg, \
                m.max_target_run \
         FROM convos c JOIN convo_monologue m USING (convo_key) \
         WHERE c.has_target",
    )
    .bind(target.to_vec())
    .bind(after)
    .bind(before)
    .bind(gap_nanos)
    .fetch_all(&mut **tx)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(ConvoRow {
            convo_start: r.try_get("convo_start")?,
            total_nanos: r.try_get("total_nanos")?,
            target_spoken_nanos: r.try_get("target_spoken_nanos")?,
            all_spoken_nanos: r.try_get("all_spoken_nanos")?,
            unattributed_nanos: r.try_get("unattributed_nanos")?,
            max_target_run: r.try_get("max_target_run")?,
            other_ids: r
                .try_get::<Option<Vec<String>>, _>("other_ids")?
                .unwrap_or_default(),
            other_pos: r.try_get("other_pos")?,
            other_neu: r.try_get("other_neu")?,
            other_neg: r.try_get("other_neg")?,
        });
    }
    Ok(out)
}

type ConvoRollup = (
    TalkBalance,
    SentimentDist,
    Vec<(String, i64, i64)>,
    Rhythm,
    f64,
);

fn roll_up_convos(rows: &[ConvoRow], cfg: &DigestConfig) -> ConvoRollup {
    let mut sum_target = 0i64;
    let mut sum_total = 0i64;
    let mut sum_unattr = 0i64;
    let mut sum_all = 0i64;
    let mut others = SentimentDist::default();
    let mut ratios: Vec<f64> = Vec::new();
    let mut longest_run = 0i64;

    let mut by_hour = [0i64; 24];
    let mut by_dow = [0i64; 7];
    let mut week_counts: std::collections::BTreeMap<i64, i64> = std::collections::BTreeMap::new();
    // speaker_id -> (shared_convos, shared_nanos)
    let mut inter: std::collections::HashMap<String, (i64, i64)> = std::collections::HashMap::new();

    for c in rows {
        sum_target += c.target_spoken_nanos;
        sum_total += c.total_nanos;
        sum_unattr += c.unattributed_nanos;
        sum_all += c.all_spoken_nanos;
        others.pos += c.other_pos;
        others.neu += c.other_neu;
        others.neg += c.other_neg;
        longest_run = longest_run.max(c.max_target_run);
        if c.total_nanos > 0 {
            ratios.push(c.target_spoken_nanos as f64 / c.total_nanos as f64);
        }
        for id in &c.other_ids {
            let e = inter.entry(id.clone()).or_insert((0, 0));
            e.0 += 1;
            e.1 += c.total_nanos;
        }
        let (h, dow, wk) = bucket(c.convo_start, cfg.tz_offset_secs);
        by_hour[h as usize] += 1;
        by_dow[dow as usize] += 1;
        *week_counts.entry(wk).or_insert(0) += 1;
    }

    let talk = TalkBalance {
        talk_ratio: ratio(sum_target, sum_total),
        median_convo_ratio: median(&mut ratios),
        conversations: rows.len() as i64,
        unattributed_share: ratio(sum_unattr, sum_all),
    };

    // Top-N interlocutors by shared conversations then shared time.
    let mut inter_vec: Vec<(String, i64, i64)> =
        inter.into_iter().map(|(id, (n, t))| (id, n, t)).collect();
    inter_vec.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));
    inter_vec.truncate(cfg.top_interlocutors);

    let convos_per_week: Vec<WeekPoint> = week_counts
        .into_iter()
        .rev()
        .take(cfg.max_weeks)
        .map(|(wk, n)| WeekPoint {
            week_start_nanos: wk,
            n,
            pct_pos: 0.0,
            pct_neg: 0.0,
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let rhythm = Rhythm {
        peak_hours: top_indices(&by_hour, 2)
            .into_iter()
            .map(|i| i as u8)
            .collect(),
        peak_dow: top_indices(&by_dow, 1).first().map(|&i| i as u8),
        by_hour,
        by_dow,
        convos_per_week,
    };

    (talk, others, inter_vec, rhythm, longest_run as f64 / 1e9)
}

fn roll_up_sentiment(
    rows: &[sqlx::postgres::PgRow],
    cfg: &DigestConfig,
) -> anyhow::Result<(SentimentDist, Vec<WeekPoint>)> {
    let mut dist = SentimentDist::default();
    // week -> (pos, neg, classified)
    let mut weeks: std::collections::BTreeMap<i64, (i64, i64, i64)> =
        std::collections::BTreeMap::new();
    for r in rows {
        let seg_start: i64 = r.try_get("seg_start")?;
        let sentiment: Option<String> = r.try_get("sentiment")?;
        let (_, _, wk) = bucket(seg_start, cfg.tz_offset_secs);
        let e = weeks.entry(wk).or_insert((0, 0, 0));
        match sentiment.as_deref() {
            Some("positive") => {
                dist.pos += 1;
                e.0 += 1;
                e.2 += 1;
            }
            Some("neutral") => {
                dist.neu += 1;
                e.2 += 1;
            }
            Some("negative") => {
                dist.neg += 1;
                e.1 += 1;
                e.2 += 1;
            }
            _ => {
                dist.n_null += 1;
            }
        }
    }
    let weekly: Vec<WeekPoint> = weeks
        .into_iter()
        .rev()
        .take(cfg.max_weeks)
        .map(|(wk, (pos, neg, n))| WeekPoint {
            week_start_nanos: wk,
            n,
            pct_pos: if n > 0 {
                100.0 * pos as f64 / n as f64
            } else {
                0.0
            },
            pct_neg: if n > 0 {
                100.0 * neg as f64 / n as f64
            } else {
                0.0
            },
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Ok((dist, weekly))
}

/// Most-recent / most-positive / most-negative target utterances as citations.
async fn fetch_excerpts(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    target: &[String],
    after: i64,
    before: i64,
    cfg: &DigestConfig,
) -> anyhow::Result<Vec<Source>> {
    let rows = sqlx::query(
        "(SELECT segment_id, device_id, text, start_unix_nanos, speaker_id FROM transcript_sentences \
           WHERE speaker_id = ANY($1::text[]) AND start_unix_nanos >= $2 AND start_unix_nanos < $3 \
             AND char_length(text) > 8 \
           ORDER BY start_unix_nanos DESC LIMIT 1) \
         UNION ALL \
         (SELECT segment_id, device_id, text, start_unix_nanos, speaker_id FROM transcript_sentences \
           WHERE speaker_id = ANY($1::text[]) AND start_unix_nanos >= $2 AND start_unix_nanos < $3 \
             AND sentiment = 'positive' AND char_length(text) > 8 \
           ORDER BY char_length(text) DESC LIMIT 1) \
         UNION ALL \
         (SELECT segment_id, device_id, text, start_unix_nanos, speaker_id FROM transcript_sentences \
           WHERE speaker_id = ANY($1::text[]) AND start_unix_nanos >= $2 AND start_unix_nanos < $3 \
             AND sentiment = 'negative' AND char_length(text) > 8 \
           ORDER BY char_length(text) DESC LIMIT 1)",
    )
    .bind(target.to_vec())
    .bind(after)
    .bind(before)
    .fetch_all(&mut **tx)
    .await?;

    let mut out: Vec<Source> = Vec::new();
    let mut seen: std::collections::HashSet<(uuid::Uuid, i64)> = std::collections::HashSet::new();
    for r in rows {
        let segment_id: uuid::Uuid = r.try_get("segment_id")?;
        let start: i64 = r.try_get("start_unix_nanos")?;
        if !seen.insert((segment_id, start)) {
            continue; // dedup if the same line is both newest and most-positive, etc.
        }
        let mut text: String = r.try_get::<Option<String>, _>("text")?.unwrap_or_default();
        truncate_chars(&mut text, cfg.excerpt_max_chars);
        out.push(Source {
            segment_id,
            device_id: r
                .try_get::<Option<String>, _>("device_id")?
                .unwrap_or_default(),
            text,
            start_unix_nanos: start,
            distance: 0.0,
            speaker_id: r.try_get::<Option<String>, _>("speaker_id")?,
            speaker_name: None,
            time_label: String::new(),
            visual_context: None,
            conversation_id: None,
        });
    }
    Ok(out)
}

/// On-topic excerpts for the chat path: semantic NN restricted to the target speaker.
async fn retrieve_topical(
    pool: &PgPool,
    embedding: &[f32],
    target: &[String],
    after: i64,
    before: i64,
    cfg: &DigestConfig,
) -> anyhow::Result<Vec<Source>> {
    let filters = crate::retrieve::Filters {
        device_id: None,
        after_unix_nanos: Some(after),
        before_unix_nanos: Some(before),
        speaker_id: Some(target.to_vec()),
    };
    let tuning = crate::retrieve::Tuning {
        ef_search: 400,
        statement_timeout_ms: cfg.statement_timeout_ms,
    };
    let mut sources = crate::retrieve::nearest(pool, embedding, 2, &tuning, &filters).await?;
    for s in &mut sources {
        truncate_chars(&mut s.text, cfg.excerpt_max_chars);
    }
    Ok(sources)
}

// ---- small pure helpers (unit-tested) -------------------------------------------------

fn ratio(part: i64, whole: i64) -> f64 {
    if whole > 0 {
        part as f64 / whole as f64
    } else {
        0.0
    }
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

/// Indices of the `k` largest non-zero buckets, descending by count.
pub(crate) fn top_indices(counts: &[i64], k: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..counts.len()).filter(|&i| counts[i] > 0).collect();
    idx.sort_by(|&a, &b| counts[b].cmp(&counts[a]));
    idx.truncate(k);
    idx
}

/// Bucket a UTC nanos timestamp (with a fixed offset) into (hour, weekday0=Mon, week_start_nanos).
pub(crate) fn bucket(nanos: i64, tz_offset_secs: i64) -> (u32, u32, i64) {
    let dt: DateTime<Utc> =
        DateTime::from_timestamp_nanos(nanos) + Duration::seconds(tz_offset_secs);
    let hour = dt.hour();
    let dow = dt.weekday().num_days_from_monday();
    // Start of the local week (Monday 00:00), expressed back as nanos for stable bucketing.
    let date = dt.date_naive();
    let monday = date - Duration::days(dow as i64);
    let week_start = monday.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc();
    let week_nanos = week_start.timestamp_nanos_opt().unwrap_or(0) - tz_offset_secs * 1_000_000_000;
    (hour, dow, week_nanos)
}

fn truncate_chars(s: &mut String, max: usize) {
    if s.chars().count() > max {
        let truncated: String = s.chars().take(max).collect();
        *s = format!("{}…", truncated.trim_end());
    } else {
        *s = s.trim().to_string();
    }
}

// ---- rendering ------------------------------------------------------------------------

/// Render a compact, fixed-size labeled block for the LLM to narrate. All arithmetic is
/// already done; numbers are pre-formatted so the model never computes.
pub fn render_digest(d: &LifeDigest) -> String {
    let (after, before) = d.window;
    let span_days = ((before - after) / (86_400 * 1_000_000_000)).max(0);

    if !d.enough_data {
        return format!(
            "LIFE DIGEST for {who} (last {days} days)\n\
             NOT ENOUGH DATA: only {utt} transcribed utterances over {segs} segments and \
             {days_active} active day(s) are attributed to this speaker in this window — too \
             little to draw reliable conclusions about conversational skills, mood, or social \
             patterns.\n{limits}",
            who = d.target_label,
            days = span_days,
            utt = d.coverage.utterances,
            segs = d.coverage.segments,
            days_active = d.coverage.active_days,
            limits = d.limits,
        );
    }

    let mut out = String::new();
    out.push_str(&format!(
        "LIFE DIGEST for {who} (last {days} days)\n",
        who = d.target_label,
        days = span_days
    ));
    out.push_str(&format!(
        "COVERAGE: {days_active} active days, {utt} utterances over {convos} conversations; ~{spk} speaking time.\n",
        days_active = d.coverage.active_days,
        utt = d.coverage.utterances,
        convos = d.talk.conversations,
        spk = fmt_dur(d.coverage.speaking_nanos),
    ));

    let listen_note = if d.talk.talk_ratio < 0.45 {
        "you listen more than you talk"
    } else if d.talk.talk_ratio > 0.6 {
        "you do most of the talking"
    } else {
        "fairly balanced give-and-take"
    };
    out.push_str(&format!(
        "TALK BALANCE: you spoke {pct}% of conversation time ({note}); median per conversation {med}%.",
        pct = pct_str(d.talk.talk_ratio),
        note = listen_note,
        med = pct_str(d.talk.median_convo_ratio),
    ));
    if d.talk.unattributed_share > 0.25 {
        out.push_str(&format!(
            " ({}% of conversation audio was unattributed, so this is approximate.)",
            pct_str(d.talk.unattributed_share)
        ));
    }
    out.push('\n');

    out.push_str(&format!(
        "STYLE: average utterance ~{words} words / {secs:.1}s; longest unbroken stretch {mono:.0}s; \
         {q}% of your utterances asked a question.\n",
        words = (d.style.avg_utterance_chars / 5.0).round() as i64,
        secs = d.style.avg_utterance_secs,
        mono = d.style.longest_monologue_secs,
        q = (d.style.question_rate * 100.0).round() as i64,
    ));

    // Mood (own)
    let own = &d.mood.own;
    let oc = own.classified();
    out.push_str(&format!(
        "MOOD (you): {p}% positive / {n}% neutral / {g}% negative (of {c} classified; {nullp}% unclassified).\n",
        p = pct_of(own.pos, oc),
        n = pct_of(own.neu, oc),
        g = pct_of(own.neg, oc),
        c = oc,
        nullp = pct_of(own.n_null, oc + own.n_null),
    ));
    if d.mood.weekly.len() >= 2 {
        let series: Vec<String> = d
            .mood
            .weekly
            .iter()
            .map(|w| format!("{}", w.pct_pos.round() as i64))
            .collect();
        out.push_str(&format!(
            "MOOD TREND (weekly positive %): {}.\n",
            series.join(" → ")
        ));
    }
    let others = &d.mood.others_in_your_convos;
    let oth_c = others.classified();
    if oth_c > 0 {
        out.push_str(&format!(
            "MOOD (others in your conversations): {p}% positive / {n}% neutral / {g}% negative (correlation only).\n",
            p = pct_of(others.pos, oth_c),
            n = pct_of(others.neu, oth_c),
            g = pct_of(others.neg, oth_c),
        ));
    }

    // Social graph. Number distinct unnamed interlocutors (shared helper, same wording as
    // the chat path) so two unidentified people don't render as one repeated phrase.
    if !d.interlocutors.is_empty() {
        let mut unnamed = 0usize;
        let parts: Vec<String> = d
            .interlocutors
            .iter()
            .map(|i| {
                let name = match &i.name {
                    Some(n) => n.clone(),
                    None => {
                        unnamed += 1;
                        crate::speakers::unidentified_speaker_label(unnamed)
                    }
                };
                format!(
                    "{} ({} convos, {})",
                    name,
                    i.shared_convos,
                    fmt_dur(i.shared_nanos)
                )
            })
            .collect();
        out.push_str(&format!("TOP INTERLOCUTORS: {}.\n", parts.join(" · ")));
    }

    // Rhythm
    let dow_name = d.rhythm.peak_dow.map(weekday_name);
    if let Some(day) = dow_name {
        let hours: Vec<String> = d
            .rhythm
            .peak_hours
            .iter()
            .map(|h| format!("{:02}:00", h))
            .collect();
        let trend = week_trend(&d.rhythm.convos_per_week);
        out.push_str(&format!(
            "RHYTHM: busiest on {day}{hrs}; ~{perweek} conversations/week{trend}.\n",
            day = day,
            hrs = if hours.is_empty() {
                String::new()
            } else {
                format!(", around {}", hours.join(" and "))
            },
            perweek = avg_per_week(&d.rhythm.convos_per_week),
            trend = trend,
        ));
    }

    // Excerpts
    if !d.excerpts.is_empty() {
        out.push_str("EXAMPLE MOMENTS (your own words):\n");
        for s in &d.excerpts {
            out.push_str(&format!("  - \"{}\"\n", s.text.trim()));
        }
    }

    out.push_str(d.limits);
    out
}

fn fmt_dur(nanos: i64) -> String {
    let secs = nanos / 1_000_000_000;
    if secs >= 3600 {
        format!("{:.1}h", secs as f64 / 3600.0)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

fn pct_str(r: f64) -> i64 {
    (r * 100.0).round() as i64
}

fn pct_of(part: i64, whole: i64) -> i64 {
    if whole > 0 {
        (100.0 * part as f64 / whole as f64).round() as i64
    } else {
        0
    }
}

fn weekday_name(d: u8) -> &'static str {
    [
        "Mondays",
        "Tuesdays",
        "Wednesdays",
        "Thursdays",
        "Fridays",
        "Saturdays",
        "Sundays",
    ]
    .get(d as usize)
    .copied()
    .unwrap_or("some days")
}

fn avg_per_week(weeks: &[WeekPoint]) -> i64 {
    if weeks.is_empty() {
        return 0;
    }
    let total: i64 = weeks.iter().map(|w| w.n).sum();
    (total as f64 / weeks.len() as f64).round() as i64
}

fn week_trend(weeks: &[WeekPoint]) -> &'static str {
    if weeks.len() < 3 {
        return "";
    }
    let first = weeks[0].n;
    let last = weeks[weeks.len() - 1].n;
    if last as f64 > first as f64 * 1.2 {
        ", trending up"
    } else if (last as f64) < first as f64 * 0.8 {
        ", trending down"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_and_median() {
        assert_eq!(ratio(1, 4), 0.25);
        assert_eq!(ratio(1, 0), 0.0);
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 2.0, 3.0]), 2.5);
        assert_eq!(median(&mut []), 0.0);
    }

    #[test]
    fn top_indices_picks_largest_nonzero() {
        let counts = [0i64, 5, 0, 9, 1];
        assert_eq!(top_indices(&counts, 2), vec![3, 1]);
        assert_eq!(top_indices(&[0i64, 0, 0], 2), Vec::<usize>::new());
    }

    #[test]
    fn truncate_adds_ellipsis() {
        let mut s = "hello world this is long".to_string();
        truncate_chars(&mut s, 5);
        assert!(s.ends_with('…'));
        assert!(s.chars().count() <= 6);
        let mut short = "  hi  ".to_string();
        truncate_chars(&mut short, 50);
        assert_eq!(short, "hi");
    }

    #[test]
    fn bucket_is_stable_for_same_week() {
        // Two timestamps a day apart in the same week share a week bucket.
        let base = DateTime::parse_from_rfc3339("2026-06-24T12:00:00Z")
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap();
        let next = base + 86_400 * 1_000_000_000;
        let (_, _, w1) = bucket(base, 0);
        let (_, _, w2) = bucket(next, 0);
        assert_eq!(w1, w2);
    }

    #[test]
    fn decline_digest_renders_not_enough_data() {
        let cov = Coverage {
            first_nanos: None,
            last_nanos: None,
            active_days: 1,
            utterances: 3,
            segments: 3,
            speaking_nanos: 0,
        };
        let d = decline_digest("you".into(), (0, 86_400 * 1_000_000_000), cov);
        let r = render_digest(&d);
        assert!(r.contains("NOT ENOUGH DATA"));
        assert!(r.contains("productivity")); // limits block present
    }

    #[test]
    fn render_includes_headline_metrics() {
        let d = LifeDigest {
            target_label: "Bob".into(),
            window: (0, 90 * 86_400 * 1_000_000_000),
            enough_data: true,
            coverage: Coverage {
                first_nanos: Some(0),
                last_nanos: Some(1),
                active_days: 47,
                utterances: 1284,
                segments: 600,
                speaking_nanos: 23_000 * 1_000_000_000,
            },
            talk: TalkBalance {
                talk_ratio: 0.38,
                median_convo_ratio: 0.41,
                conversations: 312,
                unattributed_share: 0.12,
            },
            style: SpeechStyle {
                avg_utterance_chars: 70.0,
                avg_utterance_secs: 4.2,
                longest_monologue_secs: 58.0,
                question_rate: 0.22,
            },
            mood: Mood {
                own: SentimentDist {
                    pos: 300,
                    neu: 560,
                    neg: 110,
                    n_null: 314,
                },
                weekly: vec![
                    WeekPoint {
                        week_start_nanos: 0,
                        n: 10,
                        pct_pos: 28.0,
                        pct_neg: 10.0,
                    },
                    WeekPoint {
                        week_start_nanos: 1,
                        n: 12,
                        pct_pos: 22.0,
                        pct_neg: 14.0,
                    },
                    WeekPoint {
                        week_start_nanos: 2,
                        n: 9,
                        pct_pos: 17.0,
                        pct_neg: 20.0,
                    },
                ],
                others_in_your_convos: SentimentDist {
                    pos: 44,
                    neu: 49,
                    neg: 7,
                    n_null: 0,
                },
            },
            interlocutors: vec![
                Interlocutor {
                    speaker_id: "x".into(),
                    name: Some("Alice".into()),
                    shared_convos: 89,
                    shared_nanos: 7_560 * 1_000_000_000,
                },
                Interlocutor {
                    speaker_id: "y".into(),
                    name: None,
                    shared_convos: 12,
                    shared_nanos: 1_000 * 1_000_000_000,
                },
            ],
            rhythm: Rhythm {
                by_hour: [0; 24],
                by_dow: [0; 7],
                convos_per_week: vec![
                    WeekPoint {
                        week_start_nanos: 0,
                        n: 20,
                        pct_pos: 0.0,
                        pct_neg: 0.0,
                    },
                    WeekPoint {
                        week_start_nanos: 1,
                        n: 22,
                        pct_pos: 0.0,
                        pct_neg: 0.0,
                    },
                    WeekPoint {
                        week_start_nanos: 2,
                        n: 30,
                        pct_pos: 0.0,
                        pct_neg: 0.0,
                    },
                ],
                peak_hours: vec![18, 20],
                peak_dow: Some(3),
            },
            excerpts: vec![Source {
                segment_id: uuid::Uuid::nil(),
                device_id: "cam".into(),
                text: "best demo all year".into(),
                start_unix_nanos: 0,
                distance: 0.0,
                speaker_id: None,
                speaker_name: None,
                time_label: String::new(),
                visual_context: None,
                conversation_id: None,
            }],
            limits: LIMITS,
        };
        let r = render_digest(&d);
        assert!(r.contains("TALK BALANCE: you spoke 38%"));
        assert!(r.contains("you listen more than you talk"));
        assert!(r.contains("MOOD (you):"));
        assert!(r.contains("Alice (89 convos"));
        // The unnamed interlocutor renders with a distinct numbered label, not a bare repeat.
        assert!(r.contains("unidentified speaker 1 (12 convos"));
        assert!(r.contains("Thursdays"));
        assert!(r.contains("best demo all year"));
        assert!(r.contains("productivity"));
    }
}
