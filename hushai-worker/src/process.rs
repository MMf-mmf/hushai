//! The per-segment pipeline: resolve media -> ASR -> chunk -> embed -> atomic write.
//!
//! The DB write is idempotent per source segment: in one transaction we DELETE any
//! existing `transcript_sentences` for the segment, insert the freshly-computed rows,
//! and mark the status `done`. Re-processing replaces, never duplicates; a no-speech
//! segment writes zero sentences and is still marked `done`.

use anyhow::Context;
use hushai_backend::observe;
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

/// Histogram name for per-stage latency; lane is always "audio" in this module.
const STAGE: &str = "hushai_worker_stage_seconds";

use crate::asr::Transcriber;
use crate::chunk::{self, Sentence};
use crate::config::WorkerConfig;
use crate::embed::{self, Embedder};
use crate::media;
use crate::sentiment::SentimentClassifier;
use crate::speaker::{SpeakerEmbedder, VoiceDetector};
use crate::speaker_match::{self, SpeakerMatchConfig, SpeakerWrite};
use crate::vad::{self, SpeakerQuality};

/// 16 kHz mono => one sample is exactly 62_500 ns. Used to map VAD speech bounds (sample
/// indices into the segment PCM) to absolute capture nanos.
const NANOS_PER_SAMPLE: i64 = 1_000_000_000 / vad::SAMPLE_RATE as i64;

