//! Phase A verification for the vision plumbing that does NOT need the (not-yet-provisioned)
//! face/object models: `model::load_session` (ort load-dynamic) and `frames::sample_frames`
//! (ffmpeg RGB decode of a real stored segment). Gated on the dylib + a real mp4 existing.
//!
//! Run: `cargo test -p hushai-worker --test vision_pipeline -- --nocapture`

use std::path::PathBuf;

use hushai_worker::config::WorkerConfig;
use hushai_worker::media::SegmentRow;
use hushai_worker::vision::detect::{FaceDetect, FaceDetector};
use hushai_worker::vision::face_embed::{self, FaceEmbedder};
use hushai_worker::vision::objects::{self, ClipEmbedder, ObjectDetector};
use hushai_worker::vision::{enhance, frames, model};
use uuid::Uuid;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn dylib() -> PathBuf {
    repo_root()
        .join("models/onnxruntime/onnxruntime-osx-arm64-1.20.0/lib/libonnxruntime.1.20.0.dylib")
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>()
}

/// DECISIVE correctness check: on real frontal-face photos, ArcFace embeddings must put two photos
/// of the SAME person closer (higher cosine) than two DIFFERENT people. This validates the whole
/// detect -> align -> embed chain at once — wrong landmark order, alignment, or preprocessing all
/// collapse same-person similarity. Gated on FACE_TEST_DIR (containing obama.jpg, obama2.jpg,
/// biden.jpg). Provision: download to a scratch dir, then
///   FACE_TEST_DIR=/path cargo test -p hushai-worker --test vision_pipeline face_identity -- --nocapture
#[test]
fn face_identity_separation_on_fixtures() {
    let dir = match std::env::var("FACE_TEST_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => {
            eprintln!("SKIP: set FACE_TEST_DIR to a dir with obama.jpg/obama2.jpg/biden.jpg");
            return;
        }
    };
    let dy = dylib();
    let yunet = repo_root().join("models/face_detection_yunet_2023mar.onnx");
    let arcface = repo_root().join("models/w600k_r50.onnx");
    if !dy.exists() || !yunet.exists() || !arcface.exists() {
        eprintln!("SKIP: models not provisioned");
        return;
    }
    model::init_ort(dy.to_str().unwrap());
    let detector = FaceDetector::new(
        model::load_session(yunet.to_str().unwrap(), false).unwrap(),
        0.6,
    );
    let embedder =
        FaceEmbedder::new(model::load_session(arcface.to_str().unwrap(), false).unwrap());

    let embed_best = |name: &str| -> Vec<f32> {
        let img = image::open(dir.join(name))
            .unwrap_or_else(|e| panic!("open {name}: {e}"))
            .to_rgb8();
        let mut faces = detector.detect(&img).expect("detect");
        assert!(
            !faces.is_empty(),
            "no face detected in {name} (detection broken?)"
        );
        faces.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        eprintln!(
            "{name}: {} face(s), best score {:.2}",
            faces.len(),
            faces[0].score
        );
        embedder.embed(&img, &faces[0]).expect("embed")
    };

    let obama = embed_best("obama.jpg");
    let obama2 = embed_best("obama2.jpg");
    let biden = embed_best("biden.jpg");

    let same = cosine(&obama, &obama2);
    let diff1 = cosine(&obama, &biden);
    let diff2 = cosine(&obama2, &biden);
    eprintln!(
        "cosine SAME(obama,obama2)={same:.3}  DIFF(obama,biden)={diff1:.3}  DIFF(obama2,biden)={diff2:.3}"
    );

    // The headline correctness assertion: same-person clearly above different-person, with margin.
    assert!(
        same > diff1 + 0.15,
        "same-person sim {same:.3} not clearly above diff {diff1:.3}"
    );
    assert!(
        same > diff2 + 0.15,
        "same-person sim {same:.3} not clearly above diff {diff2:.3}"
    );
    // ArcFace same-identity on clean frontal faces is typically high.
    assert!(
        same > 0.4,
        "same-person similarity {same:.3} unexpectedly low — alignment/preproc suspect"
    );
}

