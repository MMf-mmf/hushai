//! Local automatic speech recognition via whisper.cpp (`whisper-rs`).
//!
//! The `WhisperContext` is `Send + Sync` (an `Arc` over the model), so it is shared
//! across worker tasks; each transcription runs on a blocking thread and allocates
//! its own per-call state — so concurrent transcriptions are genuinely parallel. The
//! per-call thread pool size (`n_threads`) is a configured CPU budget (see
//! `WorkerConfig::asr_n_threads`) rather than "all cores", so N parallel worker loops
//! don't oversubscribe the box. Output is utterance-level text with timestamps relative
//! to the start of the segment audio (milliseconds).

use std::sync::Arc;

use anyhow::{Context, anyhow};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// One whisper segment: text plus [start, end] relative to the audio start (ms).
#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// Whisper decode-quality / anti-hallucination params, exposed as `WHISPER_*` env knobs. Defaults
/// are whisper.cpp's C defaults — i.e. the shipped default decode is byte-identical to the validated
/// eval baseline (NO behavior change). These are a CALIBRATION surface, not a free win: the eval loop
/// showed that flipping `suppress_nst` true regressed `fdr_infamy` WER 0.562→0.625 (it shifts the
/// greedy path on noisy audio), so it stays off by default. To fight hallucinated tails on a noisy/
/// looped deployment, raise `logprob_thold` (less negative) / `entropy_thold` and/or set `suppress_nst`
/// true — then re-run `hushai-eval` against representative clips to confirm WER doesn't regress.
#[derive(Clone, Copy, Debug)]
pub struct DecodeQuality {
    pub no_speech_thold: f32,
    pub logprob_thold: f32,
    pub entropy_thold: f32,
    pub suppress_nst: bool,
}

impl Default for DecodeQuality {
    fn default() -> Self {
        Self {
            no_speech_thold: 0.6,
            logprob_thold: -1.0,
            entropy_thold: 2.4,
            suppress_nst: false,
        }
    }
}

#[derive(Clone)]
pub struct Transcriber {
    ctx: Arc<WhisperContext>,
    /// Threads per transcription (whisper `n_threads`). A configured CPU budget rather than
    /// "all cores", so concurrent worker loops share the box instead of oversubscribing it.
    n_threads: i32,
    quality: DecodeQuality,
}

impl Transcriber {
    /// Load a GGML whisper model from disk. `n_threads` is the per-call whisper thread budget
    /// (see `WorkerConfig::asr_n_threads`); clamped to at least 1.
    pub fn new(model_path: &str, n_threads: i32) -> anyhow::Result<Self> {
        let ctx = WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
            .with_context(|| format!("loading whisper model at {model_path}"))?;
        Ok(Self {
            ctx: Arc::new(ctx),
            n_threads: n_threads.max(1),
            quality: DecodeQuality::default(),
        })
    }

    /// Override the decode-quality / anti-hallucination params (from `WorkerConfig`).
    pub fn with_quality(mut self, quality: DecodeQuality) -> Self {
        self.quality = quality;
        self
    }

    /// Transcribe 16 kHz mono f32 PCM. CPU-bound work runs on a blocking thread.
    /// Empty/no-audio input yields zero utterances (a valid "no speech" result).
    pub async fn transcribe(&self, pcm: Vec<f32>) -> anyhow::Result<Vec<Utterance>> {
        if pcm.is_empty() {
            return Ok(Vec::new());
        }
        let ctx = self.ctx.clone();
        let n_threads = self.n_threads;
        let quality = self.quality;
        tokio::task::spawn_blocking(move || transcribe_blocking(&ctx, &pcm, n_threads, quality))
            .await
            .context("joining whisper blocking task")?
    }
}

fn transcribe_blocking(
    ctx: &WhisperContext,
    pcm: &[f32],
    n_threads: i32,
    quality: DecodeQuality,
) -> anyhow::Result<Vec<Utterance>> {
    let mut state = ctx.create_state().context("creating whisper state")?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("en"));
    params.set_translate(false);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_n_threads(n_threads.max(1));
    // Anti-hallucination decode-quality params (see `DecodeQuality`). suppress_nst drops non-speech
    // tokens; the tholds gate low-confidence/degenerate segments (defaults = no recall change).
    params.set_no_speech_thold(quality.no_speech_thold);
    params.set_logprob_thold(quality.logprob_thold);
    params.set_entropy_thold(quality.entropy_thold);
    params.set_suppress_nst(quality.suppress_nst);

    state
        .full(params, pcm)
        .map_err(|e| anyhow!("whisper inference failed: {e}"))?;

    let n = state
        .full_n_segments()
        .map_err(|e| anyhow!("whisper full_n_segments: {e}"))?;
    let mut utterances = Vec::with_capacity(n as usize);
    for i in 0..n {
        let text = state
            .full_get_segment_text_lossy(i)
            .map_err(|e| anyhow!("whisper segment text: {e}"))?;
        // whisper timestamps are in centiseconds; convert to milliseconds.
        let t0 = state
            .full_get_segment_t0(i)
            .map_err(|e| anyhow!("whisper segment t0: {e}"))?;
        let t1 = state
            .full_get_segment_t1(i)
            .map_err(|e| anyhow!("whisper segment t1: {e}"))?;
        utterances.push(Utterance {
            text,
            start_ms: t0 * 10,
            end_ms: t1 * 10,
        });
    }
    Ok(utterances)
}
