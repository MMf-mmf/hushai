//! Device content hints + the ingest-side AI skip gate.
//!
//! Sources may attach cheap raw measurements to each segment via the manifest's `attrs`
//! escape hatch (contract §8): `hint.v` (schema version), `hint.audio_rms` /
//! `hint.audio_peak_rms` (linear PCM RMS, s16 samples normalized by 32768 — the same scale
//! as the worker's `vad::rms`), and `hint.motion_score` (32×32 grayscale mean-subtracted
//! MSE vs the device's previous sampled frame — the mirror of the worker's
//! `vision::motion` metric). The device reports MEASUREMENTS, never decisions; every
//! threshold lives here in backend env so policy is tunable without an app release.
//!
//! At ingest, a segment whose hints are provably below the floors gets its lane status row
//! born TERMINAL (`status='skipped'`, `skip_reason='silent_hint'/'static_hint'`) instead of
//! `pending`, so dead content never wakes or occupies the worker. Rules:
//!
//! - **Fail open.** Absent / unversioned / malformed hints → `Enqueue` (exactly the legacy
//!   path). Unhinted sources (web capture, loadtest, old app builds) are untouched, and the
//!   gate branches only on the PRESENCE of hint keys — never on `source_kind` (contract §7).
//! - **Strictly stricter than the worker.** Audio gates on the PEAK windowed RMS when
//!   present (peak ≤ floor ⇒ whole-segment RMS ≤ floor ⇒ a strict subset of the worker's
//!   stage-1 skip set). Vision defaults to HALF the worker's threshold because the device
//!   fingerprint comes through a different pixel path (sensor YUV vs decoded RGB).
//! - **Audited.** A deterministic slice of would-be skips (`INGEST_HINT_AUDIT_PCT`, keyed on
//!   the segment UUID's random tail so it's reproducible) is enqueued `pending` with
//!   `hint_audit=true`; the worker's own gate then records an agree/disagree verdict — the
//!   live calibration + trust signal for these client-supplied hints.

use anyhow::{Context, anyhow};

/// Ingest hint-gate policy (env-derived, see `Config::from_env`).
#[derive(Debug, Clone)]
pub struct HintGateCfg {
    /// Master switch (`INGEST_HINT_GATE_ENABLED`, default true). Off = legacy behavior exactly.
    pub enabled: bool,
    /// Per-lane switches (`INGEST_HINT_AUDIO_ENABLED` / `INGEST_HINT_VISION_ENABLED`, default true).
    pub audio_enabled: bool,
    pub vision_enabled: bool,
    /// Skip audio when the reported (peak) RMS is at/under this (`INGEST_AUDIO_RMS_FLOOR`,
    /// default 0.005 — mirrors the worker's `AUDIO_SILENCE_RMS_FLOOR`).
    pub audio_rms_floor: f32,
    /// Skip vision when the reported motion score is at/under this (`INGEST_MOTION_THRESHOLD`,
    /// default 4.0 — deliberately half the worker's `VISION_MOTION_THRESHOLD=8.0`).
    pub motion_threshold: f32,
    /// Percent (0–100) of would-be skips enqueued anyway as audit samples
    /// (`INGEST_HINT_AUDIT_PCT`, default 2).
    pub audit_pct: u8,
}

impl Default for HintGateCfg {
    /// The env defaults (gate on, worker-mirroring floors, 2% audit) — what `from_env`
    /// yields in an unconfigured environment. Used by tests constructing `Config` literally.
    fn default() -> Self {
        Self {
            enabled: true,
            audio_enabled: true,
            vision_enabled: true,
            audio_rms_floor: 0.005,
            motion_threshold: 4.0,
            audit_pct: 2,
        }
    }
}

impl HintGateCfg {
    pub fn from_env() -> anyhow::Result<Self> {
        fn parse<T: std::str::FromStr>(key: &str, default: &str) -> anyhow::Result<T>
        where
            T::Err: std::fmt::Display,
        {
            let raw = std::env::var(key).unwrap_or_else(|_| default.to_string());
            raw.trim()
                .parse()
                .map_err(|e| anyhow!("env var {key}={raw:?} is invalid: {e}"))
        }
        let audit_pct: u8 = parse("INGEST_HINT_AUDIT_PCT", "2")?;
        anyhow::ensure!(audit_pct <= 100, "INGEST_HINT_AUDIT_PCT must be 0..=100");
        Ok(Self {
            enabled: parse("INGEST_HINT_GATE_ENABLED", "true").context("parsing INGEST_HINT_GATE_ENABLED")?,
            audio_enabled: parse("INGEST_HINT_AUDIO_ENABLED", "true")?,
            vision_enabled: parse("INGEST_HINT_VISION_ENABLED", "true")?,
            audio_rms_floor: parse("INGEST_AUDIO_RMS_FLOOR", "0.005")?,
            motion_threshold: parse("INGEST_MOTION_THRESHOLD", "4.0")?,
            audit_pct,
        })
    }
}