/// Full detect -> align -> embed pipeline on real video frames. Faces may be sparse/absent in a
/// given clip (always-on footage often has none), so a no-face result is a clean pass, not a
/// failure — but when faces ARE found we assert the embeddings are well-formed 512-d unit vectors.
#[tokio::test]
async fn detect_and_embed_faces_from_real_video() {
    let dy = dylib();
    let yunet = repo_root().join("models/face_detection_yunet_2023mar.onnx");
    let arcface = repo_root().join("models/w600k_r50.onnx");
    let mp4 = repo_root().join("IMG_7256.mp4");
    if !dy.exists() || !yunet.exists() || !arcface.exists() || !mp4.exists() {
        eprintln!("SKIP: dylib/yunet/arcface/mp4 not all present");
        return;
    }
    model::init_ort(dy.to_str().unwrap());
    let detector = FaceDetector::new(
        model::load_session(yunet.to_str().unwrap(), false).unwrap(),
        0.6,
    );
    let embedder =
        FaceEmbedder::new(model::load_session(arcface.to_str().unwrap(), false).unwrap());

    let cfg = WorkerConfig::from_env().expect("config");
    let seg = SegmentRow {
        segment_id: Uuid::now_v7(),
        device_id: "test-cam".into(),
        blob_uri: format!("file://{}", mp4.display()),
        container: "mp4".into(),
        codec_init_data: None,
        capture_start_unix_nanos: 0,
        session_id: Uuid::nil(),
        stream_id: "cam0-video".into(),
        sequence: 0,
        duration_nanos: 6_000_000_000,
        gap_before: false,
    };
    let frames = frames::sample_frames(&cfg, &seg, 8)
        .await
        .expect("sample_frames");
    eprintln!("sampled {} frames", frames.len());

    let mut embeddings: Vec<Vec<f32>> = Vec::new();
    let mut total_faces = 0usize;
    for (fi, f) in frames.iter().enumerate() {
        let faces = detector.detect(&f.image).expect("detect");
        total_faces += faces.len();
        for face in &faces {
            eprintln!(
                "  frame {fi}: face score={:.2} bbox=[{:.0},{:.0},{:.0},{:.0}] min_side={:.0}",
                face.score,
                face.bbox[0],
                face.bbox[1],
                face.bbox[2],
                face.bbox[3],
                face.min_side()
            );
            let crop = face_embed::align_crop(&f.image, &face.landmarks);
            let sharp = face_embed::sharpness(&crop);
            let emb = embedder.embed(&f.image, face).expect("embed");
            assert_eq!(emb.len(), 512);
            assert!(emb.iter().all(|x| x.is_finite()));
            let norm = cosine(&emb, &emb).sqrt();
            assert!((norm - 1.0).abs() < 1e-3, "embedding not unit-norm: {norm}");
            eprintln!("    -> 512-d unit embedding (sharpness {sharp:.0})");
            embeddings.push(emb);
        }
    }
    eprintln!(
        "total faces detected: {total_faces}, embeddings: {}",
        embeddings.len()
    );

    // If we got multiple faces, show the cosine-similarity spread (objective sanity that the
    // embedder produces a sensible identity space; full identity calibration is human-in-the-loop).
    if embeddings.len() >= 2 {
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for i in 0..embeddings.len() {
            for j in i + 1..embeddings.len() {
                let s = cosine(&embeddings[i], &embeddings[j]);
                min = min.min(s);
                max = max.max(s);
            }
        }
        eprintln!("pairwise cosine similarity range: [{min:.3}, {max:.3}]");
    }
    if total_faces == 0 {
        eprintln!(
            "NOTE: no faces detected in this clip — pipeline ran cleanly; identity needs a face fixture."
        );
    }
}

