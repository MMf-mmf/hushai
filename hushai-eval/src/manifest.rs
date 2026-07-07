//! Environment manifest + config-hash (Invariant 4 + 7).
//!
//! The config-hash folds the entire DETERMINISM-RELEVANT surface — model file fingerprints,
//! Ollama model digests, the ONNX-Runtime dylib, the execution provider, and the worker's
//! matcher/threshold knobs — into one SHA. Baselines are keyed by it. A change to ANY of these
//! mints a NEW lineage, so the harness never silently compares a run to a baseline made under a
//! different pipeline/environment. The git SHA and migration head are recorded for human
//! attribution but deliberately DO NOT enter the hash: ordinary code changes (the agent loop's
//! bread and butter) keep the hash stable so baseline comparisons stay valid.

use crate::ctx::Ctx;
use anyhow::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

/// Worker env knobs that change what the pipeline produces. Folded into the config-hash.
const KNOBS: &[&str] = &[
    "WORKER_CONCURRENCY",
    "SPEAKER_AUTOHEAL_ENABLED",
    "SPEAKER_BACKFILL_ON_START",
    "SPEAKER_REPROCESS_REJECTS_ON_START",
    "SPEAKER_MATCH_THRESHOLD",
    "SPEAKER_MINT_DISTANCE_FLOOR",
    "SPEAKER_MINT_MIN_SPEECH_SECS",
    "FACE_MATCH_THRESHOLD",
    "FACE_MINT_DISTANCE_FLOOR",
    "PLATE_MINT_MIN_OCR_CONF",
    "VISION_COREML",
    "OBJECT_REQUIRED",
    "PLATE_REQUIRED",
    "WHISPER_MODEL_PATH",
    "EMBED_MODEL",
    "RAG_LLM_MODEL",
    "SENTIMENT_MODEL",
    "SENTIMENT_ENABLED",
    "EVENTS_ENABLED",
    // RAG answer/routing determinism + retrieval shape. These now determine SCORED output (the
    // `chat` modality scores live RAG answers), so a baseline is only comparable under the same
    // decode + retrieval profile. `RAG_`/`OWNER_`/`ANALYSIS_`/`REFLECTION_` are deliberately NOT
    // prefix-folded (that would sweep in secrets/urls/bind addr and fragment baselines per machine);
    // the output-determining ones are hand-listed here instead.
    "RAG_LLM_TEMPERATURE",
    "RAG_LLM_SEED",
    "RAG_TOP_K_DEFAULT",
    "RAG_DISTANCE_THRESHOLD",
    "RAG_HNSW_EF_SEARCH",
    "RAG_OBJECT_DISTANCE_THRESHOLD",
    "RAG_OBJECT_TOP_K_DEFAULT",
    "RAG_PERSON_TOP_K_DEFAULT",
    "RAG_PLATE_TOP_K_DEFAULT",
    "RAG_CHAT_HISTORY_TURNS",
    "RAG_QUERY_CONDENSE",
    "REFLECTION_LLM_MODEL",
    "ANALYSIS_WINDOW_DAYS_DEFAULT",
    "CONVERSATION_GAP_SECS",
    "ANALYSIS_TZ_OFFSET_SECS",
    "OWNER_SPEAKER_ID",
    "OWNER_SPEAKER_NAME",
    "OWNER_PERSON_ID",
    "OWNER_PERSON_NAME",
];

/// Env-var PREFIXES whose vars change what the pipeline produces. Folded into the config-hash BY
/// PREFIX (not only the hand-list above) so a NEW knob is captured automatically. A hand-maintained
/// allowlist previously omitted many output-determining knobs (OBJECT_*/PLATE_*/FACE_*/AUDIO_SILENCE_*/
/// EVENTS_*/ASR_*/…), so tuning them reused a STALE baseline → silent false-pass. Over-inclusion (a
/// perf-only timeout minting a fresh baseline lineage) is deliberate and cheap; a MISSED knob is not.
const KNOB_PREFIXES: &[&str] = &[
    "OBJECT_",
    "PLATE_",
    "FACE_",
    "SPEAKER_",
    "AUDIO_SILENCE_",
    "EVENTS_",
    "WHISPER_",
    "ASR_",
    "SENTIMENT_",
    "VISION_",
    "MOTION_",
    "LOAD_GOVERNOR_",
    "LOAD_PAUSE_",
    "FRAMES_PER_SEGMENT",
    // Conversation threading (0025): every threader knob changes conversation_id output.
    "THREADER_",
    "CONVO_",
    "RAG_EXPAND_",
    "RAG_PRUNE_",
];

#[derive(Debug, Clone, Serialize)]
pub struct EnvManifest {
    pub config_hash: String,
    pub git_sha: String,
    pub git_dirty: bool,
    pub migration_head: String,
    pub models: BTreeMap<String, String>,
    pub ollama: BTreeMap<String, String>,
    pub ort_dylib: String,
    pub execution_provider: String,
    pub knobs: BTreeMap<String, String>,
}

