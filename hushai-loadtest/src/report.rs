//! Outputs: a live-appended `timeseries.csv`, a per-N `summary_by_N.csv`, a machine-readable
//! `run.json`, a human `report.md`, and a rolling `live.json` for the viewer dashboard panel.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, serde::Serialize)]
pub struct TimeseriesRow {
    pub ts_unix: i64,
    pub elapsed_secs: f64,
    pub active_cameras: usize,
    pub phase: String,
    pub offered_seg_per_s: f64,
    pub ingest_accepted_per_s: f64,
    pub audio_throughput_seg_per_s: f64,
    pub vision_throughput_seg_per_s: f64,
    pub audio_queue_depth: f64,
    pub vision_queue_depth: f64,
    pub audio_oldest_pending_age_s: Option<f64>,
    pub vision_oldest_pending_age_s: Option<f64>,
    pub audio_errors: f64,
    pub vision_errors: f64,
    pub stage_asr_mean_s: Option<f64>,
    pub segment_audio_mean_s: Option<f64>,
    pub capture_lag_audio_mean_s: Option<f64>,
    pub feeder_overruns: u64,
    pub feeder_post_ms_mean: f64,
    pub worker_cpu_pct: Option<f64>,
    pub gpu_active_pct: Option<f64>,
    pub ane_power_mw: Option<f64>,
    pub worker_rss_mb: Option<f64>,
    pub sample_source: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SummaryRow {
    pub n: usize,
    pub offered_seg_per_s: f64,
    pub sustained_audio_tput_seg_per_s: f64,
    pub mean_audio_queue_depth: f64,
    pub audio_lag_slope_s_per_s: f64,
    pub mean_audio_oldest_pending_age_s: f64,
    pub keeping_up: bool,
    pub error_delta: f64,
    pub mean_worker_cpu_pct: Option<f64>,
    pub mean_gpu_active_pct: Option<f64>,
    pub mean_ane_power_mw: Option<f64>,
    pub worker_rss_mb: Option<f64>,
    pub dominant_stage: String,
    pub dominant_stage_mean_s: f64,
    pub per_camera_cpu_pct: Option<f64>,
    pub reason: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RunVerdict {
    pub saturation_n: Option<usize>,
    pub first_failing_n: Option<usize>,
    pub aborted: bool,
    pub dominant_stage: String,
    pub dominant_stage_mean_s: f64,
    pub profile: String,
}

/// Compact status the viewer dashboard polls (written each step to live.json).
#[derive(Clone, Debug, serde::Serialize)]
pub struct LiveStatus {
    pub running: bool,
    pub profile: String,
    pub current_cameras: usize,
    pub max_cameras: usize,
    pub saturation_n: Option<usize>,
    pub points: Vec<LivePoint>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct LivePoint {
    pub n: usize,
    pub audio_queue_depth: f64,
    pub audio_oldest_pending_age_s: f64,
    pub audio_throughput_seg_per_s: f64,
    pub worker_cpu_pct: Option<f64>,
    pub gpu_active_pct: Option<f64>,
    pub keeping_up: bool,
}

pub struct Reporter {
    pub run_dir: PathBuf,
    ts_writer: csv::Writer<File>,
    extra_live_path: Option<PathBuf>,
}

impl Reporter {
    pub fn new(out_dir: &Path, profile: &str, extra_live_path: Option<PathBuf>) -> Result<Self> {
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let run_dir = out_dir.join(format!("run-{ts}-{profile}"));
        std::fs::create_dir_all(&run_dir)
            .with_context(|| format!("create run dir {}", run_dir.display()))?;
        let ts_writer = csv::Writer::from_path(run_dir.join("timeseries.csv"))
            .context("open timeseries.csv")?;
        Ok(Self { run_dir, ts_writer, extra_live_path })
    }

    pub fn write_timeseries(&mut self, row: &TimeseriesRow) -> Result<()> {
        self.ts_writer.serialize(row).context("write timeseries row")?;
        self.ts_writer.flush().context("flush timeseries")?;
        Ok(())
    }

    pub fn write_live(&self, live: &LiveStatus) {
        let json = match serde_json::to_string_pretty(live) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "serialize live.json failed");
                return;
            }
        };
        write_atomic(&self.run_dir.join("live.json"), &json);
        if let Some(p) = &self.extra_live_path {
            write_atomic(p, &json);
        }
    }

