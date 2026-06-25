//! Local automatic speech recognition via whisper.cpp (`whisper-rs`).
//!
//! The `WhisperContext` is `Send + Sync` (an `Arc` over the model), so it is shared
//! across worker tasks; each transcription runs on a blocking thread and allocates
//! its own per-call state. Output is utterance-level text with timestamps relative
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

#[derive(Clone)]
pub struct Transcriber {
    ctx: Arc<WhisperContext>,
}

impl Transcriber {
    /// Load a GGML whisper model from disk.
    pub fn new(model_path: &str) -> anyhow::Result<Self> {
        let ctx = WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
            .with_context(|| format!("loading whisper model at {model_path}"))?;
        Ok(Self { ctx: Arc::new(ctx) })
    }

    /// Transcribe 16 kHz mono f32 PCM. CPU-bound work runs on a blocking thread.
    /// Empty/no-audio input yields zero utterances (a valid "no speech" result).
    pub async fn transcribe(&self, pcm: Vec<f32>) -> anyhow::Result<Vec<Utterance>> {
        if pcm.is_empty() {
            return Ok(Vec::new());
        }
        let ctx = self.ctx.clone();
        tokio::task::spawn_blocking(move || transcribe_blocking(&ctx, &pcm))
            .await
            .context("joining whisper blocking task")?
    }
}

fn transcribe_blocking(ctx: &WhisperContext, pcm: &[f32]) -> anyhow::Result<Vec<Utterance>> {
    let mut state = ctx.create_state().context("creating whisper state")?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("en"));
    params.set_translate(false);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    let threads = std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(4);
    params.set_n_threads(threads);

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
