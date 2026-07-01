//! Device load governor: actively pace processing so the box is NEVER driven into overload.
//!
//! The durable DB queue already guarantees nothing is dropped — work just accumulates as `pending`
//! and is processed strict-oldest-first whenever capacity frees up. What it does NOT do today is
//! protect the device itself: the fixed-concurrency loops run flat-out whenever there's a backlog,
//! pegging the CPU indefinitely (thermal throttling / unresponsiveness on an always-on edge box).
//!
//! This module adds the missing piece. A lightweight monitor task samples how loaded the device is
//! — the backlog-lag TREND (is the oldest pending item getting older?) and, optionally, the OS load
//! average — and publishes a 3-level state (Normal / Elevated / Saturated) the worker loops read on
//! each iteration. Under load the EXPENSIVE lane (vision) pauses first and both lanes take an
//! inter-segment cooldown, so audio keeps up and the box gets headroom. Nothing is reordered (strict
//! chronological stays) and nothing is dropped — deferral only delays. When load clears, full
//! throughput resumes and the backlog drains oldest-first (organic quiet-time drain).
//!
//! Hysteresis (separate up/down thresholds + a recover streak) keeps the level from flapping.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use sqlx::PgPool;

/// How loaded the device is. Ordered: a higher level means less headroom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LoadLevel {
    Normal = 0,
    Elevated = 1,
    Saturated = 2,
}

impl LoadLevel {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => LoadLevel::Normal,
            1 => LoadLevel::Elevated,
            _ => LoadLevel::Saturated,
        }
    }
    /// One level less pressured (saturates at Normal). Used for the gentle step-down on recovery.
    fn down(self) -> Self {
        match self {
            LoadLevel::Saturated => LoadLevel::Elevated,
            _ => LoadLevel::Normal,
        }
    }
}

/// Governor tunables (`LOAD_*` env). Defaults are conservative; `enabled=false` restores today's
/// behavior exactly (claim always, never pause, never cooldown).
#[derive(Debug, Clone)]
pub struct GovernorConfig {
    pub enabled: bool,
    /// How often the monitor samples + re-decides the level.
    pub sample: Duration,
    /// Backlog-lag slope (s of lag gained per s of wall-clock) entering Elevated / Saturated.
    pub slope_elevated: f64,
    pub slope_saturated: f64,
    /// Also use the OS 1-min load average (normalized by core count) as a faster-reacting signal.
    pub use_cpu: bool,
    pub cpu_elevated: f64,
    pub cpu_saturated: f64,
    /// Consecutive calmer samples required before stepping the level DOWN (anti-flap hysteresis).
    pub recover_samples: u32,
    /// Pause vision (the expensive lane) before audio when Saturated. Default true.
    pub pause_vision_first: bool,
    /// Inter-segment cooldown applied to the non-paused lane(s) at Elevated / Saturated.
    pub cooldown_elevated: Duration,
    pub cooldown_saturated: Duration,
}

/// Shared, cheaply-readable load state. One per process, wrapped in `Arc` and handed to every loop.
pub struct Governor {
    level: AtomicU8,
    cfg: GovernorConfig,
}

