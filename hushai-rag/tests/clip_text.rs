//! CLIP TEXT tower tests for open-vocabulary object retrieval (Phase B query side).
//!
//! Two kinds, all SKIP-gated so the suite stays green without provisioned models / DB:
//!   * model-gated: the DECISIVE cross-modal proof that the exported text + image towers share ONE
//!     512-d space AND that the Rust tokenizer matches — cosine(car image, "a car") must beat
//!     cosine(car image, "a dog"). This is the objects analogue of the worker's
//!     `face_identity_separation_on_fixtures`. If this fails, "when did I see a car" returns junk.
//!   * DB-gated (`DATABASE_URL`): `retrieve::nearest_objects` orders scene_objects by cosine
//!     distance and includes the whole-frame `__frame__` rows (the SQL plumbing, no model needed).
//!
//! Provision: local_dev/export_clip.py + fetch_clip_tokenizer.sh + fetch_onnxruntime.sh, then
//!   CLIP_TEST_IMAGE=/path/to/car.jpg cargo test -p hushai-rag --test clip_text -- --nocapture

use std::path::PathBuf;

use ndarray::Array4;
use ort::execution_providers::CPUExecutionProvider;
use ort::session::Session;
use pgvector::Vector;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use hushai_rag::clip_text::{self, ClipTextEmbedder};
use hushai_rag::retrieve::{self, Filters, Tuning};

// OpenAI CLIP ViT-B/32 preprocessing — MUST match hushai-worker/src/vision/objects.rs.
const CLIP_SIZE: usize = 224;
const CLIP_MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const CLIP_STD: [f32; 3] = [0.268_629_54, 0.261_302_58, 0.275_777_11];

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

fn model(name: &str) -> PathBuf {
    repo_root().join("models").join(name)
}

fn cos(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>()
}

fn l2_normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

/// Embed an image with the CLIP IMAGE tower (independent re-implementation of the worker's
/// `ClipEmbedder` so the cross-modal test validates the exact preprocessing too).
fn embed_image(image_model: &str, dylib_path: &str, image_path: &str) -> Vec<f32> {
    clip_text::init_ort(dylib_path);
    let session = Session::builder()
        .unwrap()
        .with_execution_providers([CPUExecutionProvider::default().build()])
        .unwrap()
        .commit_from_file(image_model)
        .unwrap();
    let img = image::open(image_path).unwrap().to_rgb8();
    let resized = image::imageops::resize(
        &img,
        CLIP_SIZE as u32,
        CLIP_SIZE as u32,
        image::imageops::FilterType::Triangle,
    );
    let mut input = Array4::<f32>::zeros((1, 3, CLIP_SIZE, CLIP_SIZE));
    for y in 0..CLIP_SIZE {
        for x in 0..CLIP_SIZE {
            let px = resized.get_pixel(x as u32, y as u32).0;
            for c in 0..3 {
                input[[0, c, y, x]] = (px[c] as f32 / 255.0 - CLIP_MEAN[c]) / CLIP_STD[c];
            }
        }
    }
    let outputs = session.run(ort::inputs![input].unwrap()).unwrap();
    let name = session.outputs.first().unwrap().name.clone();
    let t = outputs[name.as_str()].try_extract_tensor::<f32>().unwrap();
    let mut v: Vec<f32> = t.iter().copied().collect();
    l2_normalize(&mut v);
    v
}

#[test]
fn embed_text_is_unit_norm() {
    let (dy, txt_model, tok) = (
        dylib(),
        model("clip_vit_b32_text.onnx"),
        model("clip_tokenizer.json"),
    );
    if !dy.exists() || !txt_model.exists() || !tok.exists() {
        eprintln!("SKIP: CLIP text model/tokenizer/dylib not provisioned");
        return;
    }
    let emb = ClipTextEmbedder::new(
        txt_model.to_str().unwrap(),
        tok.to_str().unwrap(),
        dy.to_str().unwrap(),
    )
    .expect("build clip text embedder")
    .embed_text("a photo of a car")
    .expect("embed");
    assert_eq!(emb.len(), 512, "CLIP text embedding must be 512-d");
    assert!(emb.iter().all(|x| x.is_finite()));
    let norm = cos(&emb, &emb).sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-3,
        "text embedding not unit-norm: {norm}"
    );
}

/// DECISIVE: the text and image towers share one space and the tokenizer matches.
#[test]
fn clip_text_matches_image_cross_modal() {
    let (dy, txt_model, img_model, tok) = (
        dylib(),
        model("clip_vit_b32_text.onnx"),
        model("clip_vit_b32_image.onnx"),
        model("clip_tokenizer.json"),
    );
    let car = match std::env::var("CLIP_TEST_IMAGE") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: set CLIP_TEST_IMAGE to a .jpg of a car");
            return;
        }
    };
    if !dy.exists() || !txt_model.exists() || !img_model.exists() || !tok.exists() || !car.exists()
    {
        eprintln!("SKIP: CLIP models/tokenizer/image not all present");
        return;
    }
    let txt = ClipTextEmbedder::new(
        txt_model.to_str().unwrap(),
        tok.to_str().unwrap(),
        dy.to_str().unwrap(),
    )
    .expect("build clip text embedder");
    let car_img = embed_image(
        img_model.to_str().unwrap(),
        dy.to_str().unwrap(),
        car.to_str().unwrap(),
    );
    let t_car = txt.embed_text("a photo of a car").expect("embed car text");
    let t_dog = txt.embed_text("a photo of a dog").expect("embed dog text");

    let s_car = cos(&car_img, &t_car);
    let s_dog = cos(&car_img, &t_dog);
    eprintln!(
        "cosine(car image, 'a photo of a car')={s_car:.3}  cosine(car image, 'a photo of a dog')={s_dog:.3}"
    );
    assert!(
        s_car > s_dog + 0.02,
        "CLIP image/text spaces misaligned (or tokenizer wrong): car {s_car:.3} not above dog {s_dog:.3}"
    );
}

