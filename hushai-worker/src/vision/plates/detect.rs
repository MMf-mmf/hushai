//! License-plate detection inside a vehicle ROI. Runs a YOLO-family plate detector (bbox, optionally
//! 4 corner keypoints for perspective rectification) on the (upscaled) vehicle crop, then the caller
//! maps coordinates back to original-frame pixels.
//!
//! ⚠️ DECODE-VALIDATED-AT-PROVISIONING (like `objects.rs`): plate-detector exports vary, so this is a
//! defensive YOLOv8/YOLO11-style decoder. It takes the largest 2-D output `[C, N]` (or `[N, C]`),
//! treats the first 4 channels as `cx,cy,w,h` in model-input pixels, channel 4 as confidence, and —
//! if there are ≥8 trailing channels — the next 4 `(x,y)[,vis]` groups as plate corners. Validate
//! against the real export with `tests/vision_pipeline.rs::inspect_plate_model_io_shapes`.

use anyhow::{Context, Result};
use image::RgbImage;
use ndarray::Array4;
use ort::session::Session;

use crate::vision::geom;

/// One detected plate, in the coordinate space of the IMAGE PASSED TO `detect` (the vehicle crop).
#[derive(Debug, Clone)]
pub struct PlateBox {
    pub bbox: [f32; 4],
    pub score: f32,
    /// 4 corners (TL,TR,BR,BL) when the detector is a keypoint/pose model; enables rectification.
    pub corners: Option<[[f32; 2]; 4]>,
}

pub struct PlateDetector {
    session: Session,
    input: usize,
    score_threshold: f32,
    nms_iou: f32,
}

impl PlateDetector {
    pub fn new(session: Session, input: usize, score_threshold: f32) -> Self {
        Self {
            session,
            input: input.max(64),
            score_threshold,
            nms_iou: 0.45,
        }
    }

    /// Detect plates in an RGB crop. CPU-bound ONNX work — call inside `spawn_blocking`.
    pub fn detect(&self, crop: &RgbImage) -> Result<Vec<PlateBox>> {
        let (ow, oh) = (crop.width() as f32, crop.height() as f32);
        let n = self.input;
        let scale = (n as f32 / ow).min(n as f32 / oh);
        let (nw, nh) = ((ow * scale).round() as u32, (oh * scale).round() as u32);
        let resized = image::imageops::resize(crop, nw, nh, image::imageops::FilterType::Triangle);

        // NCHW [1,3,N,N], RGB, 0-1, zero-padded (Ultralytics default preprocessing).
        let mut input = Array4::<f32>::zeros((1, 3, n, n));
        for y in 0..nh as usize {
            for x in 0..nw as usize {
                let px = resized.get_pixel(x as u32, y as u32).0;
                for c in 0..3 {
                    input[[0, c, y, x]] = px[c] as f32 / 255.0;
                }
            }
        }

        let outputs = self
            .session
            .run(ort::inputs![input].context("plate-detect inputs")?)
            .context("plate-detect inference")?;

        // Take the largest 2-D output (ignoring the batch dim).
        let out_name = self
            .session
            .outputs
            .first()
            .map(|o| o.name.clone())
            .context("plate detector has no outputs")?;
        let t = outputs[out_name.as_str()]
            .try_extract_tensor::<f32>()
            .context("extracting plate-detect output")?;
        let shape: Vec<usize> = t.shape().iter().copied().filter(|&d| d != 1).collect();
        anyhow::ensure!(
            shape.len() == 2,
            "plate detector output (squeezed) {shape:?} not 2-D [C,N]/[N,C]"
        );
        let data: Vec<f32> = t.iter().copied().collect();
        let (a, b) = (shape[0], shape[1]);
        // Channels are the SMALLER dim (N boxes ≫ C channels for YOLO, e.g. 8400 ≫ 5..17).
        let (channels, nboxes, channels_first) = if a <= b { (a, b, true) } else { (b, a, false) };
        anyhow::ensure!(channels >= 5, "plate detector has {channels} channels, expected ≥5");
        let at = |c: usize, i: usize| -> f32 {
            if channels_first {
                data[c * nboxes + i]
            } else {
                data[i * channels + c]
            }
        };

        // Corner keypoints present when there are ≥8 trailing channels beyond box(4)+conf(1).
        let kpt_block = channels.saturating_sub(5);
        let (has_corners, kstride) = if kpt_block >= 12 {
            (true, 3) // (x,y,visibility)×4
        } else if kpt_block >= 8 {
            (true, 2) // (x,y)×4
        } else {
            (false, 0)
        };

        let mut cands: Vec<PlateBox> = Vec::new();
        for i in 0..nboxes {
            let conf = at(4, i);
            if conf < self.score_threshold {
                continue;
            }
            let (cx, cy, bw, bh) = (at(0, i), at(1, i), at(2, i), at(3, i));
            let x = (cx - bw / 2.0) / scale;
            let y = (cy - bh / 2.0) / scale;
            let w = bw / scale;
            let h = bh / scale;
            if w <= 1.0 || h <= 1.0 {
                continue;
            }
            let corners = if has_corners {
                let mut c = [[0.0f32; 2]; 4];
                for (k, cc) in c.iter_mut().enumerate() {
                    let base = 5 + k * kstride;
                    cc[0] = at(base, i) / scale;
                    cc[1] = at(base + 1, i) / scale;
                }
                Some(c)
            } else {
                None
            };
            cands.push(PlateBox {
                bbox: [x.max(0.0), y.max(0.0), w, h],
                score: conf,
                corners,
            });
        }
        Ok(geom::nms_by(cands, self.nms_iou, |p| p.bbox, |p| p.score))
    }
}