/// Parsed per-segment hints. All-`None` when the segment carries no (recognized) hints.
#[derive(Debug, Default, PartialEq)]
pub struct Hints {
    pub audio_rms: Option<f32>,
    pub audio_peak_rms: Option<f32>,
    pub motion_score: Option<f32>,
    /// A `hint.*` value was present but unusable (bad number, negative, non-finite). The
    /// affected field stays `None` (fail open); surfaced as a counter for visibility.
    pub malformed: bool,
}

/// Tolerantly extract hints from `segments.attrs` JSON. Only `hint.v == "1"` is understood;
/// any other/absent version yields no hints (a future v2 device degrades to full processing
/// on an old backend, never to misinterpretation). Values may be JSON strings or numbers.
pub fn parse(attrs: &serde_json::Value) -> Hints {
    let mut out = Hints::default();
    let Some(map) = attrs.as_object() else { return out };
    let version_ok = map
        .get("hint.v")
        .map(|v| v.as_str() == Some("1") || v.as_i64() == Some(1))
        .unwrap_or(false);
    if !version_ok {
        return out;
    }
    let mut field = |key: &str| -> Option<f32> {
        let v = map.get(key)?;
        let parsed = match v {
            serde_json::Value::String(s) => s.trim().parse::<f32>().ok(),
            serde_json::Value::Number(n) => n.as_f64().map(|f| f as f32),
            _ => None,
        };
        match parsed {
            Some(f) if f.is_finite() && f >= 0.0 => Some(f),
            _ => {
                out.malformed = true;
                None
            }
        }
    };
    out.audio_rms = field("hint.audio_rms");
    out.audio_peak_rms = field("hint.audio_peak_rms");
    out.motion_score = field("hint.motion_score");
    out
}

/// What ingest should do with one AI lane of one segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneDecision {
    /// Normal path: status row born `pending`, worker notified.
    Enqueue,
    /// Hints say skip, but this segment drew the audit lot: born `pending` with
    /// `hint_audit=true` so the worker's own gate can grade the hint.
    Audit,
    /// Hints say skip: status row born terminal `skipped` with this `skip_reason`.
    Skip(&'static str),
}

/// Audio-lane decision. Gates on the peak windowed RMS when the device reports it (strictly
/// more conservative than whole-segment RMS), else on the whole-segment RMS (the exact
/// worker stage-1 metric). `audit_roll` is 0–99, derived from the segment id.
pub fn audio_decision(h: &Hints, cfg: &HintGateCfg, audit_roll: u8) -> LaneDecision {
    if !cfg.enabled || !cfg.audio_enabled {
        return LaneDecision::Enqueue;
    }
    let Some(level) = h.audio_peak_rms.or(h.audio_rms) else {
        return LaneDecision::Enqueue;
    };
    if level > cfg.audio_rms_floor {
        return LaneDecision::Enqueue;
    }
    if audit_roll < cfg.audit_pct {
        return LaneDecision::Audit;
    }
    LaneDecision::Skip("silent_hint")
}

