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
    // Gotham "Detective" agentic runtime (G3, the `agent` eval modality). These SHAPE the streamed
    // answer + tool trace the `agent` scorer asserts on, so a baseline is only comparable under the
    // identical decode + loop-bound profile. HAND-LISTED (not a `GOTHAM_` prefix-fold) ON PURPOSE:
    // prefix-folding would sweep the secret `GOTHAM_BACKEND_TOKEN` + the machine-specific
    // `GOTHAM_BACKEND_BASE_URL` into every machine's hash and fragment baselines. Output-shaping
    // knobs only — audit/mutation/briefing-wall-clock knobs (`GOTHAM_AUDIT_READS`,
    // `GOTHAM_MUTATIONS_ENABLED`, `GOTHAM_BRIEFING_*`, `GOTHAM_CONFIRM_TTL_SECS`) are deliberately
    // omitted (no effect on a Phase-1 read-only answer). NOTE: these fold into the hash ONLY when
    // actually SET in the eval process env — they are pinned in a staging-only env layer, NOT the
    // shared `local_dev/eval.env`, so the frozen `graph`/perception lineage (d4acc862) is untouched
    // by a plain `--fixtures all` run.
    "GOTHAM_ENABLED",
    "GOTHAM_RUNTIME",
    "GOTHAM_LLM_MODEL",
    "GOTHAM_JUDGE_MODEL",
    "GOTHAM_TEMPERATURE",
    "GOTHAM_SEED",
    "GOTHAM_NUM_CTX",
    "GOTHAM_MAX_TURNS",
    "GOTHAM_MAX_TOOL_CALLS",
    "GOTHAM_VOICE_MAX_TOOL_CALLS",
    "GOTHAM_TOOL_TIMEOUT_MS",
    "GOTHAM_WALL_CLOCK_SECS",
    "GOTHAM_TOOL_RESULT_MAX_CHARS",
    "GOTHAM_OBS_TOTAL_MAX_CHARS",
    "GOTHAM_CRITIQUE_ENABLED",
];