/// Run the full pipeline for one claimed segment. Returns the number of sentences written.
///
/// Instrumented so every log emitted during processing — including the best-effort warnings deeper
/// in the call tree (VAD/quality/sentiment/event production) — carries `segment_id` and `lane`,
/// making one segment's path filterable in production logs. Each fallible stage is `.context`-tagged
/// so the top-level "segment failed" error names the stage that failed.
#[tracing::instrument(
    skip_all,
    fields(segment_id = %segment_id, lane = "audio"),
)]
pub async fn process_segment(
    pool: &PgPool,
    transcriber: &Transcriber,
    embedder: &Embedder,
    sentiment_clf: &SentimentClassifier,
    speaker_embedder: &SpeakerEmbedder,
    voice_detector: &VoiceDetector,
    cfg: &WorkerConfig,
    segment_id: Uuid,
) -> anyhow::Result<usize> {
    // Whole-pipeline wall-clock for `hushai_worker_segment_seconds` (recorded at the success path
    // below). Stage timers (`hushai_worker_stage_seconds`) wrap each step so the load-test can
    // attribute per-camera cost; their overhead is a single `Instant` read each (negligible).
    let started = std::time::Instant::now();

    let seg = {
        let _t = observe::StageTimer::start(STAGE, &[("lane", "audio"), ("stage", "load_segment")]);
        media::load_segment(pool, segment_id).await.context("load_segment")?
    };
    let pcm = {
        let _t = observe::StageTimer::start(STAGE, &[("lane", "audio"), ("stage", "extract_pcm")]);
        media::extract_pcm(cfg, &seg).await.context("extract_pcm")?
    };
    // The speaker embedder runs VAD over the raw PCM (independent of whisper), so retain a
    // copy before `transcribe` moves `pcm`. One small clone per segment (~128 KB at 2s/16
    // kHz) keeps asr.rs + its tests untouched.
    let pcm_for_speaker = pcm.clone();

    // Skip-silent gate: whisper is the dominant per-segment cost, so a segment with no speech
    // should never reach it. Two stages, cheapest first — a free RMS floor short-circuits dead
    // air without the VAD model, else the Silero VAD (already loaded for speaker work) makes the
    // definitive call. Fails OPEN: a VAD error falls through to normal ASR so a transient model
    // fault never silently drops real audio. Skipping leaves the segment's blob + status rows
    // intact, so it does NOT remove it from a later segment's speaker window
    // (`build_speaker_window` reads neighbor PCM by sequence, not by transcript) — don't "fix" that.
    if cfg.audio_silence_skip_enabled {
        let level = vad::rms(&pcm_for_speaker);
        let verdict = match vad::silence_verdict(
            level,
            None,
            cfg.audio_silence_rms_floor,
            cfg.audio_silence_min_speech_secs,
        ) {
            // Dead air on RMS alone — no need to run the VAD model.
            Some(reason) => Some((reason, None)),
            // Energy present: let the VAD make the definitive speech/no-speech call.
            None => {
                let _t = observe::StageTimer::start(
                    STAGE,
                    &[("lane", "audio"), ("stage", "silence_gate")],
                );
                match voice_detector.detect(&pcm_for_speaker).await {
                    Ok(vr) => vad::silence_verdict(
                        level,
                        Some(vr.speech_secs),
                        cfg.audio_silence_rms_floor,
                        cfg.audio_silence_min_speech_secs,
                    )
                    .map(|reason| (reason, Some(vr.speech_secs))),
                    Err(e) => {
                        tracing::warn!(error = %e, "silence gate: VAD failed; proceeding with ASR");
                        None
                    }
                }
            }
        };
        if let Some((reason, speech_secs)) = verdict {
            // No transcribable speech. Write an empty transcript through the normal path: it
            // deletes any prior sentences AND writes the `quality='reject'` speaker tombstone
            // that stops `reconcile_missing_speaker_segments` from re-queueing this segment on
            // every restart, then marks status `done`. Skips whisper/sentiment/speaker/embed.
            {
                let _t = observe::StageTimer::start(
                    STAGE,
                    &[("lane", "audio"), ("stage", "write_transcript")],
                );
                write_transcript(
                    pool,
                    segment_id,
                    &seg.device_id,
                    &[],
                    &[],
                    embedder.model_name(),
                    None,
                    &cfg.speaker_match_cfg(),
                )
                .await
                .context("write_transcript (silent)")?;
            }
            observe::counter(
                "hushai_audio_segments_skipped_silent_total",
                &[("reason", reason.as_str())],
            );
            tracing::debug!(
                %segment_id,
                rms = level,
                speech_secs = ?speech_secs,
                reason = reason.as_str(),
                "skipped silent segment (no ASR)"
            );
            observe::observe_duration(
                "hushai_worker_segment_seconds",
                &[("lane", "audio")],
                started.elapsed().as_secs_f64(),
            );
            record_capture_lag("audio", seg.capture_start_unix_nanos);
            return Ok(0);
        }
    }

    let utterances = {
        let _t = observe::StageTimer::start(STAGE, &[("lane", "audio"), ("stage", "transcribe")]);
        transcriber.transcribe(pcm).await.context("transcribe")?
    };
    let mut sentences = chunk::chunk_into_sentences(&utterances, seg.capture_start_unix_nanos);

    // Sentiment is a segment-level signal: classify the segment's transcript text once,
    // then denormalize the single label onto every sentence (same pattern as device_id).
    // Returns None (NULL) when disabled, empty, timed out, or unparseable.
    let segment_text = sentences
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let sentiment = {
        // Only time it when it actually runs, so a sentiment-disabled profile doesn't add a
        // misleading near-zero bucket to the histogram.
        let _t = cfg
            .sentiment_enabled
            .then(|| observe::StageTimer::start(STAGE, &[("lane", "audio"), ("stage", "sentiment")]));
        sentiment_clf.classify(&segment_text).await
    };
    for s in &mut sentences {
        s.sentiment = sentiment.clone();
    }

    // Speaker embedding (VAD-gated + multi-speaker refused). None => speaker_id stays NULL.
    // Skip entirely when there are no transcribable sentences: a segment with voiced audio
    // but only non-speech whisper output should not mint a speaker (it has no text to
    // attribute, and a NULL beats a text-less voiceprint cluttering the catalog).
    let speaker = if sentences.is_empty() {
        None
    } else {
        // Aggregate this segment with its contiguous same-stream predecessors so a short
        // (~2s) clip has enough speech for the VAD/quality gate to attribute it, instead of
        // rejecting every individual clip. Falls back to this segment alone when windowing
        // is disabled or there are no contiguous neighbors.
        let (window_pcm, window_start_nanos) = {
            let _t = observe::StageTimer::start(STAGE, &[("lane", "audio"), ("stage", "speaker_window")]);
            build_speaker_window(pool, cfg, &seg, pcm_for_speaker).await.context("speaker_window")?
        };
        let _t = observe::StageTimer::start(STAGE, &[("lane", "audio"), ("stage", "speaker_embed")]);
        compute_speaker_embedding(
            voice_detector,
            speaker_embedder,
            &window_pcm,
            window_start_nanos,
            cfg,
        )
        .await
    };

    let texts: Vec<String> = sentences.iter().map(|s| s.text.clone()).collect();
    let embeddings = {
        let _t = observe::StageTimer::start(STAGE, &[("lane", "audio"), ("stage", "embed")]);
        embedder.embed(texts).await.context("embed")?
    };

    let speaker_match_cfg = cfg.speaker_match_cfg();
    let speaker_id = {
        let _t = observe::StageTimer::start(STAGE, &[("lane", "audio"), ("stage", "write_transcript")]);
        write_transcript(
            pool,
            segment_id,
            &seg.device_id,
            &sentences,
            &embeddings,
            embedder.model_name(),
            speaker,
            &speaker_match_cfg,
        )
        .await
        .context("write_transcript")?
    };

    // Proactive layer (roadmap A3): materialize a `speech` event + evaluate alert rules. Guarded so
    // an event/alert failure can never fail the segment's core transcription.
    if cfg.events.enabled {
        let _t = observe::StageTimer::start(STAGE, &[("lane", "audio"), ("stage", "derive_events")]);
        if let Err(e) = crate::events_producer::derive_audio_events(
            pool,
            &seg,
            segment_id,
            &sentences,
            &sentiment,
            speaker_id,
            &cfg.events,
        )
        .await
        {
            tracing::warn!(%segment_id, error = %format!("{e:#}"), "audio event production failed; continuing");
        }
    }

    // Success-path latency: total per-segment wall-clock, and capture->done lag (incl. queue wait),
    // the "are we keeping up with realtime?" signal the load-test ramps against.
    observe::observe_duration(
        "hushai_worker_segment_seconds",
        &[("lane", "audio")],
        started.elapsed().as_secs_f64(),
    );
    record_capture_lag("audio", seg.capture_start_unix_nanos);

    Ok(sentences.len())
}

