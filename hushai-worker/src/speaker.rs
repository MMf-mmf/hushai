//! Server-side speaker embeddings via sherpa-onnx (NVIDIA TitaNet-large, 192-dim).
//!
//! Mirrors `asr.rs:Transcriber` at the inference layer — a model loaded once, shared
//! across worker tasks, inference on `spawn_blocking` — with ONE key difference:
//! sherpa's `compute_speaker_embedding` takes `&mut self`, so we cannot share the
//! extractor through a plain `Arc` (as `asr.rs` shares `Arc<WhisperContext>` via `&self`).
//! We wrap it in `Arc<Mutex<…>>` and lock per call. At `WORKER_CONCURRENCY=2` on a 2s
//! cadence the brief serialization is negligible (and speaker match/mint is already
//! globally serialized by a pg advisory lock), and it avoids loading the ~100 MB model
//! once per worker.
//!
//! sherpa runs the mel-fbank front-end in C++, so we pass raw 16 kHz mono f32 PCM and get
//! back a 192-d vector, which we L2-normalize before returning (centroids + all cosine
//! comparisons assume unit vectors).

use std::sync::{Arc, Mutex};

use anyhow::Context;
use sherpa_rs::silero_vad::{SileroVad, SileroVadConfig};
use sherpa_rs::speaker_id::{EmbeddingExtractor, ExtractorConfig};

use crate::vad::{self, VadResult};

/// TitaNet-large output dimension. Asserted at load so a wrong model fails fast.
pub const SPEAKER_EMBED_DIM: usize = 192;

#[derive(Clone)]
pub struct SpeakerEmbedder {
    inner: Arc<Mutex<EmbeddingExtractor>>,
}

impl SpeakerEmbedder {
    /// Load the speaker ONNX model at `model_path`. sherpa auto-detects the NeMo
    /// front-end from the model's metadata; we only assert the output dim.
    pub fn new(model_path: &str) -> anyhow::Result<Self> {
        let cfg = ExtractorConfig {
            model: model_path.to_string(),
            provider: None, // sherpa picks the default provider (CPU on this box)
            num_threads: Some(2),
            debug: false,
        };
        let extractor = EmbeddingExtractor::new(cfg)
            .map_err(|e| anyhow::anyhow!("loading speaker model at {model_path}: {e}"))?;
        anyhow::ensure!(
            extractor.embedding_size == SPEAKER_EMBED_DIM,
            "speaker model embedding_size={} but expected {SPEAKER_EMBED_DIM}",
            extractor.embedding_size,
        );
        Ok(Self {
            inner: Arc::new(Mutex::new(extractor)),
        })
    }

    /// Embed 16 kHz mono f32 PCM into an L2-normalized 192-d vector. Blocking inference
    /// runs on a blocking thread; the mutex serializes concurrent worker calls.
    pub async fn embed(&self, pcm: &[f32]) -> anyhow::Result<Vec<f32>> {
        let inner = self.inner.clone();
        let samples = pcm.to_vec();
        let mut emb = tokio::task::spawn_blocking(move || {
            let mut ex = inner.lock().expect("speaker extractor mutex poisoned");
            ex.compute_speaker_embedding(samples, vad::SAMPLE_RATE)
                .map_err(|e| anyhow::anyhow!("computing speaker embedding: {e}"))
        })
        .await
        .context("joining speaker embed task")??;

        anyhow::ensure!(
            emb.len() == SPEAKER_EMBED_DIM,
            "speaker embedding had dim {} (expected {SPEAKER_EMBED_DIM})",
            emb.len(),
        );
        // A degenerate/near-silent input can make TitaNet's per-feature normalization
        // divide by ~zero variance and emit NaN/Inf. Refuse it (caller leaves speaker_id
        // NULL) so a non-finite vector never reaches a centroid running-mean.
        anyhow::ensure!(
            emb.iter().all(|x| x.is_finite()),
            "speaker embedding has non-finite values (degenerate/near-silent input)",
        );
        vad::l2_normalize(&mut emb);
        Ok(emb)
    }
}

