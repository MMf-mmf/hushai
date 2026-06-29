//! Minimal, dependency-free Prometheus metrics + health exposition shared by every Hushai service
//! (roadmap B1/B5 — cloud-native observability). No new crates: a tiny global registry (atomic-ish
//! `Mutex<HashMap>`), a Prometheus text renderer, an axum `/metrics` handler for the HTTP services
//! (backend/rag/viewer), and a raw-tokio `/metrics`+`/healthz` server for the worker (which has no
//! axum router). The registry is process-global, so each binary exposes its OWN metrics — exactly
//! what a Prometheus scraper expects (one target per service).
//!
//! Usage: `observe::describe(name, "counter"|"gauge", help)` once at startup, then `observe::counter`/
//! `counter_by`/`gauge` anywhere. Label values are escaped; series are keyed by (name, sorted labels).

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Prometheus text exposition content type (v0.0.4).
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

type Labels = Vec<(&'static str, String)>;
type Key = (&'static str, Labels);

/// Upper bounds (seconds) for every Hushai latency histogram. One global ladder spans sub-millisecond
/// DB reads through multi-second ONNX/Whisper stages, so a single bucket set covers all timed stages.
/// The implicit `+Inf` bucket equals the series `_count`.
const LATENCY_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

/// One histogram series: per-bucket observation counts (non-cumulative; `render` accumulates),
/// plus running `sum`/`count`. `buckets[i]` counts observations whose value falls in
/// `(LATENCY_BUCKETS[i-1], LATENCY_BUCKETS[i]]`; observations above the last bound land only in `count`.
#[derive(Clone)]
struct HistogramSeries {
    buckets: Vec<u64>,
    sum: f64,
    count: u64,
}

#[derive(Default)]
struct Registry {
    /// name -> (type, help)
    meta: HashMap<&'static str, (&'static str, &'static str)>,
    counters: HashMap<Key, u64>,
    gauges: HashMap<Key, i64>,
    histograms: HashMap<Key, HistogramSeries>,
}

static REG: OnceLock<Mutex<Registry>> = OnceLock::new();
fn reg() -> &'static Mutex<Registry> {
    REG.get_or_init(|| Mutex::new(Registry::default()))
}

fn key(name: &'static str, labels: &[(&'static str, &str)]) -> Key {
    let mut l: Labels = labels.iter().map(|(k, v)| (*k, (*v).to_string())).collect();
    l.sort_by(|a, b| a.0.cmp(b.0)); // stable series key regardless of arg order
    (name, l)
}

/// Register a metric's TYPE + HELP. Idempotent; call at startup so `/metrics` is self-describing.
pub fn describe(name: &'static str, typ: &'static str, help: &'static str) {
    reg().lock().unwrap().meta.insert(name, (typ, help));
}

/// Increment a counter by 1.
pub fn counter(name: &'static str, labels: &[(&'static str, &str)]) {
    counter_by(name, labels, 1);
}

/// Increment a counter by `by`.
pub fn counter_by(name: &'static str, labels: &[(&'static str, &str)], by: u64) {
    *reg().lock().unwrap().counters.entry(key(name, labels)).or_insert(0) += by;
}

/// Set a gauge to an absolute value.
pub fn gauge(name: &'static str, labels: &[(&'static str, &str)], v: i64) {
    reg().lock().unwrap().gauges.insert(key(name, labels), v);
}

/// Record one duration (seconds) into a histogram series, lazily creating it. Thread-safe (global
/// `Mutex`), so it is callable from blocking threads (e.g. the vision `spawn_blocking` closure).
/// Register the name once with `describe(name, "histogram", help)` so `/metrics` is self-describing.
pub fn observe_duration(name: &'static str, labels: &[(&'static str, &str)], secs: f64) {
    let mut g = reg().lock().unwrap();
    let entry = g.histograms.entry(key(name, labels)).or_insert_with(|| HistogramSeries {
        buckets: vec![0; LATENCY_BUCKETS.len()],
        sum: 0.0,
        count: 0,
    });
    // Bump the first bucket whose upper bound covers this observation (non-cumulative store);
    // `render` turns these into the cumulative `le` counts Prometheus expects.
    for (i, ub) in LATENCY_BUCKETS.iter().enumerate() {
        if secs <= *ub {
            entry.buckets[i] += 1;
            break;
        }
    }
    entry.sum += secs;
    entry.count += 1;
}