/// Record capture->done end-to-end latency (s) into `hushai_worker_capture_lag_seconds`.
/// `capture_start_unix_nanos` is the device wall clock, so this folds in capture, upload, queue
/// wait, and processing — exactly the realtime-lag the capacity test watches.
pub(crate) fn record_capture_lag(lane: &'static str, capture_start_unix_nanos: i64) {
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(capture_start_unix_nanos);
    let lag = (now_ns - capture_start_unix_nanos) as f64 / 1e9;
    if lag.is_finite() && lag >= 0.0 {
        observe::observe_duration("hushai_worker_capture_lag_seconds", &[("lane", lane)], lag);
    }
}

/// Compute one segment-level speaker embedding behind the accuracy guards, or `None` to
/// leave `speaker_id` NULL. Pipeline: VAD strips static/silence -> quality gate -> embed the
/// CLEANED speech. Returns None when: too little / too noisy speech (Reject), the cleaned
/// speech looks like two voices (multi-speaker refusal), VAD or embedding fails. The carried
/// `quality` controls whether the matcher may mint a NEW identity from this segment. Never
/// errors the pipeline.
async fn compute_speaker_embedding(
    detector: &VoiceDetector,
    embedder: &SpeakerEmbedder,
    pcm: &[f32],
    capture_start_unix_nanos: i64,
    cfg: &WorkerConfig,
) -> Option<SpeakerWrite> {
    let total_secs = pcm.len() as f64 / vad::SAMPLE_RATE as f64;
    let vr = match detector.detect(pcm).await {
        Ok(vr) => vr,
        Err(e) => {
            tracing::warn!(error = %e, "speaker: VAD failed; skipping speaker work");
            return None;
        }
    };

    // Quality gate on POST-VAD speech. Reject (too little/too noisy) => NULL, skip embedding.
    let q = vad::assess_quality(&vr, total_secs, &cfg.mint_gates());
    // Surface the quality signals for EVERY segment (not just Reject) so the mint gate that BINDS
    // (speech_secs vs snr_db vs voiced_frac) is observable and calibratable from real captures
    // rather than inferred. Zero behavior change. (added for hushai-eval speaker calibration)
    tracing::info!(
        speech_secs = q.speech_secs,
        voiced_frac = q.voiced_frac,
        snr_db = q.snr_db,
        total_secs,
        quality = ?q.quality,
        "speaker: quality assessed"
    );
    if q.quality == SpeakerQuality::Reject {
        tracing::debug!(
            secs = q.speech_secs,
            snr_db = q.snr_db,
            "speaker: rejected (too little/too noisy speech)"
        );
        return None;
    }

    // Multi-speaker refusal on CLEANED speech: split at the midpoint of the concatenated
    // speech; if the halves look like two voices, refuse (a blended embedding would
    // mis-attribute and poison a centroid). Only when there's enough speech for two
    // meaningful halves — running it on cleaned (not raw) audio also removes the old
    // static-driven false refusals.
    if vr.speech_secs >= 2.0 * cfg.speaker_min_speech_secs {
        let mid = vr.speech.len() / 2;
        let (first, second) = vr.speech.split_at(mid);
        if !first.is_empty() && !second.is_empty() {
            match tokio::try_join!(embedder.embed(first), embedder.embed(second)) {
                Ok((a, b)) => {
                    let spread = vad::cosine_distance(&a, &b);
                    if spread > cfg.speaker_split_threshold {
                        tracing::debug!(spread, "speaker: multi-speaker segment refused (NULL)");
                        return None;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "speaker: half-embed failed; skipping speaker work");
                    return None;
                }
            }
        }
    }

    match embedder.embed(&vr.speech).await {
        Ok(embedding) => Some(SpeakerWrite {
            embedding,
            start_unix_nanos: capture_start_unix_nanos + vr.start_sample as i64 * NANOS_PER_SAMPLE,
            end_unix_nanos: capture_start_unix_nanos + vr.end_sample as i64 * NANOS_PER_SAMPLE,
            quality: q.quality,
        }),
        Err(e) => {
            tracing::warn!(error = %e, "speaker: embedding failed; speaker_id NULL");
            None
        }
    }
}

