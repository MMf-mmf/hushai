//! Spike (Phase A, the flagged long-pole risk): prove that `ort` (vision ONNX, dlopening its OWN
//! libonnxruntime 1.20.x via `load-dynamic`) and `sherpa-rs` (audio, with its STATICALLY-bundled
//! libonnxruntime 1.17.1) coexist in ONE process without duplicate-symbol corruption.
//!
//! Both halves actively create a session in their respective runtime — that is what exercises the
//! native libs in-process. If the two onnxruntimes clashed (flat namespace / shared symbol table),
//! this would crash or mis-resolve. On macOS two-level namespaces they're distinct images.
//!
//! Gated on the model + dylib files existing (like the repo's live-DB tests gate on DATABASE_URL),
//! so it's a no-op skip on a machine that hasn't provisioned them.
//!
//! Run: `cargo test -p hushai-worker --test ort_coexistence -- --nocapture`

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = .../hushai-worker ; the repo root is its parent.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn ort_and_sherpa_coexist_in_one_process() {
    let root = repo_root();
    let dylib = root
        .join("models/onnxruntime/onnxruntime-osx-arm64-1.20.0/lib/libonnxruntime.1.20.0.dylib");
    let vad_model = root.join("models/silero_vad.onnx");
    let titanet = root.join("models/nemo_en_titanet_large.onnx");

    if !dylib.exists() {
        eprintln!("SKIP: ORT dylib not provisioned at {}", dylib.display());
        return;
    }
    if !vad_model.exists() {
        eprintln!("SKIP: {} missing", vad_model.display());
        return;
    }

    // Point `ort` (load-dynamic) at the provisioned 1.20 dylib. Set before any ort call.
    unsafe { std::env::set_var("ORT_DYLIB_PATH", &dylib) };

    // 1) ort: initialize + create a session from a real ONNX model. This loads ort's
    //    libonnxruntime 1.20 and calls OrtCreateSession against it.
    let session = ort::session::Session::builder()
        .expect("ort session builder")
        .with_execution_providers([
            ort::execution_providers::CPUExecutionProvider::default().build()
        ])
        .expect("ort set execution providers")
        .commit_from_file(&vad_model)
        .expect("ort load silero_vad.onnx (ort's onnxruntime failed to init)");
    eprintln!(
        "ort OK: loaded {} ({} inputs, {} outputs) via libonnxruntime 1.20",
        vad_model.display(),
        session.inputs.len(),
        session.outputs.len()
    );

    // 2) sherpa-rs: construct an embedder in the SAME process. This loads sherpa's bundled
    //    libonnxruntime 1.17.1 and creates an extractor against it.
    if !titanet.exists() {
        eprintln!(
            "SKIP sherpa half: {} missing — ort half already proved load-dynamic works",
            titanet.display()
        );
        return;
    }
    let _embedder = hushai_worker::speaker::SpeakerEmbedder::new(titanet.to_str().unwrap())
        .expect("sherpa TitaNet embedder construction failed (coexistence broke sherpa's runtime)");
    eprintln!("sherpa OK: TitaNet embedder constructed alongside the live ort session");

    eprintln!(
        "COEXISTENCE OK: ort (1.20) and sherpa-rs (1.17.1) onnxruntimes both live in one process"
    );
}