/// Ahithophel advisor knobs (the `advisor` eval modality). These SHAPE the streamed consultation +
/// its grounding + the memory recall the advisor scorer asserts on, so an advisor baseline is only
/// comparable under the identical decode + gate/route/memory profile. HAND-LISTED (not an
/// `ADVISOR_` prefix-fold) ON PURPOSE: prefix-folding would sweep the secret `ADVISOR_TOKEN` and
/// the machine-specific `ADVISOR_BIND_ADDR`/`ADVISOR_TLS_*` into every machine's hash.
///
/// Folded into the config-hash ONLY for advisor cases (`include_corpus`), NOT globally — because
/// UNLIKE the GOTHAM knobs (kept OUT of the shared `local_dev/eval.env`), the advisor determinism
/// pins `ADVISOR_LLM_TEMPERATURE`/`ADVISOR_LLM_SEED` ARE set in `eval.env` (they configure the live
/// advisor SERVICE). A global fold would therefore sweep them into every perception/graph
/// `--fixtures all` run and clobber the frozen `d4acc862` lineage. Gating on advisor-ness keeps
/// `d4acc862` byte-stable while still minting a fresh lineage the moment an advisor knob changes.
const ADVISOR_KNOBS: &[&str] = &[
    "ADVISOR_LLM_MODEL",
    "ADVISOR_JUDGE_MODEL",
    "ADVISOR_LLM_TEMPERATURE",
    "ADVISOR_LLM_SEED",
    "ADVISOR_NUM_CTX",
    "ADVISOR_MAX_FOLLOWUP_ROUNDS",
    "ADVISOR_MAX_QUESTIONS_PER_ROUND",
    "ADVISOR_MAX_REFINE_ITERS",
    "ADVISOR_MAX_CHAPTERS_PER_ROUTE",
    "ADVISOR_MAX_TOTAL_CHAPTERS",
    "ADVISOR_HISTORY_TURNS",
    "ADVISOR_CHAPTER_MAX_CHARS",
    "ADVISOR_CONTEXT_MAX_TOTAL_CHARS",
    "ADVISOR_ROUTE_SEMANTIC_TOP_K",
    "ADVISOR_MEMORY_TOP_K",
    "ADVISOR_MEMORY_DISTANCE_THRESHOLD",
    "ADVISOR_MEMORY_ENABLED",
    "ADVISOR_MAX_MESSAGE_CHARS",
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
    // Gotham entity graph (0028): every GRAPH_ knob changes the folded entity_edges (co-presence
    // slack, vehicle-correlation window, binding thresholds, sample cap, grace) — determinism-
    // relevant. Safe to prefix-fold: the family carries no secrets/URLs/bind-addrs.
    "GRAPH_",
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
    /// `include_corpus` folds the advisor book-corpus fingerprint into the hash — passed `true`
    /// ONLY for advisor cases (`FixtureMeta::needs_advisor`). A perception/graph `--fixtures all`
    /// run passes `false`, so its config-hash never sees the corpus key even when the shared
    /// `hushai_test` DB also holds an ingested book — the frozen `d4acc862` lineage is preserved.
    pub async fn collect(
        ctx: &Ctx,
        per_case_config: &serde_json::Map<String, serde_json::Value>,
        include_corpus: bool,
    ) -> Result<Self> {
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
        // Advisor determinism knobs + corpus lineage — folded ONLY for advisor cases so the frozen
        // perception/graph `d4acc862` lineage is byte-stable even though `eval.env` sets the advisor
        // service's `ADVISOR_LLM_TEMPERATURE`/`SEED` (see `ADVISOR_KNOBS`). A re-clean / re-ingest /
        // different book (the corpus fingerprint) or any advisor knob change mints a fresh advisor
        // lineage; a media/graph `--fixtures all` run carries NEITHER key.
        if include_corpus {
            for k in ADVISOR_KNOBS {
                if let Ok(v) = std::env::var(k) {
                    knobs.insert((*k).to_string(), v);
                }
            }
            knobs.insert("corpus::book".to_string(), corpus_fingerprint(ctx).await);
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

/// Fingerprint the ingested advisor book corpus: chapter count + an md5 over the synopses (the
/// routing surface every answer is grounded through). `"absent"` when `book_chapters` doesn't
/// exist (a DB without the advisor migrations). Folded into the hash only for advisor cases.
async fn corpus_fingerprint(ctx: &Ctx) -> String {
    match sqlx::query_as::<_, (i64, Option<String>)>(
        "SELECT count(*), md5(string_agg(synopsis, '' ORDER BY chapter_no)) FROM book_chapters",
    )
    .fetch_one(&ctx.pool)
    .await
    {
        Ok((n, md5)) => format!("{n}:{}", md5.unwrap_or_default()),
        Err(_) => "absent".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{ADVISOR_KNOBS, KNOBS, KNOB_PREFIXES};

    /// Guard the d4acc862-preservation contract at the const level. The advisor knobs are folded
    /// ONLY for advisor cases (`eval.env` pins the advisor service's `ADVISOR_LLM_TEMPERATURE`/
    /// `SEED`, so a GLOBAL fold would sweep them into every perception/graph `--fixtures all` hash).
    /// This test asserts every path that could regress that:
    ///   1. `ADVISOR_KNOBS` are well-formed and free of the secret/machine-specific vars a prefix
    ///      fold would have swept (`ADVISOR_TOKEN`, `ADVISOR_BIND_ADDR`, `ADVISOR_TLS_*`).
    ///   2. No `ADVISOR_KNOBS` entry is ALSO in the global `KNOBS` (double-fold).
    ///   3. No global `KNOBS` entry is `ADVISOR_`-prefixed (a new advisor knob added to the wrong
    ///      list would fold into every perception hash).
    ///   4. No `KNOB_PREFIXES` entry matches an `ADVISOR_` var — the exact trap the `ADVISOR_KNOBS`
    ///      docstring warns against; the prefix fold (unlike the hand-list) is NOT advisor-gated, so
    ///      an `"ADVISOR_"` prefix here would clobber d4acc862 for every run.
    #[test]
    fn advisor_knobs_isolated_and_secret_free() {
        for k in ADVISOR_KNOBS {
            assert!(k.starts_with("ADVISOR_"), "{k} misfiled in ADVISOR_KNOBS");
            assert!(!KNOBS.contains(k), "{k} must not also be in global KNOBS (would clobber d4acc862)");
        }
        for forbidden in ["ADVISOR_TOKEN", "ADVISOR_BIND_ADDR", "ADVISOR_TLS_CERT", "ADVISOR_TLS_KEY"] {
            assert!(!ADVISOR_KNOBS.contains(&forbidden), "{forbidden} is a secret/machine var — must never fold");
        }
        assert_eq!(ADVISOR_KNOBS.len(), 18, "the spec hand-lists 18 output-shaping advisor knobs");
        // The two regression vectors that would silently breach d4acc862 (both fold GLOBALLY, not
        // advisor-gated): a raw ADVISOR_ var in the global KNOBS, or an ADVISOR_ prefix fold.
        for k in KNOBS {
            assert!(!k.starts_with("ADVISOR_"), "{k}: advisor knobs belong in ADVISOR_KNOBS (advisor-gated), not the global KNOBS");
        }
        for p in KNOB_PREFIXES {
            for k in ADVISOR_KNOBS {
                assert!(!k.starts_with(p), "prefix {p} captures advisor knob {k} — the prefix fold is NOT advisor-gated, so it would clobber d4acc862");
            }
            assert!(!p.starts_with("ADVISOR"), "{p} is ADVISOR-scoped — the prefix fold would sweep ADVISOR_TOKEN + eval.env's ADVISOR_LLM_* into every perception hash and clobber d4acc862");
        }
    }
}