/// Frame-level voice-activity detection (sherpa Silero VAD) — strips static and silence so
/// only speech reaches the TitaNet embedder. This is the core fix for static fragmenting
/// one person into many "unknown speaker" rows: embedding a time-slice that includes
/// background static produces a scattered voiceprint that misses the speaker's centroid and
/// mints a duplicate. We embed the concatenated speech-only PCM instead.
///
/// Silero is in the SAME already-linked sherpa-onnx native lib as TitaNet (no extra crate);
/// it only needs a small `silero_vad.onnx` model file (handled like the TitaNet model).
///
/// We construct a FRESH `SileroVad` per `detect` call rather than reusing one: the 0.6.8
/// Rust wrapper exposes `clear()` (buffer flush) but NOT the C-API's `Reset` (recurrent
/// model-state reset), so reusing one detector would bleed LSTM state between independent
/// segments and could bias static detection. Per-call construction is unambiguously correct;
/// the model is tiny and inference runs on a blocking thread.
#[derive(Clone)]
pub struct VoiceDetector {
    model_path: String,
    threshold: f32,
    min_silence_secs: f32,
    min_speech_secs: f32,
}

impl VoiceDetector {
    /// Validate the Silero model loads (fail fast at startup), then store the config for
    /// per-call construction.
    pub fn new(
        model_path: &str,
        threshold: f32,
        min_silence_secs: f32,
        min_speech_secs: f32,
    ) -> anyhow::Result<Self> {
        // Trial construction validates the model path + format up front.
        let _probe = SileroVad::new(
            Self::config(model_path, threshold, min_silence_secs, min_speech_secs),
            2.0,
        )
        .map_err(|e| anyhow::anyhow!("loading VAD model at {model_path}: {e}"))?;
        Ok(Self {
            model_path: model_path.to_string(),
            threshold,
            min_silence_secs,
            min_speech_secs,
        })
    }

    fn config(
        model_path: &str,
        threshold: f32,
        min_silence_secs: f32,
        min_speech_secs: f32,
    ) -> SileroVadConfig {
        SileroVadConfig {
            model: model_path.to_string(),
            min_silence_duration: min_silence_secs,
            min_speech_duration: min_speech_secs,
            // Large so a normal utterance is never force-split; segments are ~2s anyway.
            max_speech_duration: 20.0,
            threshold,
            sample_rate: vad::SAMPLE_RATE,
            window_size: 512, // Silero's required 16 kHz frame size
            provider: None,   // sherpa default (CPU on this box)
            num_threads: Some(1),
            debug: false,
        }
    }

