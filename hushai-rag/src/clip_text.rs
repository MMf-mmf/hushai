//! CLIP TEXT tower for open-vocabulary object retrieval (Phase B query side).
//!
//! "When did I see a car / a red mug" is answered by embedding the query phrase with this CLIP
//! TEXT tower (512-d) and nearest-neighbouring it against `scene_objects.embedding` — the SAME
//! 512-d space the worker's CLIP IMAGE tower (hushai-worker/src/vision/objects.rs `ClipEmbedder`)
//! wrote object/whole-frame crops into. Both towers MUST come from the same OpenCLIP ViT-B/32
//! checkpoint (export both with local_dev/export_clip.py) or the spaces won't align.
//!
//! COEXISTENCE (mirrors hushai-worker/src/vision/model.rs, see AGENTS.md "vision ONNX runtime"):
//! this crate links sherpa-onnx, which STATICALLY bundles its own libonnxruntime 1.17.1. `ort`
//! (ort-sys rc.9 → ONNX Runtime 1.20.0) therefore uses `load-dynamic` (no link-time onnxruntime)
//! and dlopen()s the SAME 1.20 dylib the worker uses via `ORT_DYLIB_PATH`. Proven coexistent by
//! hushai-worker/tests/ort_coexistence.rs.
//!
//! TOKENIZATION: the text tower expects `[1,77]` int64 CLIP-BPE token ids. We load the HF CLIP
//! tokenizer.json (local_dev/fetch_clip_tokenizer.sh) and pad/truncate to the 77-token context.
//! CLIP's text transformer is causal and pools at the EOT position, so the padding value after EOT
//! is irrelevant to the embedding — we pad with 0 to match open_clip.

use std::sync::Once;

use anyhow::{Context, Result, anyhow};
use ndarray::Array2;
use ort::execution_providers::CPUExecutionProvider;
use ort::session::Session;
use tokenizers::Tokenizer;

/// CLIP context length (token count fed to the text tower) and embedding dim.
const CONTEXT_LEN: usize = 77;
const EMBED_DIM: usize = 512;

static ORT_INIT: Once = Once::new();

/// Point `ort` at the provisioned libonnxruntime dylib (idempotent; before the first session).
/// Copied verbatim from hushai-worker/src/vision/model.rs so the two crates load ORT identically.
pub fn init_ort(dylib_path: &str) {
    ORT_INIT.call_once(|| {
        // SAFETY: called once, before any ort session is created, under the Once guard.
        unsafe { std::env::set_var("ORT_DYLIB_PATH", dylib_path) };
    });
}

/// Load an ONNX model into a CPU `Session`. (The CLIP text tower is tiny — no CoreML needed.)
fn load_session(model_path: &str) -> Result<Session> {
    Session::builder()
        .context("ort session builder")?
        .with_execution_providers([CPUExecutionProvider::default().build()])
        .context("registering CPU execution provider")?
        .commit_from_file(model_path)
        .with_context(|| format!("loading ONNX model {model_path}"))
}

/// L2-normalize in place. Copied from hushai-worker/src/vad.rs (rag can't depend on the worker) —
/// MUST match the worker's `ClipEmbedder` normalization so cosine over scene_objects is valid.
fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// OpenCLIP ViT-B/32 text tower → 512-d L2-normalized embedding for a query phrase.
pub struct ClipTextEmbedder {
    session: Session,
    tokenizer: Tokenizer,
}

impl ClipTextEmbedder {
    /// Build the text embedder. Calls `init_ort` first (so the same 1.20 dylib is used), then loads
    /// the text-tower ONNX + the CLIP tokenizer. Returns an error if either file is missing —
    /// the caller treats that as "object retrieval unavailable" (503), never fatal.
    pub fn new(model_path: &str, tokenizer_path: &str, dylib_path: &str) -> Result<Self> {
        init_ort(dylib_path);
        let session = load_session(model_path)?;
        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow!("loading CLIP tokenizer {tokenizer_path}: {e}"))?;
        Ok(Self { session, tokenizer })
    }

    /// Embed a query phrase into a 512-d L2-normalized vector in CLIP's joint image/text space.
    /// CPU-bound ONNX work — call inside `spawn_blocking` from async contexts.
    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        // CLIP BPE with special tokens (BOS=49406 / EOS=49407). Truncate to the 77-token context;
        // pad with 0 (value after EOT is irrelevant — causal pooling at the EOT position).
        let enc = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow!("clip tokenize {text:?}: {e}"))?;
        let mut ids: Vec<i64> = enc.get_ids().iter().map(|&i| i as i64).collect();
        ids.truncate(CONTEXT_LEN);
        ids.resize(CONTEXT_LEN, 0);

        let input = Array2::<i64>::from_shape_vec((1, CONTEXT_LEN), ids)
            .context("building clip text input tensor")?;
        let outputs = self
            .session
            .run(ort::inputs![input].context("clip text inputs")?)
            .context("clip text inference")?;
        let out_name = self
            .session
            .outputs
            .first()
            .map(|o| o.name.clone())
            .context("clip text tower has no outputs")?;
        let t = outputs[out_name.as_str()]
            .try_extract_tensor::<f32>()
            .context("extracting clip text embedding")?;
        let mut v: Vec<f32> = t.iter().copied().collect();
        anyhow::ensure!(
            v.len() == EMBED_DIM,
            "clip text embedding is {}-d, expected {EMBED_DIM}",
            v.len()
        );
        l2_normalize(&mut v);
        Ok(v)
    }
}