impl Governor {
    pub fn new(cfg: GovernorConfig) -> Self {
        Self {
            level: AtomicU8::new(LoadLevel::Normal as u8),
            cfg,
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Current load level (cheap atomic read).
    pub fn level(&self) -> LoadLevel {
        LoadLevel::from_u8(self.level.load(Ordering::Relaxed))
    }

    fn set(&self, l: LoadLevel) {
        self.level.store(l as u8, Ordering::Relaxed);
    }

    /// Should the vision lane pause (skip its claim and idle) right now? Vision is the expensive
    /// lane, so it's the first to pause at Saturated when `pause_vision_first` is set.
    pub fn vision_should_pause(&self) -> bool {
        self.cfg.enabled && self.cfg.pause_vision_first && self.level() == LoadLevel::Saturated
    }

    /// Should the audio lane pause? Only when the operator chose to throttle audio first
    /// (`pause_vision_first=false`) and we're Saturated. Off by default.
    pub fn audio_should_pause(&self) -> bool {
        self.cfg.enabled && !self.cfg.pause_vision_first && self.level() == LoadLevel::Saturated
    }

    /// Inter-segment cooldown for a still-running lane at the current level (ZERO when disabled or
    /// Normal). Gives the box headroom without stopping a lane.
    pub fn cooldown(&self) -> Duration {
        if !self.cfg.enabled {
            return Duration::ZERO;
        }
        match self.level() {
            LoadLevel::Normal => Duration::ZERO,
            LoadLevel::Elevated => self.cfg.cooldown_elevated,
            LoadLevel::Saturated => self.cfg.cooldown_saturated,
        }
    }
}

/// Ordinary least-squares slope of `ys` vs `xs` (0.0 when < 2 points or x has no spread). Mirrors
/// the loadtest's `saturation::linreg_slope` — a runtime crate shouldn't depend on the dev-only
/// loadtest crate, so the ~10 lines are duplicated here.
fn linreg_slope(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len();
    if n < 2 || n != ys.len() {
        return 0.0;
    }
    let nf = n as f64;
    let mean_x = xs.iter().sum::<f64>() / nf;
    let mean_y = ys.iter().sum::<f64>() / nf;
    let mut cov = 0.0;
    let mut var = 0.0;
    for (x, y) in xs.iter().zip(ys.iter()) {
        cov += (x - mean_x) * (y - mean_y);
        var += (x - mean_x) * (x - mean_x);
    }
    if var <= 0.0 {
        return 0.0;
    }
    cov / var
}

/// OS 1-minute load average (unix only; `None` elsewhere or on failure).
#[cfg(unix)]
fn load_avg_1m() -> Option<f64> {
    let mut la = [0f64; 3];
    // SAFETY: getloadavg writes up to `nelem` f64s into the provided buffer and returns the count
    // written (or -1). We pass a 3-element buffer and read only la[0] when it returns >= 1.
    let n = unsafe { libc::getloadavg(la.as_mut_ptr(), 3) };
    if n >= 1 {
        Some(la[0])
    } else {
        None
    }
}

#[cfg(not(unix))]
fn load_avg_1m() -> Option<f64> {
    None
}

/// Decide the target level from the current signals (pure, so it unit-tests).
fn target_level(lag_slope: f64, cpu_ratio: Option<f64>, cfg: &GovernorConfig) -> LoadLevel {
    let cpu = cpu_ratio.unwrap_or(0.0);
    if lag_slope > cfg.slope_saturated || (cfg.use_cpu && cpu > cfg.cpu_saturated) {
        LoadLevel::Saturated
    } else if lag_slope > cfg.slope_elevated || (cfg.use_cpu && cpu > cfg.cpu_elevated) {
        LoadLevel::Elevated
    } else {
        LoadLevel::Normal
    }
}

/// Apply hysteresis: step UP immediately, step DOWN only after `recover_samples` consecutive
/// calmer ticks (and only one level at a time). Returns the new level and the updated calm streak.
fn apply_hysteresis(
    current: LoadLevel,
    target: LoadLevel,
    calm_streak: u32,
    recover_samples: u32,
) -> (LoadLevel, u32) {
    if target > current {
        (target, 0) // pressure rising — react now
    } else if target < current {
        let calm = calm_streak + 1;
        if calm >= recover_samples.max(1) {
            (current.down(), 0) // sustained calm — ease off one level
        } else {
            (current, calm)
        }
    } else {
        (current, 0)
    }
}

/// Oldest pending AUDIO segment's age in seconds (how long the head of the queue has waited), or
/// 0.0 when the queue is empty / the count fails. The realtime keep-up signal whose TREND we watch.
async fn audio_backlog_lag_secs(pool: &PgPool) -> f64 {
    let lag: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (now() - min(updated_at)))::float8 \
         FROM segment_transcription_status WHERE status = 'pending'",
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten();
    lag.unwrap_or(0.0)
}

/// Spawn the monitor task: every `cfg.sample`, read the load signals, decide the level (with
/// hysteresis), publish it to `governor` + the `hushai_worker_load_level` gauge. Best-effort —
/// a failed DB read just skips that tick. Exits when `shutdown` is set.
pub fn spawn_load_monitor(pool: PgPool, governor: Arc<Governor>, shutdown: Arc<AtomicBool>) {
    if !governor.cfg.enabled {
        return; // disabled — leave the level at Normal forever (accessors short-circuit anyway)
    }
    tokio::spawn(async move {
        let cores = crate::config::WorkerConfig::cores().max(1) as f64;
        let start = Instant::now();
        // Ring buffer of (t_secs, lag_secs) for the slope; ~1 minute of history at the default 5s.
        let cap = 12usize;
        let mut ts: Vec<f64> = Vec::with_capacity(cap);
        let mut lags: Vec<f64> = Vec::with_capacity(cap);
        let mut current = LoadLevel::Normal;
        let mut calm: u32 = 0;

        while !shutdown.load(Ordering::SeqCst) {
            let lag = audio_backlog_lag_secs(&pool).await;
            let t = start.elapsed().as_secs_f64();
            if ts.len() == cap {
                ts.remove(0);
                lags.remove(0);
            }
            ts.push(t);
            lags.push(lag);

            let slope = linreg_slope(&ts, &lags);
            let cpu_ratio = if governor.cfg.use_cpu {
                load_avg_1m().map(|l| l / cores)
            } else {
                None
            };

            let target = target_level(slope, cpu_ratio, &governor.cfg);
            let (next, next_calm) =
                apply_hysteresis(current, target, calm, governor.cfg.recover_samples);
            if next != current {
                tracing::info!(
                    from = ?current,
                    to = ?next,
                    lag_slope = slope,
                    cpu_ratio = ?cpu_ratio,
                    "load governor: level change"
                );
            }
            current = next;
            calm = next_calm;
            governor.set(current);
            hushai_backend::observe::gauge("hushai_worker_load_level", &[], current as i64);

            tokio::select! {
                _ = tokio::time::sleep(governor.cfg.sample) => {}
                _ = wait_shutdown(&shutdown) => {}
            }
        }
    });
}

/// Poll `shutdown` so the sample sleep wakes promptly on shutdown instead of waiting a full period.
async fn wait_shutdown(shutdown: &Arc<AtomicBool>) {
    while !shutdown.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GovernorConfig {
        GovernorConfig {
            enabled: true,
            sample: Duration::from_secs(5),
            slope_elevated: 0.05,
            slope_saturated: 0.10,
            use_cpu: true,
            cpu_elevated: 0.80,
            cpu_saturated: 0.95,
            recover_samples: 3,
            pause_vision_first: true,
            cooldown_elevated: Duration::from_millis(0),
            cooldown_saturated: Duration::from_millis(250),
        }
    }

    #[test]
    fn linreg_slope_flat_and_rising() {
        assert!(linreg_slope(&[0.0, 1.0, 2.0], &[5.0, 5.0, 5.0]).abs() < 1e-9);
        assert!((linreg_slope(&[0.0, 1.0, 2.0], &[0.0, 2.0, 4.0]) - 2.0).abs() < 1e-9);
        assert_eq!(linreg_slope(&[1.0], &[1.0]), 0.0); // too few points
    }

    #[test]
    fn target_level_from_signals() {
        let c = cfg();
        // Flat lag, idle CPU => Normal.
        assert_eq!(target_level(0.0, Some(0.1), &c), LoadLevel::Normal);
        // Lag growing past the elevated slope => Elevated.
        assert_eq!(target_level(0.07, Some(0.1), &c), LoadLevel::Elevated);
        // Lag growing past the saturated slope => Saturated.
        assert_eq!(target_level(0.20, Some(0.1), &c), LoadLevel::Saturated);
        // CPU pegged => Saturated even with flat lag.
        assert_eq!(target_level(0.0, Some(0.99), &c), LoadLevel::Saturated);
    }

    #[test]
    fn cpu_signal_ignored_when_disabled() {
        let mut c = cfg();
        c.use_cpu = false;
        assert_eq!(target_level(0.0, Some(0.99), &c), LoadLevel::Normal);
    }

    #[test]
    fn hysteresis_steps_up_fast_down_slow() {
        let recover = 3;
        // Step UP immediately.
        let (l, calm) =
            apply_hysteresis(LoadLevel::Normal, LoadLevel::Saturated, 0, recover);
        assert_eq!(l, LoadLevel::Saturated);
        assert_eq!(calm, 0);
        // Calm ticks accumulate but don't step down until the streak is reached.
        let (l, calm) = apply_hysteresis(LoadLevel::Saturated, LoadLevel::Normal, 0, recover);
        assert_eq!(l, LoadLevel::Saturated);
        assert_eq!(calm, 1);
        let (l, calm) = apply_hysteresis(l, LoadLevel::Normal, calm, recover);
        assert_eq!(l, LoadLevel::Saturated);
        assert_eq!(calm, 2);
        // Third calm tick steps down ONE level (Saturated -> Elevated), resetting the streak.
        let (l, calm) = apply_hysteresis(l, LoadLevel::Normal, calm, recover);
        assert_eq!(l, LoadLevel::Elevated);
        assert_eq!(calm, 0);
    }

    #[test]
    fn accessors_respect_enabled_and_pause_lane() {
        let g = Governor::new(cfg());
        g.set(LoadLevel::Saturated);
        assert!(g.vision_should_pause());
        assert!(!g.audio_should_pause());
        assert_eq!(g.cooldown(), Duration::from_millis(250));

        let mut disabled = cfg();
        disabled.enabled = false;
        let g2 = Governor::new(disabled);
        g2.set(LoadLevel::Saturated);
        assert!(!g2.vision_should_pause());
        assert_eq!(g2.cooldown(), Duration::ZERO);
    }
}