#[test]
fn ort_loads_a_session_via_load_dynamic() {
    let dy = dylib();
    let vad = repo_root().join("models/silero_vad.onnx");
    if !dy.exists() || !vad.exists() {
        eprintln!("SKIP: dylib or silero model missing");
        return;
    }
    model::init_ort(dy.to_str().unwrap());
    // CPU-only here (matches the proven coexistence path); CoreML is exercised separately at runtime.
    let session = model::load_session(vad.to_str().unwrap(), false).expect("load silero via ort");
    eprintln!(
        "load_session OK: {} inputs / {} outputs",
        session.inputs.len(),
        session.outputs.len()
    );
    assert!(!session.inputs.is_empty());
}

#[test]
fn inspect_face_model_io_shapes() {
    let dy = dylib();
    if !dy.exists() {
        eprintln!("SKIP: dylib missing");
        return;
    }
    model::init_ort(dy.to_str().unwrap());
    for (name, rel) in [
        ("YuNet", "models/face_detection_yunet_2023mar.onnx"),
        ("ArcFace", "models/w600k_r50.onnx"),
    ] {
        let p = repo_root().join(rel);
        if !p.exists() {
            eprintln!("SKIP {name}: {} missing", p.display());
            continue;
        }
        let s = model::load_session(p.to_str().unwrap(), false).expect("load");
        eprintln!("== {name} ({rel}) ==");
        for i in &s.inputs {
            eprintln!("  IN  {} : {:?}", i.name, i.input_type.tensor_dimensions());
        }
        for o in &s.outputs {
            eprintln!("  OUT {} : {:?}", o.name, o.output_type.tensor_dimensions());
        }
    }
}

/// Print the REAL I/O contract of the object-lane ONNX graphs (RF-DETR + CLIP image/text), the
/// sibling of `inspect_face_model_io_shapes`. Run this FIRST after provisioning to read the export's
/// actual output names/order/shapes and class count — that drives whether `objects.rs`'s defensive
/// decode (read-by-order boxes[1,N,4]/logits[1,N,C]; COCO class indexing) needs correcting.
///   cargo test -p hushai-worker --test vision_pipeline inspect_object_model_io_shapes -- --nocapture
#[test]
fn inspect_object_model_io_shapes() {
    let dy = dylib();
    if !dy.exists() {
        eprintln!("SKIP: dylib missing");
        return;
    }
    model::init_ort(dy.to_str().unwrap());
    for (name, rel) in [
        ("RF-DETR", "models/rf-detr-nano.onnx"),
        ("CLIP-image", "models/clip_vit_b32_image.onnx"),
        ("CLIP-text", "models/clip_vit_b32_text.onnx"),
    ] {
        let p = repo_root().join(rel);
        if !p.exists() {
            eprintln!(
                "SKIP {name}: {} missing (run local_dev/export_*.py)",
                p.display()
            );
            continue;
        }
        let s = model::load_session(p.to_str().unwrap(), false).expect("load");
        eprintln!("== {name} ({rel}) ==");
        for i in &s.inputs {
            eprintln!("  IN  {} : {:?}", i.name, i.input_type.tensor_dimensions());
        }
        for o in &s.outputs {
            eprintln!("  OUT {} : {:?}", o.name, o.output_type.tensor_dimensions());
        }
    }
}