/// Build the PCM the speaker stage embeds: this segment concatenated with its contiguous
/// same-stream predecessors (oldest-first), so short clips accumulate enough speech to clear
/// the VAD/quality gate. Returns `(window_pcm, window_start_capture_nanos)`. Reuses the
/// already-decoded `current_pcm` for this segment and decodes only the neighbors. Degrades
/// gracefully: disabled / no contiguous neighbors / a single-segment window all return this
/// segment alone, and a neighbor that fails to decode is skipped rather than failing the
/// segment. Deterministic (fixed audio + sequence), so reprocessing is idempotent.
async fn build_speaker_window(
    pool: &PgPool,
    cfg: &WorkerConfig,
    seg: &media::SegmentRow,
    current_pcm: Vec<f32>,
) -> anyhow::Result<(Vec<f32>, i64)> {
    if !cfg.speaker_window_enabled || cfg.speaker_window_max_segments <= 1 {
        return Ok((current_pcm, seg.capture_start_unix_nanos));
    }
    let candidates =
        media::load_window_candidates(pool, seg, cfg.speaker_window_max_segments).await?;
    let window = media::select_window(
        &candidates,
        cfg.speaker_window_target_secs,
        cfg.speaker_window_max_segments,
    );
    if window.len() <= 1 {
        return Ok((current_pcm, seg.capture_start_unix_nanos));
    }

    let window_start_nanos = window
        .first()
        .map(|s| s.capture_start_unix_nanos)
        .unwrap_or(seg.capture_start_unix_nanos);
    let mut pcm: Vec<f32> = Vec::new();
    for s in &window {
        if s.segment_id == seg.segment_id {
            pcm.extend_from_slice(&current_pcm);
        } else {
            match media::extract_pcm(cfg, s).await {
                Ok(p) => pcm.extend_from_slice(&p),
                Err(e) => tracing::debug!(
                    neighbor = %s.segment_id,
                    error = %e,
                    "speaker window: neighbor decode failed; shortening window"
                ),
            }
        }
    }
    tracing::debug!(
        segment = %seg.segment_id,
        window_segments = window.len(),
        window_secs = pcm.len() as f64 / vad::SAMPLE_RATE as f64,
        "speaker window built"
    );
    Ok((pcm, window_start_nanos))
}

/// Max rows per batched INSERT. Each row binds 11 params (8 base + sentiment, emotion,
/// speaker_id); 11 * 1000 = 11000, well under Postgres' 65535 bind-parameter limit even
/// for an implausibly long segment.
const ROWS_PER_INSERT: usize = 1000;

/// Record that the speaker stage ran for a segment but resolved no voice. Delete-then-insert
/// by segment_id (idempotent; the partitioned table can't carry a UNIQUE(segment_id)). The
/// row is a tombstone: `speaker_id` NULL, `embedding` NULL, `quality='reject'`.
async fn write_speaker_tombstone(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    segment_id: Uuid,
    device_id: &str,
) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM speaker_segments WHERE segment_id = $1")
        .bind(segment_id)
        .execute(&mut **tx)
        .await
        .context("clearing prior speaker_segment (tombstone)")?;
    sqlx::query(
        "INSERT INTO speaker_segments (segment_id, device_id, speaker_id, embedding, quality) \
         VALUES ($1, $2, NULL, NULL, 'reject')",
    )
    .bind(segment_id)
    .bind(device_id)
    .execute(&mut **tx)
    .await
    .context("inserting speaker tombstone")?;
    Ok(())
}

