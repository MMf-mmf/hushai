//! Face detection + 5-point landmarks via YuNet (`face_detection_yunet_2023mar.onnx`, OpenCV Zoo,
//! Apache-2.0). Fixed 640×640 NCHW input (BGR, 0-255, no normalization). SSD-style heads at strides
//! 8/16/32 emit per-anchor cls/obj/bbox(4)/kps(10). We letterbox the frame to 640², decode all
//! three strides above a score threshold, NMS, and map boxes + landmarks back to original-frame
//! pixels. See the I/O dump in `tests/vision_pipeline.rs::inspect_face_model_io_shapes`.

use anyhow::{Context, Result};
use image::RgbImage;
use ndarray::Array4;
use ort::session::Session;

const INPUT: usize = 640;
const STRIDES: [usize; 3] = [8, 16, 32];

/// One detected face in ORIGINAL-frame pixel coordinates.
#[derive(Debug, Clone)]
pub struct Face {
    /// [x, y, w, h] top-left + size, in original-frame pixels.
    pub bbox: [f32; 4],
    pub score: f32,
    /// 5 landmarks (x, y) in original-frame pixels. YuNet order:
    /// [right-eye, left-eye, nose, right-mouth-corner, left-mouth-corner] (person's perspective).
    pub landmarks: [[f32; 2]; 5],
}

impl Face {
    /// Shorter bbox side in pixels — the min-face-size quality gate.
    pub fn min_side(&self) -> f32 {
        self.bbox[2].min(self.bbox[3])
    }
}

pub struct FaceDetector {
    session: Session,
    score_threshold: f32,
    nms_iou: f32,
}

impl FaceDetector {
    pub fn new(session: Session, score_threshold: f32) -> Self {
        Self {
            session,
            score_threshold,
            nms_iou: 0.3,
        }
    }

    /// Detect faces in an RGB frame. Pure-CPU-bound; call inside `spawn_blocking`.
    pub fn detect(&self, frame: &RgbImage) -> Result<Vec<Face>> {
        let (ow, oh) = (frame.width() as f32, frame.height() as f32);
        // Letterbox: scale to fit 640² preserving aspect, pad the remainder with 0.
        let scale = (INPUT as f32 / ow).min(INPUT as f32 / oh);
        let (nw, nh) = ((ow * scale).round() as u32, (oh * scale).round() as u32);
        let resized = image::imageops::resize(frame, nw, nh, image::imageops::FilterType::Triangle);

        // NCHW [1,3,640,640], BGR, 0-255, zero-padded. (YuNet trained on BGR; our frame is RGB.)
        let mut input = Array4::<f32>::zeros((1, 3, INPUT, INPUT));
        for y in 0..nh as usize {
            for x in 0..nw as usize {
                let px = resized.get_pixel(x as u32, y as u32).0; // [R,G,B]
                input[[0, 0, y, x]] = px[2] as f32; // B
                input[[0, 1, y, x]] = px[1] as f32; // G
                input[[0, 2, y, x]] = px[0] as f32; // R
            }
        }

        let outputs = self
            .session
            .run(ort::inputs![input].context("yunet inputs")?)
            .context("yunet inference")?;

        let mut cands: Vec<Face> = Vec::new();
        for (si, &stride) in STRIDES.iter().enumerate() {
            let cls = extract(&outputs, &format!("cls_{stride}"))?;
            let obj = extract(&outputs, &format!("obj_{stride}"))?;
            let bbox = extract(&outputs, &format!("bbox_{stride}"))?;
            let kps = extract(&outputs, &format!("kps_{stride}"))?;
            let cols = INPUT / stride; // grid width = height
            let n = cols * cols;
            debug_assert_eq!(cls.len(), n, "stride {stride} anchor count");
            let _ = si;

            for idx in 0..n {
                let score = clamp01(cls[idx]) * clamp01(obj[idx]);
                if score < self.score_threshold {
                    continue;
                }
                let row = idx / cols;
                let col = idx % cols;
                let b = &bbox[idx * 4..idx * 4 + 4];
                let cx = (col as f32 + b[0]) * stride as f32;
                let cy = (row as f32 + b[1]) * stride as f32;
                let w = b[2].exp() * stride as f32;
                let h = b[3].exp() * stride as f32;
                // letterbox -> original frame coords (no pad offset: we padded bottom/right with 0,0)
                let to_orig = |vx: f32, vy: f32| [vx / scale, vy / scale];
                let tl = to_orig(cx - w / 2.0, cy - h / 2.0);
                let sz = [w / scale, h / scale];
                let k = &kps[idx * 10..idx * 10 + 10];
                let mut landmarks = [[0.0f32; 2]; 5];
                for j in 0..5 {
                    let lx = (col as f32 + k[2 * j]) * stride as f32;
                    let ly = (row as f32 + k[2 * j + 1]) * stride as f32;
                    landmarks[j] = to_orig(lx, ly);
                }
                cands.push(Face {
                    bbox: [tl[0], tl[1], sz[0], sz[1]],
                    score,
                    landmarks,
                });
            }
        }

        Ok(nms(cands, self.nms_iou))
    }
}

fn extract<'a>(outputs: &'a ort::session::SessionOutputs, name: &str) -> Result<Vec<f32>> {
    let t = outputs[name]
        .try_extract_tensor::<f32>()
        .with_context(|| format!("extracting {name}"))?;
    Ok(t.iter().copied().collect())
}

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// Greedy non-max suppression by descending score.
fn nms(mut faces: Vec<Face>, iou_thresh: f32) -> Vec<Face> {
    faces.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut keep: Vec<Face> = Vec::new();
    for f in faces {
        if keep.iter().all(|k| iou(&k.bbox, &f.bbox) <= iou_thresh) {
            keep.push(f);
        }
    }
    keep
}

/// IoU of two [x, y, w, h] boxes.
fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let (ax2, ay2) = (a[0] + a[2], a[1] + a[3]);
    let (bx2, by2) = (b[0] + b[2], b[1] + b[3]);
    let ix1 = a[0].max(b[0]);
    let iy1 = a[1].max(b[1]);
    let ix2 = ax2.min(bx2);
    let iy2 = ay2.min(by2);
    let iw = (ix2 - ix1).max(0.0);
    let ih = (iy2 - iy1).max(0.0);
    let inter = iw * ih;
    let union = a[2] * a[3] + b[2] * b[3] - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iou_basics() {
        // identical boxes
        assert!((iou(&[0.0, 0.0, 10.0, 10.0], &[0.0, 0.0, 10.0, 10.0]) - 1.0).abs() < 1e-6);
        // disjoint
        assert_eq!(iou(&[0.0, 0.0, 10.0, 10.0], &[20.0, 20.0, 10.0, 10.0]), 0.0);
        // half overlap: 5x10 inter / (100+100-50) = 50/150
        assert!(
            (iou(&[0.0, 0.0, 10.0, 10.0], &[5.0, 0.0, 10.0, 10.0]) - (50.0 / 150.0)).abs() < 1e-6
        );
    }

    #[test]
    fn nms_suppresses_overlap_keeps_distinct() {
        let f = |x: f32, score: f32| Face {
            bbox: [x, 0.0, 10.0, 10.0],
            score,
            landmarks: [[0.0; 2]; 5],
        };
        // two heavily overlapping (keep the higher score) + one far away (keep)
        let out = nms(vec![f(0.0, 0.9), f(1.0, 0.8), f(50.0, 0.7)], 0.3);
        assert_eq!(out.len(), 2);
        assert!((out[0].score - 0.9).abs() < 1e-6);
    }
}
