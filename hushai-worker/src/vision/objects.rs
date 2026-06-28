//! Open-vocabulary object lane (Phase B): RF-DETR region boxes + OpenCLIP image embeddings.
//!
//! ⚠️ OPERATOR-PROVISIONED + DECODE-VALIDATED-AT-PROVISIONING. Unlike the face models (YuNet/ArcFace,
//! whose exact ONNX I/O is proven in `tests/vision_pipeline.rs`), the RF-DETR and CLIP ONNX exports
//! are NOT committed. This decoder targets the standard DETR-family export contract and is
//! deliberately defensive:
//!   * outputs are read by ORDER (first = boxes `[1,N,4]`, second = class logits `[1,N,C]`) since
//!     export tensor NAMES vary (dets/labels, pred_boxes/pred_logits, boxes/scores).
//!   * boxes are assumed cxcywh; NORMALIZED [0,1] when the max coord looks normalized, else treated
//!     as model-input pixels and rescaled. Logits are sigmoid-activated (RF-DETR), argmax → COCO label.
//! VALIDATE this decode against the real export at provisioning (the tooling now exists — see AGENTS.md
//! "vision"): `local_dev/export_rf_detr.py` + `export_clip.py`, then
//! `cargo test -p hushai-worker --test vision_pipeline inspect_object_model_io_shapes` and
//! `detect_objects_from_real_video`. ⚠️ `coco_label` is dense COCO-80; RF-DETR may use a 90/91-slot
//! layout — if the test shows `class_<i>` labels, fix the class map against `models/rf-detr-classes.json`.
//! Any failure here is NON-FATAL: the object lane self-disables / skips, and the face lane still runs.
//!
//! Coordinate contract (load-bearing for the viewer overlay): boxes are returned in ORIGINAL-frame
//! pixels `[x,y,w,h]`, exactly like `detect.rs` does for faces, so faces and objects share one space.

use anyhow::{Context, Result};
use image::RgbImage;
use ndarray::Array4;
use ort::session::Session;

/// CLIP ViT-B/32 image-tower input side and normalization (the standard OpenAI CLIP preprocessing).
const CLIP_SIZE: usize = 224;
const CLIP_MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const CLIP_STD: [f32; 3] = [0.268_629_54, 0.261_302_58, 0.275_777_11];

/// One detected object in ORIGINAL-frame pixel coordinates.
#[derive(Debug, Clone)]
pub struct DetectedObject {
    /// [x, y, w, h] top-left + size, in original-frame pixels.
    pub bbox: [f32; 4],
    pub label: String,
    pub score: f32,
}

/// RF-DETR object detector. Input side is configurable (RF-DETR-Nano variants differ); ImageNet
/// normalization on RGB, NCHW. See the module header for the defensive decode contract.
pub struct ObjectDetector {
    session: Session,
    input: usize,
    score_threshold: f32,
    /// Keep at most this many region detections per frame (highest score first); 0 = unlimited.
    max_per_frame: usize,
    /// Drop boxes whose smaller side is below this many original-frame pixels.
    min_box_px: f32,
}

impl ObjectDetector {
    pub fn new(
        session: Session,
        input: usize,
        score_threshold: f32,
        max_per_frame: usize,
        min_box_px: f32,
    ) -> Self {
        Self {
            session,
            input: input.max(64),
            score_threshold,
            max_per_frame,
            min_box_px: min_box_px.max(0.0),
        }
    }