/// RAII stage timer: `let _t = observe::StageTimer::start(name, &labels);` records the elapsed time
/// into the histogram on drop. Overhead is one `Instant::now()` + a subtraction (nanoseconds) — it
/// will not perturb the stage it measures. Labels are owned so the guard can be held across `.await`.
pub struct StageTimer {
    name: &'static str,
    labels: Vec<(&'static str, String)>,
    start: std::time::Instant,
}

impl StageTimer {
    pub fn start(name: &'static str, labels: &[(&'static str, &str)]) -> Self {
        Self {
            name,
            labels: labels.iter().map(|(k, v)| (*k, (*v).to_string())).collect(),
            start: std::time::Instant::now(),
        }
    }
}

impl Drop for StageTimer {
    fn drop(&mut self) {
        let secs = self.start.elapsed().as_secs_f64();
        let borrowed: Vec<(&'static str, &str)> =
            self.labels.iter().map(|(k, v)| (*k, v.as_str())).collect();
        observe_duration(self.name, &borrowed, secs);
    }
}

/// Render the whole registry in Prometheus text exposition format.
pub fn render() -> String {
    let g = reg().lock().unwrap();
    // Every metric name that has metadata OR an emitted series, sorted for stable output.
    let mut names: BTreeSet<&'static str> = g.meta.keys().copied().collect();
    names.extend(g.counters.keys().map(|(n, _)| *n));
    names.extend(g.gauges.keys().map(|(n, _)| *n));
    names.extend(g.histograms.keys().map(|(n, _)| *n));

    let mut out = String::new();
    for name in names {
        if let Some((typ, help)) = g.meta.get(name) {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} {typ}");
        }
        let mut series: Vec<(String, String)> = Vec::new();
        for ((n, labels), v) in &g.counters {
            if *n == name {
                series.push((fmt_labels(labels), v.to_string()));
            }
        }
        for ((n, labels), v) in &g.gauges {
            if *n == name {
                series.push((fmt_labels(labels), v.to_string()));
            }
        }
        series.sort(); // deterministic ordering across scrapes
        for (lbl, val) in series {
            let _ = writeln!(out, "{name}{lbl} {val}");
        }

        // Histograms render as cumulative `_bucket{le=...}` lines (monotonic, required by
        // `histogram_quantile`), then `_sum` and `_count`. Collect + sort by label string so the
        // output is deterministic across scrapes despite HashMap ordering.
        let mut hseries: Vec<(&Labels, &HistogramSeries)> = Vec::new();
        for ((n, labels), h) in &g.histograms {
            if *n == name {
                hseries.push((labels, h));
            }
        }
        hseries.sort_by(|a, b| fmt_labels(a.0).cmp(&fmt_labels(b.0)));
        for (labels, h) in hseries {
            let mut cum = 0u64;
            for (i, ub) in LATENCY_BUCKETS.iter().enumerate() {
                cum += h.buckets[i];
                let _ = writeln!(out, "{name}_bucket{} {}", fmt_labels_le(labels, &fmt_f64(*ub)), cum);
            }
            let _ = writeln!(out, "{name}_bucket{} {}", fmt_labels_le(labels, "+Inf"), h.count);
            let _ = writeln!(out, "{name}_sum{} {}", fmt_labels(labels), fmt_f64(h.sum));
            let _ = writeln!(out, "{name}_count{} {}", fmt_labels(labels), h.count);
        }
    }
    out
}

/// Format an f64 for Prometheus exposition (plain decimal, never scientific in this range).
fn fmt_f64(v: f64) -> String {
    format!("{v}")
}

fn fmt_labels(labels: &Labels) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let inner: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
        .collect();
    format!("{{{}}}", inner.join(","))
}

