//! Speaker-accuracy guards + small vector helpers, kept pure so they unit-test directly.
//!
//! Segments are ~2s wall-clock with no speaker-turn alignment, so a raw segment often
//! holds < 1s of voiced speech surrounded by room tone, dead air, and — the bug this
//! module now fixes — background static. A real frame-level VAD (sherpa Silero, in
//! `speaker::VoiceDetector`) strips the non-speech first, producing a [`VadResult`] of
//! concatenated speech-only PCM plus the signals needed to decide *quality*. We then:
//!
//!   * GATE on post-VAD speech duration (skip speaker work when there's too little), and
//!   * classify [`SpeakerQuality`] (clean / marginal / reject) so the matcher may only mint
//!     a NEW identity from clean, high-SNR audio — marginal/noisy audio can attach to a
//!     known speaker but never spawns a duplicate "unknown speaker".
//!
//! (Earlier versions took the voiced span from whisper's timestamps and embedded the whole
//! slice including static — that is exactly what fragmented one person into many voices.)

use crate::asr::Utterance;

/// Pipeline PCM sample rate (16 kHz mono f32), shared with media::extract_pcm + sherpa.
pub const SAMPLE_RATE: u32 = 16_000;

const SAMPLES_PER_MS: i64 = SAMPLE_RATE as i64 / 1000; // 16

/// Voiced-speech bounds `(min_start_ms, max_end_ms)` across all utterances, or `None`
/// when there are no utterances or the span is non-positive (treated as "no speech").
pub fn voiced_bounds_ms(utterances: &[Utterance]) -> Option<(i64, i64)> {
    let start = utterances.iter().map(|u| u.start_ms).min()?;
    let end = utterances.iter().map(|u| u.end_ms).max()?;
    if end <= start {
        return None;
    }
    Some((start, end))
}

/// Duration of a voiced-bounds span in seconds.
pub fn span_secs((start_ms, end_ms): (i64, i64)) -> f64 {
    (end_ms - start_ms).max(0) as f64 / 1000.0
}

/// Slice 16 kHz mono PCM to `[start_ms, end_ms)`, clamped to the buffer. Used to embed the
/// voiced region (not the dead air) and to halve it for the multi-speaker check.
pub fn slice_ms(pcm: &[f32], start_ms: i64, end_ms: i64) -> &[f32] {
    let s = (start_ms.max(0) * SAMPLES_PER_MS) as usize;
    let e = (end_ms.max(0) * SAMPLES_PER_MS) as usize;
    let s = s.min(pcm.len());
    let e = e.min(pcm.len()).max(s);
    &pcm[s..e]
}

/// Normalize a vector to unit L2 length in place (no-op for a zero vector).
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Cosine distance (`1 - cosine_similarity`), robust to non-normalized inputs. Returns the
/// max distance `1.0` if either vector is zero or lengths differ.
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 1.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 1.0;
    }
    1.0 - (dot / (na * nb))
}

/// Root-mean-square amplitude of a PCM buffer (0.0 for empty). Used to estimate speech
/// energy vs the noise floor for the SNR signal.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|x| (*x as f64) * (*x as f64)).sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

/// The output of one VAD pass over a segment's PCM: the speech-only audio to embed plus
/// the signals [`assess_quality`] turns into a [`SpeakerQuality`]. All sample indices are
/// into the ORIGINAL 16 kHz buffer that was fed to the detector.
#[derive(Debug, Clone)]
pub struct VadResult {
    /// Concatenated speech-only PCM (all kept segments, in order), ready to embed.
    pub speech: Vec<f32>,
    /// Total kept speech duration in seconds (= speech.len() / SAMPLE_RATE).
    pub speech_secs: f64,
    /// Union speech bounds `[start_sample, end_sample)` for timestamp bookkeeping.
    pub start_sample: usize,
    pub end_sample: usize,
    /// Mean RMS over kept speech samples (SNR numerator).
    pub speech_rms: f32,
    /// Mean RMS over the dropped (non-speech) remainder (noise-floor estimate / SNR denom).
    pub noise_rms: f32,
}

/// What the matcher is allowed to do with a segment's embedding, given its input quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeakerQuality {
    /// Clean, long-enough, high-SNR speech: the matcher MAY mint a new identity.
    Mint,
    /// Real speech but below mint-confidence (low SNR / short / sparse). May ATTACH to a
    /// nearest existing speaker within threshold, but NEVER mints and NEVER updates a
    /// centroid. This is what stops static fragmenting one person into many voices.
    AttachOnly,
    /// Too little / too noisy to attribute at all — caller leaves speaker_id NULL.
    Reject,
}

/// Raw quality signals behind a [`SpeakerQuality`] verdict (kept for logging + retuning).
#[derive(Debug, Clone, Copy)]
pub struct QualitySignals {
    pub speech_secs: f64,
    pub voiced_frac: f64,
    pub snr_db: f32,
    pub quality: SpeakerQuality,
}

/// Thresholds for [`assess_quality`], built from `WorkerConfig`.
#[derive(Debug, Clone, Copy)]
pub struct MintGates {
    /// Below this many seconds of cleaned speech -> Reject (NULL).
    pub min_speech_secs: f64,
    /// Mint requires at least this much cleaned speech.
    pub mint_min_speech_secs: f64,
    /// Mint requires at least this estimated SNR (dB).
    pub mint_min_snr_db: f32,
    /// Mint requires at least this voiced fraction (cleaned speech / total segment).
    pub mint_min_voiced_frac: f64,
}

