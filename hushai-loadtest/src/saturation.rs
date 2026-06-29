//! The saturation rule. A camera count "keeps up with realtime" iff, over the soak's steady-state
//! tail: (1) the capture-lag (oldest_pending_age) regression slope is ~flat, (2) the worker drains
//! ~all of the offered load, (3) standing lag stays bounded, and (4) errors don't climb. The
//! saturation point is the largest N that keeps up; the system is saturated at the first N that
//! doesn't (backlog grows without bound).

/// Backlog age may grow at most this fast (s per elapsed s) and still count as "flat".
const EPS_SLOPE: f64 = 0.05;
/// Worker must drain at least this fraction of the offered segments/s.
const TPUT_FRACTION: f64 = 0.95;
/// Standing lag cap as a multiple of the segment duration.
const LAG_CAP_SEGMENTS: f64 = 3.0;

#[derive(Default)]
pub struct StepWindow {
    pub t: Vec<f64>,
    pub audio_lag: Vec<f64>,
    pub audio_tput: Vec<f64>,
    pub errors: Vec<f64>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct StepVerdict {
    pub keeping_up: bool,
    pub lag_slope: f64,
    pub mean_lag: f64,
    pub sustained_tput: f64,
    pub error_delta: f64,
    pub reason: String,
}

impl StepWindow {
    pub fn push(&mut self, t: f64, audio_lag: Option<f64>, audio_tput: f64, errors: f64) {
        if let Some(lag) = audio_lag {
            self.t.push(t);
            self.audio_lag.push(lag);
            self.audio_tput.push(audio_tput);
            self.errors.push(errors);
        }
    }

    /// The trailing `frac` (0..1) of samples — the steady-state tail, excluding the transient while
    /// a just-added camera's backlog settles.
    pub fn tail(&self, frac: f64) -> StepWindow {
        let n = self.t.len();
        let start = (((n as f64) * (1.0 - frac)).floor() as usize).min(n.saturating_sub(1));
        StepWindow {
            t: self.t[start..].to_vec(),
            audio_lag: self.audio_lag[start..].to_vec(),
            audio_tput: self.audio_tput[start..].to_vec(),
            errors: self.errors[start..].to_vec(),
        }
    }

    pub fn evaluate(&self, offered_seg_per_s: f64, seg_seconds: u64) -> StepVerdict {
        let lag_slope = linreg_slope(&self.t, &self.audio_lag);
        let mean_lag = mean(&self.audio_lag);
        let sustained_tput = mean(&self.audio_tput);
        let error_delta = match (self.errors.first(), self.errors.last()) {
            (Some(a), Some(b)) => (b - a).max(0.0),
            _ => 0.0,
        };
        let lag_cap = LAG_CAP_SEGMENTS * seg_seconds as f64;

        let mut reasons = Vec::new();
        let flat = lag_slope <= EPS_SLOPE;
        if !flat {
            reasons.push(format!("backlog growing ({lag_slope:.3}s/s)"));
        }
        let drains = sustained_tput >= offered_seg_per_s * TPUT_FRACTION;
        if !drains {
            reasons.push(format!(
                "throughput {sustained_tput:.2}/s < {:.2}/s offered",
                offered_seg_per_s
            ));
        }
        let bounded = mean_lag <= lag_cap;
        if !bounded {
            reasons.push(format!("lag {mean_lag:.1}s > cap {lag_cap:.1}s"));
        }
        let no_errors = error_delta < 1.0;
        if !no_errors {
            reasons.push(format!("{error_delta:.0} errors"));
        }

        let keeping_up = flat && drains && bounded && no_errors;
        StepVerdict {
            keeping_up,
            lag_slope,
            mean_lag,
            sustained_tput,
            error_delta,
            reason: if keeping_up { "keeping up".to_string() } else { reasons.join("; ") },
        }
    }
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Ordinary least-squares slope of y over x. 0 when fewer than 2 points or x is constant.
fn linreg_slope(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len().min(y.len());
    if n < 2 {
        return 0.0;
    }
    let xm = mean(&x[..n]);
    let ym = mean(&y[..n]);
    let mut num = 0.0;
    let mut den = 0.0;
    for i in 0..n {
        let dx = x[i] - xm;
        num += dx * (y[i] - ym);
        den += dx * dx;
    }
    if den == 0.0 { 0.0 } else { num / den }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_lag_keeps_up() {
        let mut w = StepWindow::default();
        for i in 0..10 {
            w.push(i as f64, Some(2.0), 5.0, 0.0); // flat 2s lag, 5/s drained, 5/s offered
        }
        let v = w.evaluate(5.0, 2);
        assert!(v.keeping_up, "{}", v.reason);
        assert!(v.lag_slope.abs() < 0.01);
    }

    #[test]
    fn growing_lag_saturates() {
        let mut w = StepWindow::default();
        for i in 0..10 {
            w.push(i as f64, Some(2.0 + i as f64 * 1.0), 2.0, 0.0); // lag climbs 1s/s, tput < offered
        }
        let v = w.evaluate(8.0, 2);
        assert!(!v.keeping_up);
    }
}