/// The subset that the config-hash is computed over (stable field order via BTreeMap).
#[derive(Serialize)]
struct HashSurface<'a> {
    models: &'a BTreeMap<String, String>,
    ollama: &'a BTreeMap<String, String>,
    ort_dylib: &'a str,
    execution_provider: &'a str,
    knobs: &'a BTreeMap<String, String>,
}

impl EnvManifest {
    pub async fn collect(ctx: &Ctx, per_case_config: &serde_json::Map<String, serde_json::Value>) -> Result<Self> {
        let models = fingerprint_models(&ctx.repo_root.join("models"));
        let ollama = ollama_digests(ctx).await;
        let ort_dylib = find_ort_dylib(&ctx.repo_root);
        let execution_provider = if env_bool("VISION_COREML", true) { "coreml" } else { "cpu" }.to_string();

        let mut knobs = BTreeMap::new();
        for k in KNOBS {
            if let Ok(v) = std::env::var(k) {
                knobs.insert((*k).to_string(), v);
            }
        }
        // Fold ANY env var under a determinism-relevant prefix, so a newly-added output-determining
        // knob is captured without editing the hand-list (the gap that let knob tuning silently reuse
        // a stale baseline → false pass). BTreeMap dedups against the explicit KNOBS above.
        for (k, v) in std::env::vars() {
            if KNOB_PREFIXES.iter().any(|p| k.starts_with(p)) {
                knobs.insert(k, v);
            }
        }
        // Per-case worker-config overrides participate in the hash too (a case can pin a knob).
        for (k, v) in per_case_config {
            knobs.insert(format!("case::{k}"), v.to_string());
        }

        let surface = HashSurface {
            models: &models,
            ollama: &ollama,
            ort_dylib: &ort_dylib,
            execution_provider: &execution_provider,
            knobs: &knobs,
        };
        let canonical = serde_json::to_string(&surface)?;
        let config_hash = hex::encode(Sha256::digest(canonical.as_bytes()))[..16].to_string();

        let (git_sha, git_dirty) = git_state(&ctx.repo_root);
        let migration_head = migration_head(ctx).await.unwrap_or_else(|_| "unknown".into());

        Ok(Self { config_hash, git_sha, git_dirty, migration_head, models, ollama, ort_dylib, execution_provider, knobs })
    }
}

/// Fingerprint every model weight (len:mtime is fast and sufficient to detect a swapped file).
fn fingerprint_models(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>, depth: usize) {
        if depth > 3 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out, depth + 1);
            } else if matches!(
                p.extension().and_then(|s| s.to_str()),
                Some("onnx") | Some("bin") | Some("gguf") | Some("json")
            ) {
                if let Ok(md) = e.metadata() {
                    let mtime = md
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let rel = p.strip_prefix(base).unwrap_or(&p).to_string_lossy().to_string();
                    out.insert(rel, format!("{}:{}", md.len(), mtime));
                }
            }
        }
    }
    walk(dir, dir, &mut out, 0);
    out
}

async fn ollama_digests(ctx: &Ctx) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let url = format!("{}/api/tags", ctx.ollama_url.trim_end_matches('/'));
    if let Ok(resp) = ctx.http.get(&url).send().await {
        if let Ok(json) = resp.json::<serde_json::Value>().await {
            if let Some(models) = json.get("models").and_then(|m| m.as_array()) {
                for m in models {
                    let name = m.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let digest = m.get("digest").and_then(|d| d.as_str()).unwrap_or("");
                    if !name.is_empty() {
                        out.insert(name.to_string(), digest.chars().take(16).collect());
                    }
                }
            }
        }
    }
    out
}

fn find_ort_dylib(repo_root: &Path) -> String {
    if let Ok(p) = std::env::var("ORT_DYLIB_PATH") {
        if let Some(name) = Path::new(&p).file_name().and_then(|s| s.to_str()) {
            return name.to_string();
        }
    }
    // Best-effort: scan models/onnxruntime for a libonnxruntime*.dylib name.
    let ort = repo_root.join("models/onnxruntime");
    fn scan(dir: &Path, depth: usize) -> Option<String> {
        if depth > 4 {
            return None;
        }
        for e in std::fs::read_dir(dir).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(n) = scan(&p, depth + 1) {
                    return Some(n);
                }
            } else if let Some(n) = p.file_name().and_then(|s| s.to_str()) {
                if n.starts_with("libonnxruntime") && n.ends_with(".dylib") {
                    return Some(n.to_string());
                }
            }
        }
        None
    }
    scan(&ort, 0).unwrap_or_else(|| "none".into())
}

fn git_state(repo_root: &Path) -> (String, bool) {
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_root)
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    (sha, dirty)
}

async fn migration_head(ctx: &Ctx) -> Result<String> {
    let row: (i64, String) = sqlx::query_as(
        "SELECT version, description FROM _sqlx_migrations ORDER BY version DESC LIMIT 1",
    )
    .fetch_one(&ctx.pool)
    .await?;
    Ok(format!("{} {}", row.0, row.1))
}

fn env_bool(k: &str, default: bool) -> bool {
    std::env::var(k).ok().map(|v| v == "true" || v == "1").unwrap_or(default)
}