/// Vision-lane decision, on the device's motion score.
pub fn vision_decision(h: &Hints, cfg: &HintGateCfg, audit_roll: u8) -> LaneDecision {
    if !cfg.enabled || !cfg.vision_enabled {
        return LaneDecision::Enqueue;
    }
    let Some(score) = h.motion_score else {
        return LaneDecision::Enqueue;
    };
    if score > cfg.motion_threshold {
        return LaneDecision::Enqueue;
    }
    if audit_roll < cfg.audit_pct {
        return LaneDecision::Audit;
    }
    LaneDecision::Skip("static_hint")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> HintGateCfg {
        HintGateCfg {
            enabled: true,
            audio_enabled: true,
            vision_enabled: true,
            audio_rms_floor: 0.005,
            motion_threshold: 4.0,
            audit_pct: 0,
        }
    }

    #[test]
    fn absent_or_unversioned_hints_parse_to_none() {
        assert_eq!(parse(&json!({})), Hints::default());
        assert_eq!(parse(&json!({"client": "hushai-android"})), Hints::default());
        // Unknown version: everything ignored, even well-formed values.
        let h = parse(&json!({"hint.v": "2", "hint.audio_rms": "0.001"}));
        assert_eq!(h, Hints::default());
        // Non-object attrs (defensive).
        assert_eq!(parse(&json!(null)), Hints::default());
    }

    #[test]
    fn versioned_hints_parse_strings_and_numbers() {
        let h = parse(&json!({
            "hint.v": "1",
            "hint.audio_rms": "0.0023",
            "hint.audio_peak_rms": 0.0041,
            "hint.motion_score": "1.5"
        }));
        assert_eq!(h.audio_rms, Some(0.0023));
        assert_eq!(h.audio_peak_rms, Some(0.0041));
        assert_eq!(h.motion_score, Some(1.5));
        assert!(!h.malformed);
    }

    #[test]
    fn malformed_values_fail_open_and_flag() {
        for bad in ["NaN", "-1", "inf", "abc", ""] {
            let h = parse(&json!({"hint.v": "1", "hint.audio_rms": bad}));
            assert_eq!(h.audio_rms, None, "value {bad:?} must not parse");
            assert!(h.malformed, "value {bad:?} must set the malformed flag");
        }
    }

    #[test]
    fn audio_gate_prefers_peak_and_respects_floor() {
        let c = cfg();
        // Peak above floor blocks the skip even when the mean is below it (conservative).
        let h = Hints { audio_rms: Some(0.001), audio_peak_rms: Some(0.02), ..Default::default() };
        assert_eq!(audio_decision(&h, &c, 99), LaneDecision::Enqueue);
        // Peak at/below floor skips.
        let h = Hints { audio_rms: Some(0.001), audio_peak_rms: Some(0.004), ..Default::default() };
        assert_eq!(audio_decision(&h, &c, 99), LaneDecision::Skip("silent_hint"));
        // No peak: fall back to whole-segment RMS (the worker's exact stage-1 metric).
        let h = Hints { audio_rms: Some(0.004), ..Default::default() };
        assert_eq!(audio_decision(&h, &c, 99), LaneDecision::Skip("silent_hint"));
        // No hints at all: enqueue.
        assert_eq!(audio_decision(&Hints::default(), &c, 99), LaneDecision::Enqueue);
    }

    #[test]
    fn vision_gate_thresholds() {
        let c = cfg();
        let quiet = Hints { motion_score: Some(0.4), ..Default::default() };
        let moving = Hints { motion_score: Some(9.7), ..Default::default() };
        assert_eq!(vision_decision(&quiet, &c, 99), LaneDecision::Skip("static_hint"));
        assert_eq!(vision_decision(&moving, &c, 99), LaneDecision::Enqueue);
        assert_eq!(vision_decision(&Hints::default(), &c, 99), LaneDecision::Enqueue);
    }

    #[test]
    fn audit_roll_promotes_skip_to_audit_deterministically() {
        let c = HintGateCfg { audit_pct: 10, ..cfg() };
        let silent = Hints { audio_peak_rms: Some(0.0001), ..Default::default() };
        assert_eq!(audio_decision(&silent, &c, 9), LaneDecision::Audit);
        assert_eq!(audio_decision(&silent, &c, 10), LaneDecision::Skip("silent_hint"));
        // Audit never fires for segments the gate wouldn't have skipped.
        let loud = Hints { audio_peak_rms: Some(0.5), ..Default::default() };
        assert_eq!(audio_decision(&loud, &c, 0), LaneDecision::Enqueue);
    }

    #[test]
    fn kill_switches_restore_legacy_behavior() {
        let silent = Hints { audio_peak_rms: Some(0.0), motion_score: Some(0.0), ..Default::default() };
        let off = HintGateCfg { enabled: false, ..cfg() };
        assert_eq!(audio_decision(&silent, &off, 99), LaneDecision::Enqueue);
        assert_eq!(vision_decision(&silent, &off, 99), LaneDecision::Enqueue);
        let audio_off = HintGateCfg { audio_enabled: false, ..cfg() };
        assert_eq!(audio_decision(&silent, &audio_off, 99), LaneDecision::Enqueue);
        assert_eq!(vision_decision(&silent, &audio_off, 99), LaneDecision::Skip("static_hint"));
    }
}