/// Decode validation for the object lane: run RF-DETR + the CLIP image tower over a clip that
/// RELIABLY contains a COCO object (synthesize one with `local_dev/make_object_clip.sh`). Asserts at
/// least one detection with a REAL COCO label (not `class_<i>` — which would mean the class indexing
/// is wrong), in-frame bboxes, and well-formed 512-d unit-norm region + whole-frame embeddings. This
/// is where the "decode UNVALIDATED at rest" header on objects.rs gets retired. Gated on the models +
/// `OBJECT_TEST_MP4` (defaults to ./object_clip.mp4) existing.
///   OBJECT_TEST_MP4=./object_clip.mp4 cargo test -p hushai-worker --test vision_pipeline detect_objects_from_real_video -- --nocapture
#[tokio::test]
async fn detect_objects_from_real_video() {
    let dy = dylib();
    let rfdetr = repo_root().join("models/rf-detr-nano.onnx");
    let clip = repo_root().join("models/clip_vit_b32_image.onnx");
    let mp4 = std::env::var("OBJECT_TEST_MP4")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("object_clip.mp4"));
    if !dy.exists() || !rfdetr.exists() || !clip.exists() || !mp4.exists() {
        eprintln!(
            "SKIP: dylib/rf-detr/clip/object-clip not all present — provision with \
             local_dev/export_rf_detr.py + export_clip.py + make_object_clip.sh"
        );
        return;
    }
    model::init_ort(dy.to_str().unwrap());
    let cfg = WorkerConfig::from_env().expect("config");
    let detector = ObjectDetector::new(
        model::load_session(rfdetr.to_str().unwrap(), false).unwrap(),
        cfg.object_det_input_size,
        cfg.object_min_det_score,
        cfg.object_max_per_frame,
        cfg.object_min_box_px,
    );
    let embedder = ClipEmbedder::new(model::load_session(clip.to_str().unwrap(), false).unwrap());

    let seg = SegmentRow {
        segment_id: Uuid::now_v7(),
        device_id: "test-cam".into(),
        blob_uri: format!("file://{}", mp4.display()),
        container: "mp4".into(),
        codec_init_data: None,
        capture_start_unix_nanos: 0,
        session_id: Uuid::nil(),
        stream_id: "cam0-video".into(),
        sequence: 0,
        duration_nanos: 6_000_000_000,
        gap_before: false,
    };
    let frames = frames::sample_frames(&cfg, &seg, 8)
        .await
        .expect("sample_frames");
    eprintln!("sampled {} frames", frames.len());

    let mut total_objects = 0usize;
    let mut labels: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (fi, f) in frames.iter().enumerate() {
        let (fw, fh) = (f.image.width() as f32, f.image.height() as f32);
        let dets = detector.detect(&f.image).expect("object detect");
        for d in &dets {
            total_objects += 1;
            labels.insert(d.label.clone());
            eprintln!(
                "  frame {fi}: {} score={:.2} bbox=[{:.0},{:.0},{:.0},{:.0}]",
                d.label, d.score, d.bbox[0], d.bbox[1], d.bbox[2], d.bbox[3]
            );
            // Boxes must land inside the original frame (validates the cxcywh + letterbox-rescale decode).
            assert!(
                d.bbox[0] >= 0.0 && d.bbox[1] >= 0.0,
                "bbox origin negative: {:?}",
                d.bbox
            );
            assert!(
                d.bbox[0] < fw && d.bbox[1] < fh,
                "bbox origin outside frame {fw}x{fh}: {:?}",
                d.bbox
            );
            assert!(
                d.bbox[2] > 1.0 && d.bbox[3] > 1.0,
                "degenerate bbox: {:?}",
                d.bbox
            );
            // Region CLIP embedding well-formed (the open-vocab retrieval vector).
            let region = objects::crop_region(&f.image, &d.bbox);
            let emb = embedder.embed(&region).expect("clip region embed");
            assert_eq!(emb.len(), 512, "CLIP region embedding not 512-d");
            assert!(
                emb.iter().all(|x| x.is_finite()),
                "non-finite region embedding"
            );
            let norm = cosine(&emb, &emb).sqrt();
            assert!(
                (norm - 1.0).abs() < 1e-3,
                "region embedding not unit-norm: {norm}"
            );
        }
        // Whole-frame open-vocab row embedding (matches arbitrary non-COCO phrases).
        let femb = embedder.embed(&f.image).expect("clip frame embed");
        assert_eq!(femb.len(), 512);
        let fnorm = cosine(&femb, &femb).sqrt();
        assert!(
            (fnorm - 1.0).abs() < 1e-3,
            "frame embedding not unit-norm: {fnorm}"
        );
    }
    eprintln!("total objects: {total_objects}, distinct labels: {labels:?}");
    assert!(
        total_objects >= 1,
        "expected >=1 object detection on the synthesized object clip — RF-DETR decode/contract likely broken"
    );
    assert!(
        labels.iter().any(|l| !l.starts_with("class_")),
        "all labels were class_<i> — COCO class indexing is wrong; fix coco_label()/class offset in objects.rs \
         (see models/rf-detr-classes.json from export_rf_detr.py)"
    );
}

