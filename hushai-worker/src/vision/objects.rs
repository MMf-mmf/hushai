//! Open-vocabulary object lane (Phase B): RF-DETR region boxes + OpenCLIP image embeddings.
//!
//! OPERATOR-PROVISIONED (RF-DETR + CLIP ONNX are gitignored). DECODE VALIDATED against the real
//! export (2026-06-30): RF-DETR-Nano emits `dets[1,300,4]` (boxes) + `labels[1,300,91]` (class logits).
//!   * outputs are read by ORDER (first = boxes `[1,N,4]`, second = class logits `[1,N,C]`) since
//!     export tensor NAMES vary (dets/labels, pred_boxes/pred_logits, boxes/scores).
//!   * boxes are assumed cxcywh; NORMALIZED [0,1] when the max coord looks normalized, else treated
//!     as model-input pixels and rescaled.
//!   * logits are sigmoid-activated (RF-DETR focal head). **The class-logit COLUMN INDEX is the COCO
//!     category id** (the 91-slot layout: col 1=person, 2=bicycle, 82=refrigerator; col 0 + the gaps
//!     are background). We arg-max over NAMED columns only and map via [`coco91_class_names`] (or the
//!     authoritative `models/rf-detr-classes.json` via [`ObjectDetector::with_class_names`]). A prior
//!     dense-COCO-80 map mislabeled every detection (person→"bicycle"); see `coco91_column_map_is_correct`.
//! Re-validate after a re-export: `cargo test -p hushai-worker --test vision_pipeline
//! inspect_object_model_io_shapes` + `detect_objects_from_real_video`.
//! Any failure here is NON-FATAL: the object lane self-disables / skips, and the face lane still runs.
//!
//! Coordinate contract (load-bearing for the viewer overlay): boxes are returned in ORIGINAL-frame
//! pixels `[x,y,w,h]`, exactly like `detect.rs` does for faces, so faces and objects share one space.

use anyhow::{Context, Result};
use image::RgbImage;
use ndarray::Array4;
use ort::session::Session;
use std::path::Path;

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
    /// Class-aware NMS IoU threshold. RF-DETR's 300-query focal head emits several near-duplicate
    /// boxes per real object (no Hungarian suppression at inference); like every other vision
    /// detector lane (YuNet/SCRFD/plates) we greedily suppress heavy SAME-LABEL overlap. Tuned via
    /// `OBJECT_NMS_IOU`; class-aware so an overlapping person+bicycle both survive.
    nms_iou: f32,
    /// Class-logit COLUMN INDEX → label. Length == the model's C. `None` slots are the COCO
    /// background/unused columns (id 0 + the historical gaps) and are never emitted. Defaults to the
    /// canonical COCO-91 layout; override with the authoritative `models/rf-detr-classes.json` via
    /// [`with_class_names`]. (See the module header — this map IS the load-bearing decode point.)
    class_names: Vec<Option<String>>,
}

