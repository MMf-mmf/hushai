//! macOS system-load sampler. `ps` gives the worker process's CPU%/RSS (always available, no sudo).
//! `powermetrics` adds machine-wide GPU + ANE residency/power, but needs root — we invoke it with
//! `sudo -n` (non-interactive) so a missing NOPASSWD rule fails fast instead of hanging on a prompt,
//! and we degrade to `ps`-only with GPU/ANE = None.

use std::process::Command;

#[derive(Default, Clone, Debug, serde::Serialize)]
pub struct SysSample {
    /// "powermetrics+ps" when GPU/ANE were captured, else "ps".
    pub source: String,
    /// Worker process CPU% from `ps` (can exceed 100 on multi-core).
    pub worker_cpu_pct: Option<f64>,
    pub worker_rss_mb: Option<f64>,
    /// Machine-wide GPU active residency % (powermetrics).
    pub gpu_active_pct: Option<f64>,
    /// ANE power (mW) — >0 means the Apple Neural Engine (CoreML vision path) is in use.
    pub ane_power_mw: Option<f64>,
    /// Combined CPU+GPU+ANE package power (W).
    pub pkg_power_w: Option<f64>,
}

/// Sample host load. `pid` is the worker process; `use_pm` enables the powermetrics path.
pub fn collect(pid: Option<u32>, use_pm: bool) -> SysSample {
    let mut s = SysSample { source: "ps".to_string(), ..Default::default() };
    if let Some(pid) = pid {
        if let Some((cpu, rss_mb)) = ps_cpu_rss(pid) {
            s.worker_cpu_pct = Some(cpu);
            s.worker_rss_mb = Some(rss_mb);
        }
    }
    if use_pm {
        if let Some(pm) = powermetrics_once() {
            s.gpu_active_pct = pm.gpu_active_pct;
            s.ane_power_mw = pm.ane_power_mw;
            s.pkg_power_w = pm.pkg_power_w;
            if pm.gpu_active_pct.is_some() || pm.ane_power_mw.is_some() {
                s.source = "powermetrics+ps".to_string();
            }
        }
    }
    s
}

/// Read the worker PID from a pidfile (best-effort).
pub fn read_pid(path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn ps_cpu_rss(pid: u32) -> Option<(f64, f64)> {
    let out = Command::new("ps")
        .args(["-o", "%cpu=,rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let mut it = line.split_whitespace();
    let cpu = it.next()?.parse::<f64>().ok()?;
    let rss_kb = it.next()?.parse::<f64>().ok()?;
    Some((cpu, rss_kb / 1024.0))
}

#[derive(Default)]
struct PmSample {
    gpu_active_pct: Option<f64>,
    ane_power_mw: Option<f64>,
    pkg_power_w: Option<f64>,
}

/// One short powermetrics sample (text format). Best-effort: tolerant of version-specific layout —
/// each field is parsed independently and left None if its line is absent.
fn powermetrics_once() -> Option<PmSample> {
    let out = Command::new("sudo")
        .args([
            "-n", // non-interactive: fail immediately if no passwordless sudo (don't hang)
            "powermetrics",
            "--samplers", "cpu_power,gpu_power",
            "-i", "300",
            "-n", "1",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        tracing::debug!("powermetrics unavailable (needs passwordless sudo); GPU/ANE = N/A");
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut s = PmSample::default();
    for line in text.lines() {
        let l = line.trim();
        let low = l.to_lowercase();
        if low.contains("gpu") && low.contains("active residency") {
            s.gpu_active_pct = first_percent(l);
        } else if low.starts_with("ane power") {
            s.ane_power_mw = first_number(l);
        } else if low.contains("combined power") {
            // mW -> W
            s.pkg_power_w = first_number(l).map(|mw| mw / 1000.0);
        }
    }
    Some(s)
}

/// First "NN.NN%" in a line -> NN.NN.
fn first_percent(l: &str) -> Option<f64> {
    let idx = l.find('%')?;
    let prefix = &l[..idx];
    let start = prefix
        .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|i| i + 1)
        .unwrap_or(0);
    prefix[start..].parse::<f64>().ok()
}

/// First standalone number in a line (e.g. "ANE Power: 123 mW" -> 123).
fn first_number(l: &str) -> Option<f64> {
    l.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find(|t| !t.is_empty())
        .and_then(|t| t.parse::<f64>().ok())
}
