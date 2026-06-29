//! Verdict assembly + machine/human reports + exit code.

use crate::baseline::Classification;
use crate::manifest::EnvManifest;
use crate::score::{Direction, Metric};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    PassImproved,
    Fail,
    Inconclusive,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricOutcome {
    pub key: String,
    pub value: f64,
    pub direction: Direction,
    pub baseline: Option<f64>,
    pub delta: Option<f64>,
    pub classification: Classification,
    pub floor_ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    pub case_id: String,
    pub split: String,
    pub tier: String,
    pub verdict: Verdict,
    pub note: String,
    pub injected: usize,
    pub processed: i64,
    pub metrics: Vec<MetricOutcome>,
}

impl CaseResult {
    pub fn inconclusive(case_id: &str, split: &str, tier: &str, note: impl Into<String>) -> Self {
        Self {
            case_id: case_id.into(),
            split: split.into(),
            tier: tier.into(),
            verdict: Verdict::Inconclusive,
            note: note.into(),
            injected: 0,
            processed: 0,
            metrics: vec![],
        }
    }

    pub fn from_metrics(
        case_id: &str,
        split: &str,
        tier: &str,
        injected: usize,
        processed: i64,
        metrics: Vec<Metric>,
        classify: impl Fn(&Metric) -> (Classification, Option<f64>, Option<f64>),
    ) -> Self {
        let mut outcomes = Vec::new();
        let mut any_regression = false;
        let mut any_improvement = false;
        let mut any_floor_breach = false;
        for m in &metrics {
            let (cls, baseline, delta) = classify(m);
            if !m.floor_ok {
                any_floor_breach = true;
            }
            match cls {
                Classification::Regression => any_regression = true,
                Classification::Improvement => any_improvement = true,
                _ => {}
            }
            outcomes.push(MetricOutcome {
                key: m.key.clone(),
                value: m.value,
                direction: m.direction,
                baseline,
                delta,
                classification: cls,
                floor_ok: m.floor_ok,
                detail: m.detail.clone(),
            });
        }
        let verdict = if any_floor_breach || any_regression {
            Verdict::Fail
        } else if any_improvement {
            Verdict::PassImproved
        } else {
            Verdict::Pass
        };
        Self {
            case_id: case_id.into(),
            split: split.into(),
            tier: tier.into(),
            verdict,
            note: String::new(),
            injected,
            processed,
            metrics: outcomes,
        }
    }

    pub fn passed(&self) -> bool {
        matches!(self.verdict, Verdict::Pass | Verdict::PassImproved)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SuiteResult {
    pub tier: String,
    pub config_hash: String,
    pub verdict: Verdict,
    pub exit_code: i32,
    pub manifest: EnvManifest,
    pub cases: Vec<CaseResult>,
}

impl SuiteResult {
    pub fn finalize(tier: &str, manifest: EnvManifest, cases: Vec<CaseResult>) -> Self {
        let any_inconclusive = cases.iter().any(|c| c.verdict == Verdict::Inconclusive);
        let any_fail = cases.iter().any(|c| c.verdict == Verdict::Fail);
        let any_improved = cases.iter().any(|c| c.verdict == Verdict::PassImproved);
        let (verdict, exit_code) = if any_inconclusive {
            (Verdict::Inconclusive, 2)
        } else if any_fail {
            (Verdict::Fail, 1)
        } else if any_improved {
            (Verdict::PassImproved, 0)
        } else {
            (Verdict::Pass, 0)
        };
        Self {
            tier: tier.into(),
            config_hash: manifest.config_hash.clone(),
            verdict,
            exit_code,
            manifest,
            cases,
        }
    }

    pub fn human_report(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "\nhushai-eval — tier={} config_hash={} git={}{}\n",
            self.tier,
            self.config_hash,
            self.manifest.git_sha,
            if self.manifest.git_dirty { "-dirty" } else { "" }
        ));
        s.push_str(&format!("migration_head: {}  ep: {}\n", self.manifest.migration_head, self.manifest.execution_provider));
        for c in &self.cases {
            s.push_str(&format!(
                "\n[{}] {} ({}/{} segments processed) — {:?}",
                verdict_glyph(c.verdict),
                c.case_id,
                c.processed,
                c.injected,
                c.verdict
            ));
            if !c.note.is_empty() {
                s.push_str(&format!("  — {}", c.note));
            }
            s.push('\n');
            for m in &c.metrics {
                let delta = match m.delta {
                    Some(d) => format!("{d:+.3}"),
                    None => "  new".into(),
                };
                let base = match m.baseline {
                    Some(b) => format!("{b:.3}"),
                    None => "—".into(),
                };
                s.push_str(&format!(
                    "    {} {:<26} {:>8.3}  base={:>6}  Δ{:>8}  {:?}  {}\n",
                    if m.floor_ok { "ok " } else { "BAD" },
                    m.key,
                    m.value,
                    base,
                    delta,
                    m.classification,
                    m.detail,
                ));
            }
        }
        s.push_str(&format!("\n=== SUITE {:?} (exit {}) ===\n", self.verdict, self.exit_code));
        s
    }
}

fn verdict_glyph(v: Verdict) -> &'static str {
    match v {
        Verdict::Pass => "PASS",
        Verdict::PassImproved => "IMPR",
        Verdict::Fail => "FAIL",
        Verdict::Inconclusive => "INCO",
    }
}