    /// Run VAD over 16 kHz mono f32 PCM, returning the concatenated speech-only audio plus
    /// the SNR / voiced-fraction signals. Blocking inference runs on a blocking thread.
    pub async fn detect(&self, pcm: &[f32]) -> anyhow::Result<VadResult> {
        let model_path = self.model_path.clone();
        let threshold = self.threshold;
        let min_silence = self.min_silence_secs;
        let min_speech = self.min_speech_secs;
        let samples = pcm.to_vec();

        tokio::task::spawn_blocking(move || -> anyhow::Result<VadResult> {
            // Sum-of-squares over the WHOLE buffer, computed before `samples` is moved into
            // the detector, so the noise floor can be derived as the complement of speech.
            let total_sum_sq: f64 = samples.iter().map(|x| (*x as f64) * (*x as f64)).sum();
            let total_len = samples.len();

            // Buffer must hold the whole clip; size to it (+1s margin), floor 2s.
            let buffer_secs = (total_len as f32 / vad::SAMPLE_RATE as f32 + 1.0).max(2.0);
            let mut vad = SileroVad::new(
                Self::config(&model_path, threshold, min_silence, min_speech),
                buffer_secs,
            )
            .map_err(|e| anyhow::anyhow!("constructing VAD: {e}"))?;

            // Silero VAD MUST be fed in window_size (512) frames, draining completed speech
            // segments as we go. Feeding the whole buffer in one accept_waveform call makes
            // sherpa emit only a single tiny segment (~0.31s) regardless of input — verified by
            // the `vad_probe_real_speech` diagnostic (whole-buffer=0.31s vs chunked=4.41s on a
            // 14s clip). Chunk-feed + drain, then feed the tail, flush, and drain again.
            const WINDOW: usize = 512; // matches SileroVadConfig.window_size
            let mut speech: Vec<f32> = Vec::new();
            let mut start_sample = 0usize;
            let mut end_sample = 0usize;
            let mut first = true;
            let mut drain = |vad: &mut SileroVad, speech: &mut Vec<f32>| {
                while !vad.is_empty() {
                    let seg = vad.front();
                    vad.pop();
                    let s = seg.start.max(0) as usize;
                    let e = s + seg.samples.len();
                    if first {
                        start_sample = s;
                        first = false;
                    }
                    end_sample = e;
                    speech.extend_from_slice(&seg.samples);
                }
            };
            let mut i = 0usize;
            while i + WINDOW <= samples.len() {
                vad.accept_waveform(samples[i..i + WINDOW].to_vec());
                drain(&mut vad, &mut speech);
                i += WINDOW;
            }
            if i < samples.len() {
                vad.accept_waveform(samples[i..].to_vec()); // feed the sub-window tail
            }
            vad.flush(); // finalize any in-progress trailing speech segment
            drain(&mut vad, &mut speech);

            // Speech energy (numerator) and noise-floor energy (denominator) for SNR. The
            // kept speech samples are a subset of the buffer, so noise sum-of-squares is the
            // complement — no need to materialize the dropped samples.
            let speech_sum_sq: f64 = speech.iter().map(|x| (*x as f64) * (*x as f64)).sum();
            let speech_len = speech.len();
            let speech_rms = if speech_len > 0 {
                (speech_sum_sq / speech_len as f64).sqrt() as f32
            } else {
                0.0
            };
            let noise_len = total_len.saturating_sub(speech_len);
            let noise_rms = if noise_len > 0 {
                ((total_sum_sq - speech_sum_sq).max(0.0) / noise_len as f64).sqrt() as f32
            } else {
                0.0
            };
            let speech_secs = speech_len as f64 / vad::SAMPLE_RATE as f64;

            Ok(VadResult {
                speech,
                speech_secs,
                start_sample,
                end_sample,
                speech_rms,
                noise_rms,
            })
        })
        .await
        .context("joining VAD task")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test (B0 acceptance): the model loads and returns an L2-normalized 192-d
    /// vector for a 2s PCM fixture. Gated on `SPEAKER_MODEL_PATH` (skips cleanly when
    /// unset/missing), like the live-DB tests gate on `DATABASE_URL`.
    #[tokio::test]
    async fn loads_model_and_embeds_2s_fixture() {
        let Ok(path) = std::env::var("SPEAKER_MODEL_PATH") else {
            eprintln!("skipping speaker smoke test: SPEAKER_MODEL_PATH unset");
            return;
        };
        if !std::path::Path::new(&path).exists() {
            eprintln!("skipping speaker smoke test: {path} not found");
            return;
        }
        let embedder = SpeakerEmbedder::new(&path).expect("load speaker model");

        // 2 seconds of deterministic white noise at 16 kHz. (A pure tone is degenerate for
        // TitaNet's per-feature normalization and yields NaN; noise has variance in every
        // mel bin. This checks the load + inference + normalize path, not recognition.)
        let mut state: u32 = 0x1234_5678;
        let pcm: Vec<f32> = (0..32_000)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let unit = (state >> 8) as f32 / (1u32 << 24) as f32; // [0,1)
                (unit * 2.0 - 1.0) * 0.3
            })
            .collect();