// ---- DB-gated: nearest_objects SQL plumbing (no model needed) ----

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .ok()
}

async fn insert_fixture_segment(pool: &PgPool, device_id: &str) -> Uuid {
    let session_id = Uuid::now_v7();
    let segment_id = Uuid::now_v7();
    sqlx::query("INSERT INTO devices (device_id, source_kind) VALUES ($1,'test') ON CONFLICT (device_id) DO NOTHING")
        .bind(device_id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO sessions (session_id, device_id) VALUES ($1,$2)")
        .bind(session_id)
        .bind(device_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO streams (session_id, stream_id, device_id, media_type, codec, container) VALUES ($1,'s0',$2,3,'h264+aac','fmp4')")
        .bind(session_id).bind(device_id).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO segments (segment_id, device_id, stream_id, session_id, sequence, media_type, codec, container, capture_start_unix_nanos, monotonic_start_nanos, duration_nanos, content_sha256, byte_len, blob_uri, storage_backend) \
         VALUES ($1,$2,'s0',$3,0,3,'h264+aac','fmp4',1,0,2000000000,$4,100,$5,'file')",
    )
    .bind(segment_id).bind(device_id).bind(session_id).bind(vec![0u8;32]).bind(format!("file:///nonexistent/{segment_id}"))
    .execute(pool).await.unwrap();
    segment_id
}

async fn insert_scene_object(
    pool: &PgPool,
    segment_id: Uuid,
    device_id: &str,
    label: &str,
    start: i64,
    emb: Vec<f32>,
) {
    sqlx::query(
        "INSERT INTO scene_objects (segment_id, device_id, object_label, bbox, det_score, frame_offset_nanos, start_unix_nanos, end_unix_nanos, embedding, embedding_model, embedding_dim) \
         VALUES ($1,$2,$3,NULL,NULL,0,$4,$4,$5,'test',512)",
    )
    .bind(segment_id).bind(device_id).bind(label).bind(start).bind(Vector::from(emb))
    .execute(pool).await.unwrap();
}

async fn cleanup(pool: &PgPool, device_id: &str) {
    for sql in [
        "DELETE FROM scene_objects WHERE segment_id IN (SELECT segment_id FROM segments WHERE device_id=$1)",
        "DELETE FROM segment_vision_status WHERE segment_id IN (SELECT segment_id FROM segments WHERE device_id=$1)",
        "DELETE FROM segments WHERE device_id=$1",
        "DELETE FROM streams WHERE device_id=$1",
        "DELETE FROM sessions WHERE device_id=$1",
        "DELETE FROM devices WHERE device_id=$1",
    ] {
        let _ = sqlx::query(sql).bind(device_id).execute(pool).await;
    }
}

#[tokio::test]
async fn nearest_objects_orders_by_distance_and_includes_frame_rows() {
    let Some(pool) = pool().await else {
        eprintln!("skipping nearest_objects_orders_by_distance: DATABASE_URL unset");
        return;
    };
    let device = format!("test-obj-{}", Uuid::now_v7());
    // Two segments so the per-segment dedup keeps one row each (a 'car' region + a '__frame__' row).
    let seg_a = insert_fixture_segment(&pool, &device).await;
    let seg_b = insert_fixture_segment(&pool, &device).await;

    // 512-d synthetic embeddings: A == query, frame partially aligned, B orthogonal.
    let mut a = vec![0f32; 512];
    a[0] = 1.0;
    let mut frame = vec![0f32; 512];
    frame[0] = 0.6;
    frame[1] = 0.8;
    let mut b = vec![0f32; 512];
    b[1] = 1.0;

    insert_scene_object(&pool, seg_a, &device, "car", 1_000, a.clone()).await;
    insert_scene_object(&pool, seg_a, &device, "__frame__", 1_000, frame).await;
    insert_scene_object(&pool, seg_b, &device, "dog", 2_000, b).await;

    let filters = Filters {
        device_id: Some(device.clone()),
        ..Default::default()
    };
    let results = retrieve::nearest_objects(&pool, &a, 10, &Tuning::default(), &filters, true)
        .await
        .unwrap();

    // Closest first; the exact 'car' row wins, the orthogonal 'dog' is last. (One row per segment.)
    assert!(!results.is_empty(), "expected object NN results");
    assert_eq!(results[0].text, "car", "exact match should rank first");
    assert!(results[0].distance < 1e-4, "exact match distance ~0");
    for w in results.windows(2) {
        assert!(
            w[0].distance <= w[1].distance,
            "results must be distance-ordered"
        );
    }

    // regions_only excludes the whole-frame rows.
    let regions = retrieve::nearest_objects(&pool, &a, 10, &Tuning::default(), &filters, false)
        .await
        .unwrap();
    assert!(
        regions.iter().all(|s| s.text != "__frame__"),
        "regions_only must drop __frame__ rows"
    );

    cleanup(&pool, &device).await;
}