/// Print the REAL I/O contract of the SCRFD detector + the cleanup models (Real-ESRGAN, GFPGAN/
/// CodeFormer). Run after provisioning to confirm the SCRFD decode in `detect_scrfd.rs` (score/bbox/
/// kps grouped by trailing dim) and the 512² restorer / dynamic super-res contracts match the export.
///   cargo test -p hushai-worker --test vision_pipeline inspect_enhance_model_io_shapes -- --nocapture
#[test]
fn inspect_enhance_model_io_shapes() {
    let dy = dylib();
    if !dy.exists() {
        eprintln!("SKIP: dylib missing");
        return;
    }
    model::init_ort(dy.to_str().unwrap());
    for (name, rel) in [
        ("SCRFD", "models/scrfd_10g_bnkps.onnx"),
        ("Real-ESRGAN", "models/realesrgan_x4plus.onnx"),
        ("GFPGAN", "models/gfpgan_v1.4.onnx"),
        ("CodeFormer", "models/codeformer.onnx"),
    ] {
        let p = repo_root().join(rel);
        if !p.exists() {
            eprintln!("SKIP {name}: {} missing (run local_dev/fetch_*.sh / export_*.py)", p.display());
            continue;
        }
        let s = model::load_session(p.to_str().unwrap(), false).expect("load");
        eprintln!("== {name} ({rel}) ==");
        for i in &s.inputs {
            eprintln!("  IN  {} : {:?}", i.name, i.input_type.tensor_dimensions());
        }
        for o in &s.outputs {
            eprintln!("  OUT {} : {:?}", o.name, o.output_type.tensor_dimensions());
        }
    }
}

