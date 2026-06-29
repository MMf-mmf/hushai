//! The ramp + soak controller: grow the camera fleet one step at a time (cumulative load), hold a
//! soak window at each N, sample worker/backend/dashboard metrics + host load, decide whether the
//! worker keeps up with realtime, and stop once it can't (plus a couple of steps to chart the knee).

use crate::camera::{FeederStats, run_camera};
use crate::config::Cli;
use crate::corpus::Corpus;
use crate::report::{LivePoint, LiveStatus, Reporter, RunVerdict, SummaryRow, TimeseriesRow};
use crate::saturation::StepWindow;
use crate::scrape::{PromSnapshot, Scrape};
use crate::sysload;
use anyhow::{Context, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;

/// (lane, stage) pairs whose mean latency we scan to name the dominant (bottleneck) stage.
const STAGES: &[(&str, &str)] = &[
    ("audio", "extract_pcm"),
    ("audio", "transcribe"),
    ("audio", "sentiment"),
    ("audio", "speaker_window"),
    ("audio", "speaker_embed"),
    ("audio", "embed"),
    ("audio", "write_transcript"),
    ("vision", "sample_frames"),
    ("vision", "face_detect"),
    ("vision", "face_enhance_embed"),
    ("vision", "object_detect"),
    ("vision", "clip_embed"),
    ("vision", "plate"),
    ("vision", "write_tx"),
];

pub async fn run(cli: Cli) -> Result<()> {
    let video = cli.video.clone();
    let work_dir = cli
        .work_dir
        .clone()
        .unwrap_or_else(|| {
            let stem = video.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
            std::path::PathBuf::from("local_dev/.feed_work").join(stem)
        });
    let corpus = Arc::new(Corpus::build(&video, &work_dir, cli.seg_seconds).context("build corpus")?);

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(cli.insecure)
        .timeout(Duration::from_secs(30))
        .build()
        .context("build http client")?;

    let url = Arc::new(cli.url.clone());
    let token = Arc::new(cli.token());
    let stats = Arc::new(FeederStats::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let seg_dur = cli.seg_duration();

    let mut reporter = Reporter::new(&cli.out_dir, &cli.profile, cli.live_json.clone())?;
    let host = host_string();
    let cli_summary = format!(
        "max_cameras={} step={} seg_seconds={} soak_secs={} sample_secs={} profile={}",
        cli.max_cameras, cli.step, cli.seg_seconds, cli.soak_secs, cli.sample_secs, cli.profile
    );
    tracing::info!(%host, "{cli_summary}");

    let worker_pid = sysload::read_pid(&cli.worker_pid_file);
    if worker_pid.is_none() {
        tracing::warn!(pidfile = %cli.worker_pid_file.display(), "no worker PID file; CPU/RSS attribution disabled");
    }
    let use_pm = !cli.no_powermetrics;

    // Continuous delta state across the whole run (not reset per step).
    let run_start = Instant::now();
    let mut prev: Option<Delta> = None;

    let mut cam_handles: Vec<JoinHandle<()>> = Vec::new();
    let mut summary: Vec<SummaryRow> = Vec::new();
    let mut live_points: Vec<LivePoint> = Vec::new();
    let mut saturation_n: Option<usize> = None;
    let mut first_failing_n: Option<usize> = None;
    let mut aborted = false;
    let mut dominant = ("none".to_string(), 0.0f64);

    let mut active = 0usize;
    'ramp: while active < cli.max_cameras {
        // Add the next `step` cameras (cumulative).
        let target = (active + cli.step).min(cli.max_cameras);
        for idx in (active + 1)..=target {
            let stagger = seg_dur.mul_f64((idx - 1) as f64 / cli.max_cameras.max(1) as f64);
            cam_handles.push(tokio::spawn(run_camera(
                idx,
                corpus.clone(),
                client.clone(),
                url.clone(),
                token.clone(),
                stats.clone(),
                stagger,
                shutdown.clone(),
            )));
        }
        active = target;
        tracing::info!(cameras = active, "ramped up; soaking {}s", cli.soak_secs);

        // Soak: sample until the window elapses.
        let step_start = Instant::now();
        let mut window = StepWindow::default();
        let mut cpu_acc = Acc::default();
        let mut gpu_acc = Acc::default();
        let mut ane_acc = Acc::default();
        let mut qd_acc = Acc::default();
        let mut last_rss: Option<f64> = None;
        let mut last_scrape_worker: Option<PromSnapshot> = None;

        while step_start.elapsed() < Duration::from_secs(cli.soak_secs) {
            tokio::time::sleep(Duration::from_secs(cli.sample_secs.max(1))).await;
            let scrape = Scrape::collect(&client, &cli.worker_metrics, &cli.backend_metrics, &cli.dashboard).await;
            let sys = sysload::collect(worker_pid, use_pm);

            let now = Instant::now();
            let cur = Delta::read(&scrape);
            let dt = prev.as_ref().map(|p| (now - p.at).as_secs_f64()).unwrap_or(0.0);
            let audio_tput = rate(prev.as_ref().map(|p| p.proc_audio), cur.proc_audio, dt);
            let vision_tput = rate(prev.as_ref().map(|p| p.proc_vision), cur.proc_vision, dt);
            let ingest_rate = rate(prev.as_ref().map(|p| p.ingest_new), cur.ingest_new, dt);
            prev = Some(Delta { at: now, ..cur });

            let audio_lag = scrape.dashboard_oldest_pending("transcription");
            let vision_lag = scrape.dashboard_oldest_pending("vision");
            let audio_qd = wget(&scrape.worker, "hushai_worker_queue_depth", &[("lane", "audio")])
                .or_else(|| dash_depth(&scrape, "transcription"))
                .unwrap_or(0.0);
            let vision_qd = wget(&scrape.worker, "hushai_worker_queue_depth", &[("lane", "vision")])
                .or_else(|| dash_depth(&scrape, "vision"))
                .unwrap_or(0.0);
            let errors = wget(&scrape.worker, "hushai_segments_processed_total", &[("lane", "audio"), ("result", "error")]).unwrap_or(0.0);

            let asr_mean = whist(&scrape.worker, "hushai_worker_stage_seconds", &[("lane", "audio"), ("stage", "transcribe")]);
            let seg_mean = whist(&scrape.worker, "hushai_worker_segment_seconds", &[("lane", "audio")]);
            let lag_mean = whist(&scrape.worker, "hushai_worker_capture_lag_seconds", &[("lane", "audio")]);

            cpu_acc.push(sys.worker_cpu_pct);
            gpu_acc.push(sys.gpu_active_pct);
            ane_acc.push(sys.ane_power_mw);
            qd_acc.push(Some(audio_qd));
            if sys.worker_rss_mb.is_some() {
                last_rss = sys.worker_rss_mb;
            }
            if scrape.worker.is_some() {
                last_scrape_worker = scrape.worker.clone();
            }

            let elapsed = run_start.elapsed().as_secs_f64();
            let row = TimeseriesRow {
                ts_unix: unix_secs(),
                elapsed_secs: elapsed,
                active_cameras: active,
                phase: "soak".to_string(),
                offered_seg_per_s: cli.offered_seg_per_s(active),
                ingest_accepted_per_s: ingest_rate,
                audio_throughput_seg_per_s: audio_tput,
                vision_throughput_seg_per_s: vision_tput,
                audio_queue_depth: audio_qd,
                vision_queue_depth: vision_qd,
                audio_oldest_pending_age_s: audio_lag,
                vision_oldest_pending_age_s: vision_lag,
                audio_errors: errors,
                vision_errors: wget(&scrape.worker, "hushai_segments_processed_total", &[("lane", "vision"), ("result", "error")]).unwrap_or(0.0),
                stage_asr_mean_s: asr_mean,
                segment_audio_mean_s: seg_mean,
                capture_lag_audio_mean_s: lag_mean,
                feeder_overruns: stats.overruns.load(Ordering::Relaxed),
                feeder_post_ms_mean: feeder_post_mean(&stats),
                worker_cpu_pct: sys.worker_cpu_pct,
                gpu_active_pct: sys.gpu_active_pct,
                ane_power_mw: sys.ane_power_mw,
                worker_rss_mb: sys.worker_rss_mb,
                sample_source: sys.source.clone(),
            };
            reporter.write_timeseries(&row)?;
            window.push(elapsed, audio_lag, audio_tput, errors);

            // Live panel: show the in-progress step as a provisional point.
            let mut pts = live_points.clone();
            pts.push(LivePoint {
                n: active,
                audio_queue_depth: audio_qd,
                audio_oldest_pending_age_s: audio_lag.unwrap_or(0.0),
                audio_throughput_seg_per_s: audio_tput,
                worker_cpu_pct: sys.worker_cpu_pct,
                gpu_active_pct: sys.gpu_active_pct,
                keeping_up: true,
            });
            reporter.write_live(&LiveStatus {
                running: true,
                profile: cli.profile.clone(),
                current_cameras: active,
                max_cameras: cli.max_cameras,
                saturation_n,
                points: pts,
            });

            // Hard abort: backlog beyond the worker lease window (segments get re-leased/double-counted).
            if audio_lag.map(|l| l > cli.abort_lag_secs).unwrap_or(false) {
                tracing::warn!(lag = ?audio_lag, "hard-abort: capture lag exceeded {}s", cli.abort_lag_secs);
                aborted = true;
            }
            if stats.overruns.load(Ordering::Relaxed) > (active as u64) * 5 {
                tracing::warn!("hard-abort: sustained feeder overruns (backend cannot accept fast enough)");
                aborted = true;
            }
            if aborted {
                break;
            }
        }

        // Evaluate over the steady-state tail (last 60% of the soak).
        let verdict = window.tail(0.6).evaluate(cli.offered_seg_per_s(active), cli.seg_seconds);
        let (dom_stage, dom_mean) = dominant_stage(&last_scrape_worker);
        if dom_mean >= dominant.1 {
            dominant = (dom_stage.clone(), dom_mean);
        }
        let mean_cpu = cpu_acc.mean();
        let summary_row = SummaryRow {
            n: active,
            offered_seg_per_s: cli.offered_seg_per_s(active),
            sustained_audio_tput_seg_per_s: verdict.sustained_tput,
            mean_audio_queue_depth: qd_acc.mean().unwrap_or(0.0),
            audio_lag_slope_s_per_s: verdict.lag_slope,
            mean_audio_oldest_pending_age_s: verdict.mean_lag,
            keeping_up: verdict.keeping_up,
            error_delta: verdict.error_delta,
            mean_worker_cpu_pct: mean_cpu,
            mean_gpu_active_pct: gpu_acc.mean(),
            mean_ane_power_mw: ane_acc.mean(),
            worker_rss_mb: last_rss,
            dominant_stage: dom_stage,
            dominant_stage_mean_s: dom_mean,
            per_camera_cpu_pct: mean_cpu.map(|c| c / active as f64),
            reason: verdict.reason.clone(),
        };
        tracing::info!(
            n = active, keeping_up = verdict.keeping_up, slope = verdict.lag_slope,
            tput = verdict.sustained_tput, "step verdict: {}", verdict.reason
        );
        summary.push(summary_row);

        live_points.push(LivePoint {
            n: active,
            audio_queue_depth: qd_acc.mean().unwrap_or(0.0),
            audio_oldest_pending_age_s: verdict.mean_lag,
            audio_throughput_seg_per_s: verdict.sustained_tput,
            worker_cpu_pct: mean_cpu,
            gpu_active_pct: gpu_acc.mean(),
            keeping_up: verdict.keeping_up,
        });

        if verdict.keeping_up {
            saturation_n = Some(active);
        } else if first_failing_n.is_none() {
            first_failing_n = Some(active);
        }
        if aborted {
            break 'ramp;
        }
        // Chart a couple of steps past the first failure, then stop.
        if let Some(f) = first_failing_n {
            if active >= f + 2 * cli.step {
                break 'ramp;
            }
        }
    }

    // Stop the fleet and drain.
    shutdown.store(true, Ordering::SeqCst);
    for h in cam_handles {
        let _ = h.await;
    }

    let verdict = RunVerdict {
        saturation_n,
        first_failing_n,
        aborted,
        dominant_stage: dominant.0,
        dominant_stage_mean_s: dominant.1,
        profile: cli.profile.clone(),
    };
    reporter.write_live(&LiveStatus {
        running: false,
        profile: cli.profile.clone(),
        current_cameras: active,
        max_cameras: cli.max_cameras,
        saturation_n,
        points: live_points,
    });
    reporter.finish(&summary, &verdict, &cli_summary, &host)?;

    match saturation_n {
        Some(n) => tracing::info!("SATURATION: {n} cameras sustained in real time (bottleneck: {})", verdict.dominant_stage),
        None => tracing::warn!("the worker did not keep up at any tested camera count"),
    }
    Ok(())
}

#[derive(Default)]
struct Acc {
    sum: f64,
    n: usize,
}
impl Acc {
    fn push(&mut self, v: Option<f64>) {
        if let Some(v) = v {
            self.sum += v;
            self.n += 1;
        }
    }
    fn mean(&self) -> Option<f64> {
        if self.n > 0 { Some(self.sum / self.n as f64) } else { None }
    }
}

/// Counter snapshot for rate computation.
struct Delta {
    at: Instant,
    proc_audio: f64,
    proc_vision: f64,
    ingest_new: f64,
}
impl Delta {
    fn read(s: &Scrape) -> Self {
        Delta {
            at: Instant::now(),
            proc_audio: wget(&s.worker, "hushai_segments_processed_total", &[("lane", "audio"), ("result", "ok")]).unwrap_or(0.0),
            proc_vision: wget(&s.worker, "hushai_segments_processed_total", &[("lane", "vision"), ("result", "ok")]).unwrap_or(0.0),
            ingest_new: s.backend.as_ref().map(|b| b.sum_where("hushai_segments_ingested_total", "result", "new")).unwrap_or(0.0),
        }
    }
}

fn rate(prev: Option<f64>, cur: f64, dt: f64) -> f64 {
    match prev {
        Some(p) if dt > 0.0 && cur >= p => (cur - p) / dt,
        _ => 0.0,
    }
}

fn wget(s: &Option<PromSnapshot>, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    s.as_ref().and_then(|p| p.get(name, labels))
}

fn whist(s: &Option<PromSnapshot>, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    s.as_ref().and_then(|p| p.hist_mean(name, labels))
}

fn dash_depth(s: &Scrape, queue: &str) -> Option<f64> {
    let p = s.dashboard_queue(queue, "pending")?;
    let pr = s.dashboard_queue(queue, "processing").unwrap_or(0.0);
    Some(p + pr)
}

fn dominant_stage(worker: &Option<PromSnapshot>) -> (String, f64) {
    let mut best = ("none".to_string(), 0.0f64);
    for (lane, stage) in STAGES {
        if let Some(m) = whist(worker, "hushai_worker_stage_seconds", &[("lane", lane), ("stage", stage)]) {
            if m > best.1 {
                best = (format!("{lane}/{stage}"), m);
            }
        }
    }
    best
}

fn feeder_post_mean(stats: &FeederStats) -> f64 {
    let posted = stats.posted.load(Ordering::Relaxed);
    if posted == 0 {
        0.0
    } else {
        stats.post_ms_sum.load(Ordering::Relaxed) as f64 / posted as f64
    }
}

fn unix_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn host_string() -> String {
    let chip = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown CPU".to_string());
    let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    format!("{chip} ({ncpu} logical CPUs), {}", std::env::consts::OS)
}