/// Atomically replace a segment's transcript, assign its speaker, and mark it `done`.
/// Idempotent: running twice with the same input leaves the same rows (delete-then-insert)
/// AND the same `speaker_id` with a byte-identical centroid (the speaker assignment reuses
/// the prior `speaker_segments` row and skips the running-mean update — see speaker_match).
///
/// `device_id` is denormalized onto each sentence so the RAG query can filter on the same
/// table as the HNSW index; `speaker_id` is likewise denormalized (segment-level, the same
/// value on every sentence). `sentences` and `embeddings` must be the same length and
/// aligned by index. `speaker` is `None` for VAD-gated / multi-speaker / silent segments.
/// Returns the resolved `speaker_id` (the value denormalized onto the sentences, `None` when the
/// segment is silent / VAD-rejected / multi-speaker) so the caller can attribute a `speech` event.
#[allow(clippy::too_many_arguments)]
pub async fn write_transcript(
    pool: &PgPool,
    segment_id: Uuid,
    device_id: &str,
    sentences: &[Sentence],
    embeddings: &[Vec<f32>],
    embed_model: &str,
    speaker: Option<SpeakerWrite>,
    speaker_match_cfg: &SpeakerMatchConfig,
) -> anyhow::Result<Option<Uuid>> {
    anyhow::ensure!(
        sentences.len() == embeddings.len(),
        "sentence/embedding count mismatch: {} vs {}",
        sentences.len(),
        embeddings.len()
    );
    // Validate every vector up front so a bad dim fails before we touch the DB.
    for emb in embeddings {
        embed::check_dim(emb)?;
    }

    let mut tx = pool.begin().await.context("begin transcript tx")?;

    // Speaker match/mint runs FIRST (before the transcript DELETE) because it reads the
    // durable prior assignment from speaker_segments, which the DELETE below does not touch
    // but which is the idempotency source of truth. Takes the global advisory lock itself.
    // The resolved id is denormalized (as text) onto every sentence row.
    let speaker_id: Option<Uuid> = match &speaker {
        Some(sp) => {
            speaker_match::assign_speaker(&mut tx, segment_id, device_id, sp, speaker_match_cfg)
                .await?
        }
        None => {
            // The speaker stage ran but produced no voiceprint (silent / VAD-rejected /
            // multi-speaker / embed-fail). Record a `quality='reject'` tombstone so the
            // startup reconcile sees speaker work WAS attempted and stops re-queueing this
            // segment on every restart. Inert to matching/clustering/centroid (all filter it
            // out), and `assign_speaker` ignores it so a later, better run can still attribute.
            write_speaker_tombstone(&mut tx, segment_id, device_id).await?;
            None
        }
    };
    // Denormalized as text on every sentence row (matches the text device_id / speaker_id columns).
    let speaker_id_text: Option<String> = speaker_id.map(|id| id.to_string());

    sqlx::query("DELETE FROM transcript_sentences WHERE segment_id = $1")
        .bind(segment_id)
        .execute(&mut *tx)
        .await
        .context("clearing prior sentences")?;

    // One multi-row INSERT per chunk (one round-trip) instead of one INSERT per sentence.
    for (s_chunk, e_chunk) in sentences
        .chunks(ROWS_PER_INSERT)
        .zip(embeddings.chunks(ROWS_PER_INSERT))
    {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            "INSERT INTO transcript_sentences \
             (segment_id, device_id, text, start_unix_nanos, end_unix_nanos, \
              embedding, embedding_model, embedding_dim, sentiment, emotion, speaker_id) ",
        );
        qb.push_values(s_chunk.iter().zip(e_chunk.iter()), |mut b, (s, emb)| {
            // Bind the embedding as a native pgvector::Vector (sqlx 0.9 binary protocol)
            // rather than a `[..]::vector` decimal text literal — less CPU + bandwidth
            // per row and no server-side re-parse. Encoding is byte-equivalent.
            // sentiment/emotion/speaker_id are segment-level signals denormalized onto
            // every sentence; NULL when unassigned (emotion is always NULL in v1).
            b.push_bind(segment_id)
                .push_bind(device_id)
                .push_bind(&s.text)
                .push_bind(s.start_unix_nanos)
                .push_bind(s.end_unix_nanos)
                .push_bind(pgvector::Vector::from(emb.clone()))
                .push_bind(embed_model)
                .push_bind(embed::EMBED_DIM as i32)
                .push_bind(s.sentiment.clone())
                .push_bind(s.emotion.clone())
                .push_bind(speaker_id_text.clone());
        });
        qb.build()
            .execute(&mut *tx)
            .await
            .context("inserting sentences")?;
    }

    sqlx::query(
        r#"
        UPDATE segment_transcription_status
           SET status = 'done', last_error = NULL, updated_at = now()
         WHERE segment_id = $1
        "#,
    )
    .bind(segment_id)
    .execute(&mut *tx)
    .await
    .context("marking status done")?;

    tx.commit().await.context("commit transcript tx")?;
    Ok(speaker_id)
}
