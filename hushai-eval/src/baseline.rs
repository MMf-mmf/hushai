//! Baselines (keyed by config-hash) + improvement/regression/unchanged classification.
//!
//! A baseline is the accepted metric vector for one case under one config-hash. Comparing a run
//! to it only makes sense when the hash matches — a model/knob change mints a new lineage, so
//! "no baseline for this hash" means "config/environment changed; establish a baseline".

use crate::ctx::Ctx;
use crate::score::{Direction, Metric};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub case_id: String,
    pub config_hash: String,
    pub git_sha: String,
    pub metrics: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    Improvement,
    Regression,
    Unchanged,
    New, // no baseline value for this metric/hash
}

pub fn path(ctx: &Ctx, config_hash: &str, case_id: &str) -> PathBuf {
    ctx.baselines_root.join(config_hash).join(format!("{case_id}.json"))
}

pub fn load(ctx: &Ctx, config_hash: &str, case_id: &str) -> Option<Baseline> {
    let p = path(ctx, config_hash, case_id);
    let text = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save(ctx: &Ctx, config_hash: &str, git_sha: &str, case_id: &str, metrics: &[Metric]) -> Result<PathBuf> {
    let p = path(ctx, config_hash, case_id);
    std::fs::create_dir_all(p.parent().unwrap()).context("creating baseline dir")?;
    let b = Baseline {
        case_id: case_id.to_string(),
        config_hash: config_hash.to_string(),
        git_sha: git_sha.to_string(),
        metrics: metrics.iter().map(|m| (m.key.clone(), m.value)).collect(),
    };
    std::fs::write(&p, serde_json::to_string_pretty(&b)?).context("writing baseline")?;
    Ok(p)
}

/// Classify a metric against its baseline value. Returns (classification, signed delta).
pub fn classify(metric: &Metric, baseline: Option<f64>) -> (Classification, Option<f64>) {
    let Some(b) = baseline else {
        return (Classification::New, None);
    };
    let delta = metric.value - b;
    if metric.direction == Direction::Info {
        return (Classification::Unchanged, Some(delta));
    }
    if delta.abs() <= unchanged_band(&metric.key, metric.direction) {
        return (Classification::Unchanged, Some(delta));
    }
    let signed_gain = match metric.direction {
        Direction::HigherBetter | Direction::Boolean => delta,
        Direction::LowerBetter => -delta,
        Direction::Info => 0.0,
    };
    let cls = if signed_gain > 0.0 { Classification::Improvement } else { Classification::Regression };
    (cls, Some(delta))
}

/// Per-metric-class "unchanged" band. Counts are strict; sentiment widest (LLM sampling); ASR
/// tight (greedy decode is near-deterministic, the band only absorbs embedding/ORT float drift).
fn unchanged_band(key: &str, dir: Direction) -> f64 {
    if dir == Direction::Boolean {
        return 0.0;
    }
    if key.contains("count_error") {
        return 0.0;
    }
    if key.ends_with(".wer") || key.ends_with(".similarity") {
        return 0.03;
    }
    if key.starts_with("sentiment.") {
        return 0.20;
    }
    0.10
}
