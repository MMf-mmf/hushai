//! CLI configuration for the camera fan-out load-test harness.

use clap::Parser;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "hushai-loadtest",
    about = "Replay one clip as N synthetic cameras (identity-only), ramp 1..N, and report per-camera load + the saturation point."
)]
pub struct Cli {
    /// Source video replayed (byte-identical) as every synthetic camera's feed.
    #[arg(long, default_value = "IMG_7256.mp4")]
    pub video: PathBuf,

    /// Backend segment-ingest endpoint.
    #[arg(long, default_value = "http://localhost:8080/v1/segments")]
    pub url: String,

    /// Bearer token (falls back to $DEVICE_TOKEN, then the dev secret).
    #[arg(long)]
    pub token: Option<String>,

    /// Worker Prometheus /metrics (queue depth, throughput, per-stage histograms).
    #[arg(long, default_value = "http://127.0.0.1:9100/metrics")]
    pub worker_metrics: String,

    /// Backend Prometheus /metrics (accepted ingest rate/bytes).
    #[arg(long, default_value = "http://localhost:8080/metrics")]
    pub backend_metrics: String,

    /// Viewer dashboard JSON (authoritative queue depth + oldest_pending_age).
    #[arg(long, default_value = "http://localhost:8070/api/dashboard")]
    pub dashboard: String,

    /// Maximum number of cameras to ramp up to.
    #[arg(long, default_value_t = 30)]
    pub max_cameras: usize,

    /// Cameras added per ramp step.
    #[arg(long, default_value_t = 1)]
    pub step: usize,

    /// Nominal segment duration (s) — matches the feeder split + the realtime emit cadence.
    #[arg(long, default_value_t = 2)]
    pub seg_seconds: u64,

    /// Soak hold per ramp step (s) over which steady-state metrics are measured.
    #[arg(long, default_value_t = 150)]
    pub soak_secs: u64,

    /// Metric sampling cadence (s).
    #[arg(long, default_value_t = 5)]
    pub sample_secs: u64,

    /// Hard-abort the ramp if capture->done lag exceeds this (s). Default = worker lease timeout.
    #[arg(long, default_value_t = 300.0)]
    pub abort_lag_secs: f64,

    /// Output directory; a `run-<ts>-<profile>` subdir is created inside.
    #[arg(long, default_value = "loadtest-out")]
    pub out_dir: PathBuf,

    /// Label for this run (e.g. the worker config profile); used in the run-dir name + report.
    #[arg(long, default_value = "everything")]
    pub profile: String,

    /// ffmpeg split cache dir (default: local_dev/.feed_work/<video-stem>).
    #[arg(long)]
    pub work_dir: Option<PathBuf>,

    /// Worker PID file for per-process CPU/RSS attribution.
    #[arg(long, default_value = "local_dev/logs/worker.pid")]
    pub worker_pid_file: PathBuf,

    /// Skip powermetrics (no sudo). CPU/RSS via `ps` only; GPU/ANE reported as N/A.
    #[arg(long)]
    pub no_powermetrics: bool,

    /// Also write a rolling live.json here for the viewer dashboard's load-test panel.
    #[arg(long)]
    pub live_json: Option<PathBuf>,

    /// Remove synthetic devices (source_kind=loadtest_replica) via the backend API, then exit.
    #[arg(long)]
    pub cleanup: bool,

    /// Skip TLS certificate verification (self-signed dev only; prefer a real cert).
    #[arg(long)]
    pub insecure: bool,
}

impl Cli {
    pub fn token(&self) -> String {
        self.token
            .clone()
            .or_else(|| std::env::var("DEVICE_TOKEN").ok())
            .unwrap_or_else(|| "dev-secret-token".to_string())
    }

    pub fn seg_duration(&self) -> Duration {
        Duration::from_secs(self.seg_seconds)
    }

    /// Offered load (segments/s) at a given camera count — one ~`seg_seconds` segment per camera.
    pub fn offered_seg_per_s(&self, cameras: usize) -> f64 {
        cameras as f64 / self.seg_seconds.max(1) as f64
    }

    /// Backend base URL (strip the `/v1/segments` suffix) for the device-list/delete API.
    pub fn backend_base(&self) -> String {
        self.url
            .strip_suffix("/v1/segments")
            .unwrap_or(&self.url)
            .trim_end_matches('/')
            .to_string()
    }
}
