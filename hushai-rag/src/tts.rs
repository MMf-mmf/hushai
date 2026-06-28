//! Local neural text-to-speech for the assistant's spoken answers.
//!
//! Runs Kokoro-82M on-device (on the backend host) via sherpa-onnx + onnxruntime —
//! fully offline, consistent with the rest of the local stack. The Android client
//! does no synthesis; it POSTs the answer text to `/v1/tts` and plays the returned
//! WAV. The engine is loaded once at startup and held in [`AppState`] (warm for
//! every request), exactly like the embedder/LLM clients.
//!
//! Model files come from `local_dev/fetch_tts_model.sh` (the sherpa-onnx
//! `kokoro-en-v0_19` bundle): `model.onnx`, `voices.bin`, `tokens.txt`, and the
//! `espeak-ng-data/` phonemizer directory.

use std::path::Path;

use anyhow::{Context, anyhow};
use sherpa_onnx::{
    GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsKokoroModelConfig,
    OfflineTtsModelConfig,
};

/// A loaded Kokoro TTS engine plus the chosen speaker/speed.
///
/// `OfflineTts` is `Send + Sync` (the C engine is safe for concurrent inference),
/// so this is held behind an `Arc` and shared across requests without a mutex.
pub struct Tts {
    engine: OfflineTts,
    sid: i32,
    speed: f32,
    sample_rate: i32,
}

impl Tts {
    /// Load the Kokoro bundle from `model_dir` and select speaker `sid`.
    pub fn new(model_dir: &str, sid: i32, speed: f32, num_threads: i32) -> anyhow::Result<Self> {
        let dir = Path::new(model_dir);
        let model = dir.join("model.onnx");
        let voices = dir.join("voices.bin");
        let tokens = dir.join("tokens.txt");
        let data_dir = dir.join("espeak-ng-data");

        for (label, p) in [
            ("model.onnx", &model),
            ("voices.bin", &voices),
            ("tokens.txt", &tokens),
        ] {
            if !p.exists() {
                return Err(anyhow!(
                    "TTS model file {label} missing at {} — run local_dev/fetch_tts_model.sh \
                     (or set RAG_TTS_DIR / RAG_TTS_ENABLED=false)",
                    p.display()
                ));
            }
        }
        if !data_dir.is_dir() {
            return Err(anyhow!(
                "TTS espeak-ng-data directory missing at {}",
                data_dir.display()
            ));
        }

        let config = OfflineTtsConfig {
            model: OfflineTtsModelConfig {
                kokoro: OfflineTtsKokoroModelConfig {
                    model: Some(path_str(&model)?),
                    voices: Some(path_str(&voices)?),
                    tokens: Some(path_str(&tokens)?),
                    data_dir: Some(path_str(&data_dir)?),
                    ..Default::default()
                },
                num_threads: num_threads.max(1),
                ..Default::default()
            },
            ..Default::default()
        };

        let engine = OfflineTts::create(&config).ok_or_else(|| {
            anyhow!("sherpa-onnx failed to create the Kokoro TTS engine from {model_dir}")
        })?;

        let sample_rate = engine.sample_rate();
        let num_speakers = engine.num_speakers();
        if sid < 0 || (num_speakers > 0 && sid >= num_speakers) {
            return Err(anyhow!(
                "RAG_TTS_SID={sid} is out of range (model exposes {num_speakers} speakers)"
            ));
        }

        tracing::info!(
            model_dir,
            sid,
            speed,
            sample_rate,
            num_speakers,
            "Kokoro TTS engine loaded"
        );

        Ok(Self {
            engine,
            sid,
            speed,
            sample_rate,
        })
    }

    /// Output sample rate (Hz) — carried through to the WAV header (Kokoro = 24000).
    pub fn sample_rate(&self) -> i32 {
        self.sample_rate
    }

    /// Synthesize `text` and return a complete 16-bit mono PCM WAV byte buffer.
    ///
    /// CPU-bound and blocking — call from `spawn_blocking` in async handlers.
    pub fn synthesize_wav(&self, text: &str) -> anyhow::Result<Vec<u8>> {
        let gen_cfg = GenerationConfig {
            sid: self.sid,
            speed: self.speed,
            ..Default::default()
        };
        // No streaming callback: synthesize the whole utterance, then encode.
        let audio = self
            .engine
            .generate_with_config(text, &gen_cfg, None::<fn(&[f32], f32) -> bool>)
            .ok_or_else(|| anyhow!("TTS generation returned no audio for the given text"))?;

        let samples = audio.samples();
        if samples.is_empty() {
            return Err(anyhow!("TTS produced empty audio"));
        }
        encode_wav(samples, audio.sample_rate() as u32)
    }
}

fn path_str(p: &Path) -> anyhow::Result<String> {
    p.to_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("non-UTF-8 path: {}", p.display()))
}

/// Encode f32 samples in `[-1.0, 1.0]` as a mono 16-bit PCM WAV in memory.
fn encode_wav(samples: &[f32], sample_rate: u32) -> anyhow::Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec).context("init WAV writer")?;
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
            writer.write_sample(v).context("write WAV sample")?;
        }
        writer.finalize().context("finalize WAV")?;
    }
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_has_riff_header_and_expected_size() {
        let samples = vec![0.0f32, 0.5, -0.5, 1.0, -1.0];
        let wav = encode_wav(&samples, 24000).unwrap();
        // RIFF/WAVE magic.
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        // 44-byte canonical header + 2 bytes per 16-bit sample.
        assert_eq!(wav.len(), 44 + samples.len() * 2);
    }

    #[test]
    fn full_scale_sample_clamps_to_i16_max() {
        // +1.0 must not overflow i16 when scaled.
        let wav = encode_wav(&[1.0], 24000).unwrap();
        let lo = wav[44];
        let hi = wav[45];
        let sample = i16::from_le_bytes([lo, hi]);
        assert_eq!(sample, i16::MAX);
    }
}