        let emb = embedder.embed(&pcm).await.expect("embed fixture");
        assert_eq!(emb.len(), SPEAKER_EMBED_DIM, "expected a 192-d embedding");
        let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-3,
            "embedding should be L2-normalized, got norm={norm}"
        );
        assert!(
            emb.iter().all(|x| x.is_finite()),
            "embedding has non-finite values"
        );
    }

    /// Diagnostic: compare whole-buffer vs chunked-512 VAD feeding on REAL speech PCM.
    /// Gated on VAD_PROBE_PCM (path to 16k mono f32le) + VAD_MODEL_PATH. Run with --nocapture.
    #[tokio::test]
    async fn vad_probe_real_speech() {
        let (Ok(pcmpath), Ok(model)) = (std::env::var("VAD_PROBE_PCM"), std::env::var("VAD_MODEL_PATH")) else {
            eprintln!("skip vad_probe_real_speech: set VAD_PROBE_PCM + VAD_MODEL_PATH");
            return;
        };
        let bytes = std::fs::read(&pcmpath).expect("read pcm");
        let pcm: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        eprintln!("PCM: {} samples ({:.2}s)", pcm.len(), pcm.len() as f32 / 16000.0);
        let bufsecs = (pcm.len() as f32 / 16000.0 + 1.0).max(2.0);
        let cfg = || SileroVadConfig {
            model: model.clone(), min_silence_duration: 0.3, min_speech_duration: 0.25,
            max_speech_duration: 20.0, threshold: 0.5, sample_rate: 16000, window_size: 512,
            provider: None, num_threads: Some(1), debug: false,
        };
        // (A) whole buffer + flush (what detect() does today)
        {
            let mut vad = SileroVad::new(cfg(), bufsecs).unwrap();
            vad.accept_waveform(pcm.clone());
            vad.flush();
            let (mut n, mut tot) = (0usize, 0usize);
            while !vad.is_empty() { let s = vad.front(); vad.pop(); n += 1; tot += s.samples.len(); }
            eprintln!("(A) whole-buffer + flush : segments={n} speech={:.2}s", tot as f32 / 16000.0);
        }
        // (B) chunked 512 + drain + flush (canonical sherpa usage)
        {
            let mut vad = SileroVad::new(cfg(), bufsecs).unwrap();
            let (w, mut i, mut n, mut tot) = (512usize, 0usize, 0usize, 0usize);
            while i + w <= pcm.len() {
                vad.accept_waveform(pcm[i..i + w].to_vec());
                while !vad.is_empty() { let s = vad.front(); vad.pop(); n += 1; tot += s.samples.len(); }
                i += w;
            }
            vad.flush();
            while !vad.is_empty() { let s = vad.front(); vad.pop(); n += 1; tot += s.samples.len(); }
            eprintln!("(B) chunked-512 + flush  : segments={n} speech={:.2}s", tot as f32 / 16000.0);
        }
    }

    /// VAD smoke test: the Silero model loads and a silence-padded buffer yields less kept
    /// speech than its total length (i.e. silence was dropped). Gated on `VAD_MODEL_PATH`.
    #[tokio::test]
    async fn vad_loads_and_drops_silence() {
        let Ok(path) = std::env::var("VAD_MODEL_PATH") else {
            eprintln!("skipping VAD smoke test: VAD_MODEL_PATH unset");
            return;
        };
        if !std::path::Path::new(&path).exists() {
            eprintln!("skipping VAD smoke test: {path} not found");
            return;
        }
        let detector = VoiceDetector::new(&path, 0.5, 0.3, 0.25).expect("load VAD model");

        // 1s of digital silence (no speech) — detector should keep ~nothing.
        let silence = vec![0.0f32; 16_000];
        let vr = detector.detect(&silence).await.expect("vad silence");
        assert!(
            vr.speech_secs < 0.5,
            "pure silence should yield little/no speech, got {}s",
            vr.speech_secs
        );
        assert!(vr.speech.len() <= silence.len());
    }
}
