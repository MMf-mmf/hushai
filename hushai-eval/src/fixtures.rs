//! Fixture + ground-truth format.
//!
//! A test case is a directory `fixtures/<split>/<case-id>/` containing:
//!   - the media file (e.g. `media.mp4` / `media.wav`)
//!   - `meta.json`     — how to inject + which modalities to score
//!   - `expected.json` — ground truth (every modality key is independently optional)
//!   - optional `refs/` — reference assets for identity enrollment
//!
//! All ground-truth time windows are expressed as nanosecond OFFSETS from
//! `meta.base_capture_unix_nanos`, so a single base shift relocates the whole fixture in the
//! timeline without rewriting ground truth. Scorers add the base before querying.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

// ----- meta.json -------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Meta {
    pub case_id: String,
    #[serde(default)]
    pub description: String,
    pub device_id: String,
    pub media_file: String,
    /// "audio" | "video" | "muxed" — only affects which lane(s) process the segments.
    #[serde(default = "d_muxed")]
    pub media_kind: String,
    #[serde(default = "d_seg_seconds")]
    pub seg_seconds: u32,
    #[serde(default)]
    pub limit: Option<u32>,
    /// FIXED capture-start so timestamps (and 30s event buckets) are deterministic.
    pub base_capture_unix_nanos: i64,
    /// Deterministic id seed; defaults to `case_id` when absent.
    #[serde(default)]
    pub segment_id_seed: Option<String>,
    /// Which lanes to poll + score. e.g. ["transcript","speakers","sentiment","events"].
    pub modalities: Vec<String>,
    /// "fast" | "full" — speed-tier membership.
    #[serde(default = "d_full")]
    pub tier: String,
    /// Per-case worker-config overrides (folded into the config-hash; informational here).
    #[serde(default)]
    pub config: serde_json::Map<String, serde_json::Value>,
    /// Reference-identity enrollment performed after reset, before injection.
    #[serde(default)]
    pub enroll: Vec<EnrollSpec>,
    #[serde(default)]
    pub poll: PollSpec,
}

impl Meta {
    /// Build an in-memory Meta for the `probe` flow (no meta.json on disk yet).
    pub fn synthetic(device_id: &str, case_id: &str, media_kind: &str, base_ns: i64, modalities: Vec<String>) -> Self {
        Meta {
            case_id: case_id.into(),
            description: "probe".into(),
            device_id: device_id.into(),
            media_file: "media.mp4".into(),
            media_kind: media_kind.into(),
            seg_seconds: 2,
            limit: None,
            base_capture_unix_nanos: base_ns,
            segment_id_seed: Some(case_id.into()),
            modalities,
            tier: "full".into(),
            config: serde_json::Map::new(),
            enroll: vec![],
            poll: PollSpec::default(),
        }
    }