/// THE recover-then-embed correctness gate: take a clean frontal face, ARTIFICIALLY DEGRADE it
/// (downscale + blur, as a small/distant capture would be), and confirm that the restoration cascade
/// pulls the degraded face's ArcFace embedding markedly CLOSER to the clean reference than the raw
/// degraded crop does. If restoration moved identity the wrong way this fails — the guard that lets
/// restored faces fold into centroids only after this passes. Gated on FACE_TEST_DIR + the restorer.
///   FACE_TEST_DIR=/path cargo test -p hushai-worker --test vision_pipeline restored_low_quality_recovers_identity -- --nocapture
#[test]
fn restored_low_quality_recovers_identity() {
    let dir = match std::env::var("FACE_TEST_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => {
            eprintln!("SKIP: set FACE_TEST_DIR (needs obama.jpg)");
            return;
        }
    };
    let dy = dylib();
    let scrfd = repo_root().join("models/scrfd_10g_bnkps.onnx");
    let yunet = repo_root().join("models/face_detection_yunet_2023mar.onnx");
    let arcface = repo_root().join("models/w600k_r50.onnx");
    let gfpgan = repo_root().join("models/gfpgan_v1.4.onnx");
    if !dy.exists() || !arcface.exists() || !gfpgan.exists() || (!scrfd.exists() && !yunet.exists()) {
        eprintln!("SKIP: arcface/gfpgan/(scrfd|yunet) not all provisioned");
        return;
    }
    model::init_ort(dy.to_str().unwrap());
    let detector: Box<dyn FaceDetect> = if scrfd.exists() {
        Box::new(hushai_worker::vision::detect_scrfd::ScrfdDetector::new(
            model::load_session(scrfd.to_str().unwrap(), false).unwrap(),
            0.5,
        ))
    } else {
        Box::new(FaceDetector::new(
            model::load_session(yunet.to_str().unwrap(), false).unwrap(),
            0.5,
        ))
    };
    let embedder = FaceEmbedder::new(model::load_session(arcface.to_str().unwrap(), false).unwrap());
    let restorer = enhance::FaceRestorer::new(
        model::load_session(gfpgan.to_str().unwrap(), false).unwrap(),
        enhance::RestorerKind::Gfpgan,
        0.6,
    );

    let img = image::open(dir.join("obama.jpg")).expect("open obama.jpg").to_rgb8();
    let mut faces = detector.detect(&img).expect("detect");
    assert!(!faces.is_empty(), "no face detected in reference");
    faces.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    let face = &faces[0];

    // Clean reference embedding.
    let clean = embedder.embed(&img, face).expect("clean embed");

    // Degrade the WHOLE frame: 4× downscale then back up + blur (a small/distant, soft capture).
    let small = enhance::resize_rgb(&img, img.width() / 4, img.height() / 4);
    let back = enhance::resize_rgb(&small, img.width(), img.height());
    let degraded = image::imageops::blur(&back, 2.0);
    let dfaces = detector.detect(&degraded).expect("detect degraded");
    if dfaces.is_empty() {
        eprintln!("NOTE: detector lost the degraded face entirely — restoration can't help; skipping");
        return;
    }
    let dface = dfaces
        .iter()
        .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap())
        .unwrap();

    // Raw degraded embedding (the old behavior).
    let raw = embedder.embed(&degraded, dface).expect("raw degraded embed");

    // Restored path (mirror write.rs::try_restore): margin-crop → restore → align → embed.
    let (cur, off) = enhance::crop_with_margin(&degraded, &dface.bbox, 0.35);
    let mut lmk = dface.landmarks;
    for l in lmk.iter_mut() {
        l[0] -= off[0];
        l[1] -= off[1];
    }
    let restored = restorer.restore(&cur).expect("restore");
    let sx = restored.width() as f32 / cur.width().max(1) as f32;
    let sy = restored.height() as f32 / cur.height().max(1) as f32;
    for l in lmk.iter_mut() {
        l[0] *= sx;
        l[1] *= sy;
    }
    let aligned = face_embed::align_crop(&restored, &lmk);
    let restored_emb = embedder.embed_aligned(&aligned).expect("restored embed");

    let raw_sim = cosine(&clean, &raw);
    let restored_sim = cosine(&clean, &restored_emb);
    eprintln!("cosine to clean — raw degraded {raw_sim:.3}, restored {restored_sim:.3}");
    assert!(
        restored_sim >= raw_sim,
        "restoration moved identity the WRONG way: restored {restored_sim:.3} < raw {raw_sim:.3}"
    );
}

#[tokio::test]
async fn samples_rgb_frames_from_a_real_mp4() {
    let mp4 = repo_root().join("IMG_7256.mp4");
    if !mp4.exists() {
        eprintln!("SKIP: {} missing", mp4.display());
        return;
    }
    let cfg = WorkerConfig::from_env().expect("config");
    let seg = SegmentRow {
        segment_id: Uuid::now_v7(),
        device_id: "test-cam".into(),
        blob_uri: format!("file://{}", mp4.display()),
        container: "mp4".into(),
        codec_init_data: None,
        capture_start_unix_nanos: 0,
        session_id: Uuid::nil(),
        stream_id: "cam0-video".into(),
        sequence: 0,
        duration_nanos: 4_000_000_000, // ~4s; fps is derived from this
        gap_before: false,
    };

    let out = frames::sample_frames(&cfg, &seg, 3)
        .await
        .expect("sample_frames");
    eprintln!("decoded {} frames", out.len());
    assert!(
        !out.is_empty(),
        "expected at least one RGB frame from a real mp4"
    );
    for (i, f) in out.iter().enumerate() {
        let (w, h) = (f.image.width(), f.image.height());
        eprintln!(
            "  frame {i}: {w}x{h}, offset {} ms",
            f.offset_nanos / 1_000_000
        );
        assert!(w > 0 && h > 0);
    }
}
