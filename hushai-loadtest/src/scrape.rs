//! Scrape the worker + backend Prometheus `/metrics` and the viewer `/api/dashboard`. The Prometheus
//! parser is a tiny line reader (the exposition format `observe::render()` emits is simple + stable).

use anyhow::Result;

#[derive(Clone, Debug)]
pub struct PromSample {
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

#[derive(Default, Clone, Debug)]
pub struct PromSnapshot {
    pub samples: Vec<PromSample>,
}

impl PromSnapshot {
    pub fn parse(text: &str) -> Self {
        let mut samples = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // "<name>{labels} <value>" or "<name> <value>" — value is the last whitespace token.
            let Some(sp) = line.rfind(char::is_whitespace) else { continue };
            let (metric, val) = line.split_at(sp);
            let Ok(value) = val.trim().parse::<f64>() else { continue };
            let metric = metric.trim();
            let (name, labels) = match metric.find('{') {
                Some(b) => {
                    let name = metric[..b].to_string();
                    let lbls = metric[b + 1..].trim_end_matches('}');
                    (name, parse_labels(lbls))
                }
                None => (metric.to_string(), Vec::new()),
            };
            samples.push(PromSample { name, labels, value });
        }
        Self { samples }
    }

    /// Exact match: a sample named `name` whose labels include all of `want` (equal values).
    pub fn get(&self, name: &str, want: &[(&str, &str)]) -> Option<f64> {
        self.samples
            .iter()
            .find(|s| s.name == name && want.iter().all(|(k, v)| has_label(&s.labels, k, v)))
            .map(|s| s.value)
    }

    /// Sum every series named `name` that carries label `k=v`.
    pub fn sum_where(&self, name: &str, k: &str, v: &str) -> f64 {
        self.samples
            .iter()
            .filter(|s| s.name == name && has_label(&s.labels, k, v))
            .map(|s| s.value)
            .sum()
    }

    /// Mean of a histogram series (`_sum / _count`) selected by labels. None if no observations yet.
    pub fn hist_mean(&self, name: &str, want: &[(&str, &str)]) -> Option<f64> {
        let sum = self.get(&format!("{name}_sum"), want)?;
        let count = self.get(&format!("{name}_count"), want)?;
        if count > 0.0 { Some(sum / count) } else { None }
    }
}

fn has_label(labels: &[(String, String)], k: &str, v: &str) -> bool {
    labels.iter().any(|(lk, lv)| lk == k && lv == v)
}

fn parse_labels(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // Labels are `k="v"` comma-separated; values never contain commas/quotes in our exposition.
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(eq) = part.find('=') {
            let k = part[..eq].trim().to_string();
            let v = part[eq + 1..].trim().trim_matches('"').to_string();
            out.push((k, v));
        }
    }
    out
}

/// One scrape across all three sources. Any source may be absent (best-effort).
pub struct Scrape {
    pub worker: Option<PromSnapshot>,
    pub backend: Option<PromSnapshot>,
    pub dashboard: Option<serde_json::Value>,
}

impl Scrape {
    pub async fn collect(
        client: &reqwest::Client,
        worker_url: &str,
        backend_url: &str,
        dashboard_url: &str,
    ) -> Self {
        let (w, b, d) = tokio::join!(
            fetch_text(client, worker_url),
            fetch_text(client, backend_url),
            fetch_json(client, dashboard_url),
        );
        Scrape {
            worker: w.ok().map(|t| PromSnapshot::parse(&t)),
            backend: b.ok().map(|t| PromSnapshot::parse(&t)),
            dashboard: d.ok(),
        }
    }

    /// `oldest_pending_age_secs` for a lane from the dashboard (the realtime-lag signal). Lane is the
    /// dashboard queue key: "transcription" (audio) or "vision".
    pub fn dashboard_oldest_pending(&self, queue: &str) -> Option<f64> {
        self.dashboard
            .as_ref()?
            .get("queues")?
            .get(queue)?
            .get("oldest_pending_age_secs")?
            .as_f64()
    }

    pub fn dashboard_queue(&self, queue: &str, field: &str) -> Option<f64> {
        self.dashboard.as_ref()?.get("queues")?.get(queue)?.get(field)?.as_f64()
    }
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    Ok(client.get(url).send().await?.error_for_status()?.text().await?)
}

async fn fetch_json(client: &reqwest::Client, url: &str) -> Result<serde_json::Value> {
    Ok(client.get(url).send().await?.error_for_status()?.json().await?)
}