    pub fn seed(&self) -> String {
        self.segment_id_seed.clone().unwrap_or_else(|| self.case_id.clone())
    }
    pub fn modality(&self, name: &str) -> bool {
        self.modalities.iter().any(|m| m == name)
    }
    /// True if any scored modality requires the vision lane.
    pub fn needs_vision(&self) -> bool {
        ["persons", "faces", "objects", "plates"].iter().any(|m| self.modality(m))
    }
    /// True if any scored modality requires the audio lane.
    pub fn needs_audio(&self) -> bool {
        ["transcript", "speakers", "sentiment"].iter().any(|m| self.modality(m))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnrollSpec {
    /// "speaker" | "person" | "plate"
    pub modality: String,
    pub name: String,
    /// Path (relative to the case dir) of a clip to inject-then-rename.
    #[serde(default)]
    pub r#ref: Option<String>,
    /// For plates: directly seed the catalog with this normalized string (no clip).
    #[serde(default)]
    pub plate_text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PollSpec {
    #[serde(default = "d_poll_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "d_poll_interval")]
    pub interval_secs: u64,
    /// Consecutive polls with unchanged event count before declaring quiescence.
    #[serde(default = "d_quiesce_polls")]
    pub quiesce_polls: u32,
}
impl Default for PollSpec {
    fn default() -> Self {
        Self { timeout_secs: d_poll_timeout(), interval_secs: d_poll_interval(), quiesce_polls: d_quiesce_polls() }
    }
}

// ----- expected.json ---------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Expected {
    pub transcript: Option<TranscriptGt>,
    pub speakers: Option<SpeakersGt>,
    pub sentiment: Option<SentimentGt>,
    pub persons: Option<PersonsGt>,
    pub objects: Option<ObjectsGt>,
    pub plates: Option<PlatesGt>,
    pub events: Option<EventsGt>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TranscriptGt {
    pub full_text: String,
    #[serde(default = "d_max_wer")]
    pub max_wer: f64,
    #[serde(default = "d_min_sim")]
    pub min_similarity: f64,
    #[serde(default)]
    pub windows: Vec<TextWindow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TextWindow {
    pub start_ns: i64,
    pub end_ns: i64,
    #[serde(default)]
    pub contains: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpeakersGt {
    pub distinct_count: i64,
    #[serde(default)]
    pub count_tolerance: i64,
    #[serde(default)]
    pub utterances: Vec<UttGt>,
    #[serde(default)]
    pub named: Vec<NamedGt>,
    #[serde(default = "d_min_purity")]
    pub min_purity: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UttGt {
    pub label: String,
    pub text_contains: String,
    pub window_ns: [i64; 2],
}

#[derive(Debug, Clone, Deserialize)]
pub struct NamedGt {
    pub label: String,
    pub expect_display_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SentimentGt {
    pub windows: Vec<SentWindow>,
    #[serde(default = "d_min_acc")]
    pub min_accuracy: f64,
    #[serde(default = "d_true")]
    pub allow_null: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SentWindow {
    pub start_ns: i64,
    pub end_ns: i64,
    pub label: String,
    #[serde(default)]
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PersonsGt {
    pub distinct_count: i64,
    #[serde(default)]
    pub count_tolerance: i64,
    #[serde(default = "d_one")]
    pub min_sightings: i64,
    #[serde(default)]
    pub named: Vec<PersonNamedGt>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PersonNamedGt {
    pub expect_display_name: String,
    #[serde(default = "d_one")]
    pub min_sightings: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObjectsGt {
    pub windows: Vec<ObjWindow>,
    #[serde(default = "d_min_f1")]
    pub min_label_f1: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObjWindow {
    pub start_ns: i64,
    pub end_ns: i64,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlatesGt {
    pub expected: Vec<PlateGt>,
    #[serde(default)]
    pub require_exact: bool,
    #[serde(default = "d_one")]
    pub max_norm_edit_distance: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlateGt {
    pub text: String,
    #[serde(default)]
    pub text_norm: Option<String>,
    #[serde(default = "d_one")]
    pub min_reads: i64,
    #[serde(default)]
    pub expect_display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventsGt {
    pub expected: Vec<EventGt>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventGt {
    pub event_type: String,
    #[serde(default)]
    pub subject_type: Option<String>,
    #[serde(default)]
    pub subject_label: Option<String>,
    #[serde(default = "d_one")]
    pub min_count: i64,
    #[serde(default)]
    pub max_count: Option<i64>,
    #[serde(default = "d_info")]
    pub min_severity: String,
}

// ----- loading + discovery ---------------------------------------------------

#[derive(Debug, Clone)]
pub struct Fixture {
    pub dir: PathBuf,
    pub split: String, // "train" | "holdout"
    pub meta: Meta,
    pub expected: Expected,
}

impl Fixture {
    pub fn media_path(&self) -> PathBuf {
        self.dir.join(&self.meta.media_file)
    }
    /// Total clip span in nanoseconds (best-effort: segments * seg_seconds; a slack is added at query time).
    pub fn nominal_span_ns(&self) -> i64 {
        // Without decoding the media we don't know the exact count; the query window uses a
        // generous slack on top of this, and the poll set comes from the injector's emitted ids.
        let secs = self.meta.limit.unwrap_or(64) as i64 * self.meta.seg_seconds as i64;
        secs * 1_000_000_000
    }
}

pub fn load(dir: &Path, split: &str) -> Result<Fixture> {
    let meta: Meta = read_json(&dir.join("meta.json"))
        .with_context(|| format!("reading meta.json in {}", dir.display()))?;
    let expected: Expected = read_json(&dir.join("expected.json"))
        .with_context(|| format!("reading expected.json in {}", dir.display()))?;
    Ok(Fixture { dir: dir.to_path_buf(), split: split.to_string(), meta, expected })
}

/// Discover fixtures under `<root>/<split>/*/` for the given splits.
pub fn discover(root: &Path, splits: &[&str]) -> Result<Vec<Fixture>> {
    let mut out = Vec::new();
    for split in splits {
        let base = root.join(split);
        if !base.is_dir() {
            continue;
        }
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&base)
            .with_context(|| format!("reading {}", base.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir() && p.join("meta.json").is_file())
            .collect();
        dirs.sort();
        for d in dirs {
            out.push(load(&d, split)?);
        }
    }
    Ok(out)
}

fn read_json<T: serde::de::DeserializeOwned>(p: &Path) -> Result<T> {
    let s = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    Ok(serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display()))?)
}

// ----- serde defaults --------------------------------------------------------

fn d_muxed() -> String { "muxed".into() }
fn d_full() -> String { "full".into() }
fn d_info() -> String { "info".into() }
fn d_seg_seconds() -> u32 { 2 }
fn d_poll_timeout() -> u64 { 180 }
fn d_poll_interval() -> u64 { 2 }
fn d_quiesce_polls() -> u32 { 2 }
fn d_true() -> bool { true }
fn d_one() -> i64 { 1 }
fn d_max_wer() -> f64 { 0.15 }
fn d_min_sim() -> f64 { 0.85 }
fn d_min_purity() -> f64 { 0.80 }
fn d_min_acc() -> f64 { 0.5 }
fn d_min_f1() -> f64 { 0.5 }