/// Default class-aware NMS IoU for the object lane (conservative — only heavy overlap is merged).
pub const DEFAULT_OBJECT_NMS_IOU: f32 = 0.5;

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
            nms_iou: DEFAULT_OBJECT_NMS_IOU,
            class_names: coco91_class_names(),
        }
    }

    /// Override the class-aware NMS IoU threshold (`OBJECT_NMS_IOU`). Values outside (0,1] disable NMS.
    pub fn with_nms_iou(mut self, iou: f32) -> Self {
        self.nms_iou = iou;
        self
    }

    /// Override the column→label map with the authoritative one the exporter wrote
    /// (`models/rf-detr-classes.json`). Keeps the built-in COCO-91 map if `names` is empty.
    pub fn with_class_names(mut self, names: Vec<Option<String>>) -> Self {
        if names.iter().any(|n| n.is_some()) {
            self.class_names = names;
        }
        self
    }

    /// Number of named (emittable) classes — for startup logging / sanity.
    pub fn named_class_count(&self) -> usize {
        self.class_names.iter().filter(|n| n.is_some()).count()
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
            // Best class for this query (sigmoid logits; RF-DETR uses focal/sigmoid heads). The
            // column index IS the COCO category id, so we only consider columns that map to a real
            // class — skipping the background (id 0) and the historical gap columns rather than
            // arg-maxing over them and emitting a `class_<i>` / mislabeled detection.
            let mut best_c = usize::MAX;
            let mut best_s = 0.0f32;
            for c in 0..ncls {
                if c >= self.class_names.len() || self.class_names[c].is_none() {
                    continue;
                }
                let s = sigmoid(lg[q * ncls + c]);
                if s > best_s {
                    best_s = s;
                    best_c = c;
                }
            }
            if best_c == usize::MAX || best_s < self.score_threshold {
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
                // Some by construction: the argmax loop only considers named columns.
                label: self.class_names[best_c].clone().unwrap_or_else(|| format!("class_{best_c}")),
                score: best_s,
            });
        }
        // Class-aware NMS: RF-DETR's 300-query head emits several near-duplicate boxes per object;
        // suppress heavy SAME-LABEL overlap (so an overlapping person+bicycle both survive) before
        // we embed/persist. Skipped when nms_iou is out of (0,1]. Determinism: group order is sorted
        // by label (BTreeMap) and the final pass re-sorts by score, so output is stable under the
        // CPU EP. Mirrors detect.rs / detect_scrfd.rs / plates/detect.rs which all end with nms_by.
        if self.nms_iou > 0.0 && self.nms_iou <= 1.0 && out.len() > 1 {
            let mut by_label: std::collections::BTreeMap<String, Vec<DetectedObject>> =
                std::collections::BTreeMap::new();
            for d in out.drain(..) {
                by_label.entry(d.label.clone()).or_default().push(d);
            }
            for (_label, group) in by_label {
                out.extend(crate::vision::geom::nms_by(
                    group,
                    self.nms_iou,
                    |d| d.bbox,
                    |d| d.score,
                ));
            }
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

/// The canonical COCO "91-slot" column→label map RF-DETR/DETR emit: the class-logit COLUMN INDEX is
/// the COCO category id (1..=90, with the historical gaps at 12/26/29/30/45/66/68/69/71/83; column 0
/// is background). Returns a vec indexed by column; `None` = background/gap (never emitted). This is
/// byte-for-byte the map `local_dev/export_rf_detr.py` writes to `models/rf-detr-classes.json`.
fn coco91_class_names() -> Vec<Option<String>> {
    const COCO91: [(usize, &str); 80] = [
        (1, "person"), (2, "bicycle"), (3, "car"), (4, "motorcycle"), (5, "airplane"), (6, "bus"),
        (7, "train"), (8, "truck"), (9, "boat"), (10, "traffic light"), (11, "fire hydrant"),
        (13, "stop sign"), (14, "parking meter"), (15, "bench"), (16, "bird"), (17, "cat"),
        (18, "dog"), (19, "horse"), (20, "sheep"), (21, "cow"), (22, "elephant"), (23, "bear"),
        (24, "zebra"), (25, "giraffe"), (27, "backpack"), (28, "umbrella"), (31, "handbag"),
        (32, "tie"), (33, "suitcase"), (34, "frisbee"), (35, "skis"), (36, "snowboard"),
        (37, "sports ball"), (38, "kite"), (39, "baseball bat"), (40, "baseball glove"),
        (41, "skateboard"), (42, "surfboard"), (43, "tennis racket"), (44, "bottle"),
        (46, "wine glass"), (47, "cup"), (48, "fork"), (49, "knife"), (50, "spoon"), (51, "bowl"),
        (52, "banana"), (53, "apple"), (54, "sandwich"), (55, "orange"), (56, "broccoli"),
        (57, "carrot"), (58, "hot dog"), (59, "pizza"), (60, "donut"), (61, "cake"), (62, "chair"),
        (63, "couch"), (64, "potted plant"), (65, "bed"), (67, "dining table"), (70, "toilet"),
        (72, "tv"), (73, "laptop"), (74, "mouse"), (75, "remote"), (76, "keyboard"),
        (77, "cell phone"), (78, "microwave"), (79, "oven"), (80, "toaster"), (81, "sink"),
        (82, "refrigerator"), (84, "book"), (85, "clock"), (86, "vase"), (87, "scissors"),
        (88, "teddy bear"), (89, "hair drier"), (90, "toothbrush"),
    ];
    let mut v = vec![None; 91];
    for (id, name) in COCO91 {
        v[id] = Some(name.to_string());
    }
    v
}

/// Load `{ "index": "name" }` (e.g. `models/rf-detr-classes.json`, written by export_rf_detr.py)
/// into a column-indexed vec where the COCO category id keys the slot. Missing/gap ids stay `None`.
pub fn load_class_map(path: &Path) -> Result<Vec<Option<String>>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading class map {}", path.display()))?;
    let map: std::collections::BTreeMap<String, String> = serde_json::from_str(&raw)
        .with_context(|| format!("parsing class map {} as {{\"id\":\"name\"}}", path.display()))?;
    let max_id = map.keys().filter_map(|k| k.parse::<usize>().ok()).max().unwrap_or(0);
    let mut v = vec![None; max_id + 1];
    for (k, name) in map {
        if let Ok(id) = k.parse::<usize>() {
            v[id] = Some(name);
        }
    }
    anyhow::ensure!(
        v.iter().any(|x| x.is_some()),
        "class map {} contained no numeric-id entries",
        path.display()
    );
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coco91_column_map_is_correct() {
        // Column index == COCO category id (NOT dense COCO-80). This is the bug that made a person
        // (column 1) decode as "bicycle": dense-80[1] == bicycle, but coco-91[1] == person.
        let m = coco91_class_names();
        assert_eq!(m.len(), 91);
        assert_eq!(m[1].as_deref(), Some("person"));
        assert_eq!(m[2].as_deref(), Some("bicycle"));
        assert_eq!(m[3].as_deref(), Some("car"));
        assert_eq!(m[37].as_deref(), Some("sports ball"));
        assert_eq!(m[62].as_deref(), Some("chair"));
        assert_eq!(m[73].as_deref(), Some("laptop"));
        assert_eq!(m[82].as_deref(), Some("refrigerator"));
        assert_eq!(m[90].as_deref(), Some("toothbrush"));
        // Background + historical gap columns are None (never emitted).
        assert_eq!(m[0], None);
        assert_eq!(m[12], None);
        assert_eq!(m[26], None);
        assert_eq!(m[83], None);
        assert_eq!(m.iter().filter(|x| x.is_some()).count(), 80);
    }

    #[test]
    fn load_class_map_parses_id_keyed_json() {
        let dir = std::env::temp_dir().join(format!("hushai_classmap_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("classes.json");
        std::fs::write(&p, r#"{"1":"person","2":"bicycle","82":"refrigerator"}"#).unwrap();
        let m = load_class_map(&p).unwrap();
        assert_eq!(m[1].as_deref(), Some("person"));
        assert_eq!(m[82].as_deref(), Some("refrigerator"));
        assert_eq!(m[3], None);
        let _ = std::fs::remove_file(&p);
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