/// Like `fmt_labels` but always appends an `le="..."` entry — used for histogram bucket lines, which
/// always carry a bound, so the result is never empty.
fn fmt_labels_le(labels: &Labels, le: &str) -> String {
    let mut inner: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
        .collect();
    inner.push(format!("le=\"{}\"", escape(le)));
    format!("{{{}}}", inner.join(","))
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Record a `hushai_build_info{service,version} 1` gauge (so a scraper can see which build is up).
pub fn record_build_info(service: &'static str) {
    describe("hushai_build_info", "gauge", "Build/version info (always 1).");
    gauge("hushai_build_info", &[("service", service), ("version", env!("CARGO_PKG_VERSION"))], 1);
}

// ---------------------------------------------------------------------------
// axum /metrics handler — for the HTTP services (backend / rag / viewer).
// ---------------------------------------------------------------------------

/// `GET /metrics` handler. Mount unauthenticated alongside `/healthz` (scrapers aren't logged in).
pub async fn metrics_handler() -> ([(axum::http::HeaderName, &'static str); 1], String) {
    ([(axum::http::header::CONTENT_TYPE, CONTENT_TYPE)], render())
}

// ---------------------------------------------------------------------------
// Raw-tokio /metrics + /healthz server — for the worker (no axum router).
// ---------------------------------------------------------------------------

/// Spawn a tiny HTTP server serving `/metrics` (the registry) and `/healthz` ("ok") for a service
/// without an axum router (the worker). Best-effort: a bind failure logs + disables metrics, never
/// blocking the worker. One request per connection (no keep-alive) — plenty for a scraper.
pub fn spawn_metrics_server(addr: SocketAddr, shutdown: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, %addr, "metrics server: bind failed; metrics disabled");
                return;
            }
        };
        tracing::info!(%addr, "metrics server listening (/metrics, /healthz)");
        while !shutdown.load(Ordering::SeqCst) {
            tokio::select! {
                r = listener.accept() => match r {
                    Ok((mut sock, _)) => { tokio::spawn(async move { handle_conn(&mut sock).await; }); }
                    Err(e) => tracing::debug!(error = %e, "metrics accept failed"),
                },
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
            }
        }
    });
}

async fn handle_conn(sock: &mut tokio::net::TcpStream) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Read until we have the full request line (ends in CRLF) — TCP may split it across segments —
    // bounded so a slow/oversized client can't make us read forever.
    let mut acc: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    for _ in 0..8 {
        match sock.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                acc.extend_from_slice(&chunk[..n]);
                if acc.windows(2).any(|w| w == b"\r\n") || acc.len() >= 8192 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let req = String::from_utf8_lossy(&acc);
    // Request line: "GET /metrics HTTP/1.1" — take the path token.
    let path = req.split_whitespace().nth(1).unwrap_or("/");
    let (status, ctype, body) = if path.starts_with("/metrics") {
        ("200 OK", CONTENT_TYPE, render())
    } else if path.starts_with("/healthz") {
        ("200 OK", "text/plain", "ok".to_string())
    } else {
        ("404 Not Found", "text/plain", "not found".to_string())
    };
    let resp = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = sock.write_all(resp.as_bytes()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_renders_cumulative_buckets_sum_and_count() {
        // Use a unique metric name so the process-global registry can't collide with other tests.
        let name = "test_hist_stage_seconds";
        describe(name, "histogram", "test histogram");
        // Observations: two in (0.025, 0.05], one in (0.5, 1.0], one above the top bound (60).
        observe_duration(name, &[("stage", "x")], 0.03);
        observe_duration(name, &[("stage", "x")], 0.04);
        observe_duration(name, &[("stage", "x")], 0.7);
        observe_duration(name, &[("stage", "x")], 120.0);

        let out = render();
        // Self-describing TYPE line.
        assert!(out.contains(&format!("# TYPE {name} histogram")), "missing TYPE:\n{out}");
        // Cumulative buckets: le=0.05 includes the two ~0.03/0.04 obs.
        assert!(out.contains(&format!("{name}_bucket{{stage=\"x\",le=\"0.05\"}} 2")), "le=0.05:\n{out}");
        // le=1 adds the 0.7 obs -> 3 cumulative.
        assert!(out.contains(&format!("{name}_bucket{{stage=\"x\",le=\"1\"}} 3")), "le=1:\n{out}");
        // The 120s obs is above the top finite bound, so le=60 stays at 3 but +Inf == count == 4.
        assert!(out.contains(&format!("{name}_bucket{{stage=\"x\",le=\"60\"}} 3")), "le=60:\n{out}");
        assert!(out.contains(&format!("{name}_bucket{{stage=\"x\",le=\"+Inf\"}} 4")), "+Inf:\n{out}");
        assert!(out.contains(&format!("{name}_count{{stage=\"x\"}} 4")), "count:\n{out}");
    }
}