    /// Detect objects in an RGB frame. CPU-bound ONNX work — call inside `spawn_blocking`.
    pub fn detect(&self, frame: &RgbImage) -> Result<Vec<DetectedObject>> {
        let (ow, oh) = (frame.width() as f32, frame.height() as f32);
        let n = self.input;
        // Letterbox to NxN preserving aspect (pad bottom/right with 0), like the face detector.
        let scale = (n as f32 / ow).min(n as f32 / oh);
        let (nw, nh) = ((ow * scale).round() as u32, (oh * scale).round() as u32);
        let resized = image::imageops::resize(frame, nw, nh, image::imageops::FilterType::Triangle);

        // NCHW [1,3,N,N], RGB, ImageNet-normalized, zero-padded.
        let mut input = Array4::<f32>::zeros((1, 3, n, n));
        for y in 0..nh as usize {
            for x in 0..nw as usize {
                let px = resized.get_pixel(x as u32, y as u32).0;
                for c in 0..3 {
                    input[[0, c, y, x]] =
                        (px[c] as f32 / 255.0 - IMAGENET_MEAN[c]) / IMAGENET_STD[c];
                }
            }
        }

        let outputs = self
            .session
            .run(ort::inputs![input].context("rf-detr inputs")?)
            .context("rf-detr inference")?;

        // Read outputs by ORDER (names vary across exports).
        let out_names: Vec<String> = self
            .session
            .outputs
            .iter()
            .map(|o| o.name.clone())
            .collect();
        anyhow::ensure!(
            out_names.len() >= 2,
            "rf-detr produced {} outputs, expected >=2 (boxes, logits)",
            out_names.len()
        );
        let boxes = outputs[out_names[0].as_str()]
            .try_extract_tensor::<f32>()
            .context("extracting rf-detr boxes")?;
        let logits = outputs[out_names[1].as_str()]
            .try_extract_tensor::<f32>()
            .context("extracting rf-detr logits")?;

        let bshape = boxes.shape().to_vec();
        let lshape = logits.shape().to_vec();
        anyhow::ensure!(
            bshape.len() == 3 && bshape[2] == 4,
            "rf-detr boxes shape {bshape:?} not [1,N,4]"
        );
        anyhow::ensure!(
            lshape.len() == 3,
            "rf-detr logits shape {lshape:?} not [1,N,C]"
        );
        let nq = bshape[1].min(lshape[1]);
        let ncls = lshape[2];

        // Flatten to slices for index math (ArrayViewD indexing by [[..]] also works but this is cheap).
        let bx: Vec<f32> = boxes.iter().copied().collect();
        let lg: Vec<f32> = logits.iter().copied().collect();

        // Heuristic: are boxes normalized [0,1] or in model-input pixels?
        let bmax = bx.iter().cloned().fold(0.0f32, f32::max);
        let normalized = bmax <= 1.5;

        let mut out: Vec<DetectedObject> = Vec::new();
        for q in 0..nq {
            // best class for this query (sigmoid logits; RF-DETR uses focal/sigmoid heads)
            let mut best_c = 0usize;
            let mut best_s = 0.0f32;
            for c in 0..ncls {
                let s = sigmoid(lg[q * ncls + c]);
                if s > best_s {
                    best_s = s;
                    best_c = c;
                }
            }
            if best_s < self.score_threshold {
                continue;
            }
            let b = &bx[q * 4..q * 4 + 4]; // cxcywh
            let (cx, cy, bw, bh) = if normalized {
                (
                    b[0] * n as f32,
                    b[1] * n as f32,
                    b[2] * n as f32,
                    b[3] * n as f32,
                )
            } else {
                (b[0], b[1], b[2], b[3])
            };
            // model-input (letterboxed) coords -> original-frame pixels (divide by the letterbox scale)
            let x = (cx - bw / 2.0) / scale;
            let y = (cy - bh / 2.0) / scale;
            let w = bw / scale;
            let h = bh / scale;
            // Drop degenerate + sub-`min_box_px` boxes (tiny/garbage detections not worth a row).
            if w <= 1.0 || h <= 1.0 || w.min(h) < self.min_box_px {
                continue;
            }
            out.push(DetectedObject {
                bbox: [x.max(0.0), y.max(0.0), w, h],
                label: coco_label(best_c),
                score: best_s,
            });
        }
        // Keep the highest-confidence detections first, then cap per frame to bound scene_objects.
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if self.max_per_frame > 0 && out.len() > self.max_per_frame {
            out.truncate(self.max_per_frame);
        }
        Ok(out)
    }
}

/// ImageNet normalization for RF-DETR (RGB, 0-1).
const IMAGENET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const IMAGENET_STD: [f32; 3] = [0.229, 0.224, 0.225];

/// OpenCLIP ViT-B/32 image tower → 512-d L2-normalized embedding (the open-vocab retrieval vector).
pub struct ClipEmbedder {
    session: Session,
}