/// Classify a VAD result into mint / attach-only / reject. `total_secs` is the full
/// pre-VAD segment duration (denominator of the voiced fraction).
pub fn assess_quality(vr: &VadResult, total_secs: f64, g: &MintGates) -> QualitySignals {
    let snr_db = 20.0 * (vr.speech_rms / vr.noise_rms.max(1e-6)).max(1e-6).log10();
    let voiced_frac = if total_secs > 0.0 {
        vr.speech_secs / total_secs
    } else {
        0.0
    };
    let quality = if vr.speech_secs < g.min_speech_secs {
        SpeakerQuality::Reject
    } else if vr.speech_secs >= g.mint_min_speech_secs
        && snr_db >= g.mint_min_snr_db
        && voiced_frac >= g.mint_min_voiced_frac
    {
        SpeakerQuality::Mint
    } else {
        SpeakerQuality::AttachOnly
    };
    QualitySignals {
        speech_secs: vr.speech_secs,
        voiced_frac,
        snr_db,
        quality,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utt(start_ms: i64, end_ms: i64) -> Utterance {
        Utterance {
            text: "x".into(),
            start_ms,
            end_ms,
        }
    }

    #[test]
    fn voiced_bounds_spans_min_start_to_max_end() {
        let b = voiced_bounds_ms(&[utt(200, 800), utt(1000, 1600)]).unwrap();
        assert_eq!(b, (200, 1600));
        assert!((span_secs(b) - 1.4).abs() < 1e-6);
    }

    #[test]
    fn voiced_bounds_none_on_empty_or_degenerate() {
        assert!(voiced_bounds_ms(&[]).is_none());
        assert!(voiced_bounds_ms(&[utt(500, 500)]).is_none());
    }

    #[test]
    fn slice_ms_maps_ms_to_samples_and_clamps() {
        let pcm: Vec<f32> = (0..32_000).map(|i| i as f32).collect(); // 2s at 16kHz
        let s = slice_ms(&pcm, 0, 1000); // first 1s -> 16000 samples
        assert_eq!(s.len(), 16_000);
        // Out-of-range end clamps to buffer length, never panics.
        assert_eq!(slice_ms(&pcm, 1900, 5000).len(), 32_000 - 1900 * 16);
    }

    #[test]
    fn l2_normalize_unit_length() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        let mut z = vec![0.0, 0.0];
        l2_normalize(&mut z); // no panic, stays zero
        assert_eq!(z, vec![0.0, 0.0]);
    }

    #[test]
    fn cosine_distance_basic() {
        assert!(cosine_distance(&[1.0, 0.0], &[1.0, 0.0]).abs() < 1e-6); // identical -> 0
        assert!((cosine_distance(&[1.0, 0.0], &[0.0, 1.0]) - 1.0).abs() < 1e-6); // orthogonal -> 1
        assert!((cosine_distance(&[1.0, 0.0], &[-1.0, 0.0]) - 2.0).abs() < 1e-6); // opposite -> 2
        assert_eq!(cosine_distance(&[0.0, 0.0], &[1.0, 1.0]), 1.0); // zero -> max
    }

    fn gates() -> MintGates {
        MintGates {
            min_speech_secs: 0.8,
            mint_min_speech_secs: 1.2,
            mint_min_snr_db: 10.0,
            mint_min_voiced_frac: 0.5,
        }
    }

    fn vr(speech_secs: f64, speech_rms: f32, noise_rms: f32) -> VadResult {
        VadResult {
            speech: vec![],
            speech_secs,
            start_sample: 0,
            end_sample: (speech_secs * SAMPLE_RATE as f64) as usize,
            speech_rms,
            noise_rms,
        }
    }

    #[test]
    fn quality_reject_when_too_little_speech() {
        // 0.4s of cleaned speech (e.g. one word in a static-heavy 2s segment) -> Reject.
        let q = assess_quality(&vr(0.4, 0.2, 0.001), 2.0, &gates());
        assert_eq!(q.quality, SpeakerQuality::Reject);
    }

    #[test]
    fn quality_mint_when_clean_long_and_loud() {
        // 1.5s of clean speech (SNR ~46 dB, voiced_frac 0.75) -> may mint.
        let q = assess_quality(&vr(1.5, 0.2, 0.001), 2.0, &gates());
        assert_eq!(q.quality, SpeakerQuality::Mint);
        assert!(q.snr_db > 20.0);
    }

    #[test]
    fn quality_attach_only_when_noisy() {
        // 1.5s of speech but barely above the noise floor (SNR ~3.5 dB) -> attach, never mint.
        let q = assess_quality(&vr(1.5, 0.015, 0.01), 2.0, &gates());
        assert_eq!(q.quality, SpeakerQuality::AttachOnly);
    }

    #[test]
    fn quality_attach_only_when_speech_sparse() {
        // Loud + clean but only 0.9s of speech in a 4s segment (voiced_frac 0.225) -> attach.
        let q = assess_quality(&vr(0.9, 0.2, 0.001), 4.0, &gates());
        assert_eq!(q.quality, SpeakerQuality::AttachOnly);
    }

    #[test]
    fn rms_basic() {
        assert_eq!(rms(&[]), 0.0);
        assert!((rms(&[0.5, -0.5, 0.5, -0.5]) - 0.5).abs() < 1e-6);
    }
}