    pub fn finish(
        &self,
        summary: &[SummaryRow],
        verdict: &RunVerdict,
        cli_summary: &str,
        host: &str,
    ) -> Result<()> {
        // summary_by_N.csv
        let mut w = csv::Writer::from_path(self.run_dir.join("summary_by_N.csv"))
            .context("open summary_by_N.csv")?;
        for r in summary {
            w.serialize(r)?;
        }
        w.flush()?;

        // run.json
        let run_json = serde_json::json!({
            "verdict": verdict,
            "summary": summary,
            "config": cli_summary,
            "host": host,
        });
        std::fs::write(
            self.run_dir.join("run.json"),
            serde_json::to_string_pretty(&run_json)?,
        )
        .context("write run.json")?;

        // report.md
        let md = render_report_md(summary, verdict, cli_summary, host);
        std::fs::write(self.run_dir.join("report.md"), md).context("write report.md")?;

        tracing::info!(dir = %self.run_dir.display(), "wrote timeseries.csv, summary_by_N.csv, run.json, report.md");
        Ok(())
    }
}

fn write_atomic(path: &Path, contents: &str) {
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, contents).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn render_report_md(summary: &[SummaryRow], v: &RunVerdict, cli: &str, host: &str) -> String {
    let mut s = String::new();
    s.push_str("# Hushai capacity / load-test report\n\n");
    s.push_str(&format!("**Profile:** `{}`  \n", v.profile));
    s.push_str(&format!("**Host:** {host}  \n\n"));

    match v.saturation_n {
        Some(n) => s.push_str(&format!(
            "## Saturation: **{n} cameras** sustained in real time{}\n\n",
            v.first_failing_n
                .map(|f| format!(" (first failing N = {f})"))
                .unwrap_or_default()
        )),
        None => s.push_str(
            "## Saturation: the worker did **not** keep up even at 1 camera, or no step kept up.\n\n",
        ),
    }
    if v.aborted {
        s.push_str("> ⚠️ The ramp was **aborted early** on a hard threshold (lag/errors/overruns). See the table.\n\n");
    }
    s.push_str(&format!(
        "**Bottleneck stage at saturation:** `{}` (mean {:.3}s).\n\n",
        v.dominant_stage, v.dominant_stage_mean_s
    ));

    s.push_str("## Per-N summary\n\n");
    s.push_str("| N | offered/s | sustained/s | lag slope | mean lag | keep up | CPU% | GPU% | RSS MB | /cam CPU% | note |\n");
    s.push_str("|--:|--:|--:|--:|--:|:--:|--:|--:|--:|--:|:--|\n");
    for r in summary {
        s.push_str(&format!(
            "| {} | {:.2} | {:.2} | {:.3} | {:.1} | {} | {} | {} | {} | {} | {} |\n",
            r.n,
            r.offered_seg_per_s,
            r.sustained_audio_tput_seg_per_s,
            r.audio_lag_slope_s_per_s,
            r.mean_audio_oldest_pending_age_s,
            if r.keeping_up { "✅" } else { "❌" },
            opt(r.mean_worker_cpu_pct, 0),
            opt(r.mean_gpu_active_pct, 0),
            opt(r.worker_rss_mb, 0),
            opt(r.per_camera_cpu_pct, 1),
            if r.keeping_up { "" } else { r.reason.as_str() },
        ));
    }
    s.push_str("\n## Hardware-sizing extrapolation\n\n");
    if let (Some(n), Some(row)) = (v.saturation_n, summary.iter().find(|r| Some(r.n) == v.saturation_n)) {
        if let Some(per_cam) = row.per_camera_cpu_pct {
            s.push_str(&format!(
                "- Per-camera CPU at the {n}-camera ceiling ≈ **{per_cam:.1}% of one core**.\n\
                 - Projected to 30 cameras ≈ **{:.1} cores** of CPU headroom needed (linear regime only).\n",
                per_cam * 30.0 / 100.0
            ));
        }
        s.push_str(
            "- If the *vision* lane saturated first while CPU had headroom, the deployment needs GPU/ANE-class\n  \
              acceleration; if *audio* (CPU Whisper) saturated first, it needs more cores or a GPU-accelerated\n  \
              Whisper build and/or a higher `WORKER_CONCURRENCY`.\n",
        );
    } else {
        s.push_str("- No keeping-up step to extrapolate from; reduce per-segment cost (disable stages) or raise `WORKER_CONCURRENCY`.\n");
    }
    s.push_str("\n## Notes\n\n");
    s.push_str("- Identity-only replay reuses one blob, so **disk-write** volume is NOT exercised; size storage analytically (`30 × byte_rate × retention`).\n");
    s.push_str("- Per-stage means come from `hushai_worker_stage_seconds`; if the worker predates that metric they read `N/A`.\n");
    s.push_str(&format!("\n---\n_config_: `{cli}`\n"));
    s
}

fn opt(v: Option<f64>, prec: usize) -> String {
    match v {
        Some(x) => format!("{:.*}", prec, x),
        None => "—".to_string(),
    }
}