impl ClipEmbedder {
    pub fn new(session: Session) -> Self {
        Self { session }
    }

    /// Embed an RGB image (a region crop or the whole frame) into a 512-d L2-normalized vector.
    /// CPU-bound; call inside `spawn_blocking`.
    pub fn embed(&self, img: &RgbImage) -> Result<Vec<f32>> {
        let resized = image::imageops::resize(
            img,
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
        let outputs = self
            .session
            .run(ort::inputs![input].context("clip inputs")?)
            .context("clip inference")?;
        let out_name = self
            .session
            .outputs
            .first()
            .map(|o| o.name.clone())
            .context("clip has no outputs")?;
        let t = outputs[out_name.as_str()]
            .try_extract_tensor::<f32>()
            .context("extracting clip embedding")?;
        let mut v: Vec<f32> = t.iter().copied().collect();
        anyhow::ensure!(!v.is_empty(), "clip returned an empty embedding");
        crate::vad::l2_normalize(&mut v);
        Ok(v)
    }
}

/// Crop an [x,y,w,h] pixel region from a frame (clamped to bounds). Returns at least a 1x1 image.
pub fn crop_region(frame: &RgbImage, bbox: &[f32; 4]) -> RgbImage {
    let (fw, fh) = (frame.width(), frame.height());
    let x = (bbox[0].max(0.0) as u32).min(fw.saturating_sub(1));
    let y = (bbox[1].max(0.0) as u32).min(fh.saturating_sub(1));
    let w = (bbox[2].max(1.0) as u32).min(fw - x).max(1);
    let h = (bbox[3].max(1.0) as u32).min(fh - y).max(1);
    image::imageops::crop_imm(frame, x, y, w, h).to_image()
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// COCO-80 class names (the order RF-DETR/Ultralytics export). Out-of-range → `class_<i>`.
fn coco_label(i: usize) -> String {
    const COCO: [&str; 80] = [
        "person",
        "bicycle",
        "car",
        "motorcycle",
        "airplane",
        "bus",
        "train",
        "truck",
        "boat",
        "traffic light",
        "fire hydrant",
        "stop sign",
        "parking meter",
        "bench",
        "bird",
        "cat",
        "dog",
        "horse",
        "sheep",
        "cow",
        "elephant",
        "bear",
        "zebra",
        "giraffe",
        "backpack",
        "umbrella",
        "handbag",
        "tie",
        "suitcase",
        "frisbee",
        "skis",
        "snowboard",
        "sports ball",
        "kite",
        "baseball bat",
        "baseball glove",
        "skateboard",
        "surfboard",
        "tennis racket",
        "bottle",
        "wine glass",
        "cup",
        "fork",
        "knife",
        "spoon",
        "bowl",
        "banana",
        "apple",
        "sandwich",
        "orange",
        "broccoli",
        "carrot",
        "hot dog",
        "pizza",
        "donut",
        "cake",
        "chair",
        "couch",
        "potted plant",
        "bed",
        "dining table",
        "toilet",
        "tv",
        "laptop",
        "mouse",
        "remote",
        "keyboard",
        "cell phone",
        "microwave",
        "oven",
        "toaster",
        "sink",
        "refrigerator",
        "book",
        "clock",
        "vase",
        "scissors",
        "teddy bear",
        "hair drier",
        "toothbrush",
    ];
    COCO.get(i)
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("class_{i}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coco_labels_and_fallback() {
        assert_eq!(coco_label(0), "person");
        assert_eq!(coco_label(63), "laptop");
        assert_eq!(coco_label(56), "chair");
        assert_eq!(coco_label(999), "class_999");
    }

    #[test]
    fn sigmoid_monotone() {
        assert!(sigmoid(-10.0) < 0.01);
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-6);
        assert!(sigmoid(10.0) > 0.99);
    }

    #[test]
    fn crop_clamps_to_bounds() {
        let img = RgbImage::new(20, 20);
        let c = crop_region(&img, &[15.0, 15.0, 100.0, 100.0]); // overruns -> clamped
        assert!(c.width() >= 1 && c.height() >= 1);
        assert!(c.width() <= 20 && c.height() <= 20);
    }
}
