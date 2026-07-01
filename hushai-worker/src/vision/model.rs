//! ORT (ONNX Runtime) session setup for the vision models.
//!
//! COEXISTENCE (see AGENTS.md "vision ONNX runtime"): sherpa-rs (the audio stack) bundles its own
//! libonnxruntime 1.17.1, dynamically linked into libsherpa-onnx-c-api.dylib via two-level
//! namespace. `ort` (ort-sys rc.9 → ONNX Runtime 1.20.0) therefore uses the `load-dynamic` feature
//! (no link-time onnxruntime dependency) and dlopen()s its OWN 1.20 dylib at runtime via
//! `ORT_DYLIB_PATH`. The two onnxruntimes are distinct Mach-O images and coexist; proven by
//! `tests/ort_coexistence.rs`.

use std::sync::Once;

use anyhow::{Context, Result};
use ort::execution_providers::{CPUExecutionProvider, CoreMLExecutionProvider};
use ort::session::Session;

static ORT_INIT: Once = Once::new();

/// Point `ort` at the provisioned libonnxruntime dylib. Idempotent; must run before the first
/// session is built. `ort` reads `ORT_DYLIB_PATH` lazily on first use, so setting it once here is
/// sufficient and avoids a hard ort::init dependency ordering.
pub fn init_ort(dylib_path: &str) {
    ORT_INIT.call_once(|| {
        // SAFETY: called once, before any ort session is created, under the Once guard.
        unsafe { std::env::set_var("ORT_DYLIB_PATH", dylib_path) };
    });
}

/// Load an ONNX model into a `Session` with ORT's default threading (all physical cores intra-op).
/// Registers CoreML (best-effort, Apple Silicon) ahead of CPU when `coreml` is set; ort silently
/// falls back to CPU for nodes CoreML can't take. Used by tests and simple callers; the worker uses
/// [`load_session_with_threads`] to apply its CPU thread budget.
pub fn load_session(model_path: &str, coreml: bool) -> Result<Session> {
    load_session_with_threads(model_path, coreml, 0)
}

/// Like [`load_session`] but caps ORT intra-op threads to `intra_threads` (`0` = ORT default, i.e.
/// all cores). The worker passes a per-session budget (`WorkerConfig::ort_intra_op_threads`) so N
/// parallel vision loops don't each spin up an all-core thread pool and oversubscribe the CPU.
/// CoreML-offloaded nodes are unaffected by this cap (it governs only CPU-fallback ops).
pub fn load_session_with_threads(
    model_path: &str,
    coreml: bool,
    intra_threads: usize,
) -> Result<Session> {
    let mut eps = Vec::new();
    if coreml {
        eps.push(CoreMLExecutionProvider::default().build());
    }
    eps.push(CPUExecutionProvider::default().build());

    let mut builder = Session::builder()
        .context("ort session builder")?
        .with_execution_providers(eps)
        .context("registering execution providers")?;
    if intra_threads > 0 {
        builder = builder
            .with_intra_threads(intra_threads)
            .context("setting ORT intra-op thread count")?;
    }
    builder
        .commit_from_file(model_path)
        .with_context(|| format!("loading ONNX model {model_path}"))
}
