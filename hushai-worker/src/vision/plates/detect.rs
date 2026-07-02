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
    /// Output layout. `true` = END2END (open-image-models YOLOv9: `output0 [N,7]` =
    /// `[batch, x1,y1,x2,y2, class, score]`, NMS already applied). `false` = raw YOLOv8/11
    /// (`[C,N]` cxcywh + conf [+ 4 corner keypoints]). Chosen by `PLATE_DETECT_FORMAT`.
    end2end: bool,
}

impl PlateDetector {
    pub fn new(session: Session, input: usize, score_threshold: f32, end2end: bool) -> Self {
        Self {
            session,
            input: input.max(64),
            score_threshold,
            nms_iou: 0.45,
            end2end,
        }
    }

    /// Detect plates in an RGB crop. CPU-bound ONNX work — call inside `spawn_blocking`.
    pub fn detect(&self, crop: &RgbImage) -> Result<Vec<PlateBox>> {
        let (ow, oh) = (crop.width() as f32, crop.height() as f32);
        let n = self.input;
        let scale = (n as f32 / ow).min(n as f32 / oh);
        let (nw, nh) = ((ow * scale).round() as u32, (oh * scale).round() as u32);
        let resized = image::imageops::resize(crop, nw, nh, image::imageops::FilterType::Triangle);

        // CENTERED letterbox with 114/255 gray padding — the exact Ultralytics/YOLOv9 preprocessing
        // the model was trained/exported with (open-image-models `letterbox`). dw/dh are the per-side
        // pad offsets we subtract back out when mapping detections to crop pixels.
        let dw = (n as f32 - nw as f32) / 2.0;
        let dh = (n as f32 - nh as f32) / 2.0;
        let (dwi, dhi) = (dw.round() as usize, dh.round() as usize);
        let mut input = Array4::<f32>::from_elem((1, 3, n, n), 114.0 / 255.0);
        for y in 0..nh as usize {
            for x in 0..nw as usize {
                let px = resized.get_pixel(x as u32, y as u32).0;
                for c in 0..3 {
                    input[[0, c, y + dhi, x + dwi]] = px[c] as f32 / 255.0;
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
        // Normalize to 2-D [rows, cols], dropping ONLY a leading batch dim on a 3-D output. (Do NOT
        // squeeze all 1s: an end2end output with a single detection is [1,7] — the leading 1 is the
        // detection COUNT, not a batch/channel axis.)
        let raw_shape: Vec<usize> = t.shape().to_vec();
        let dims2: Vec<usize> = match raw_shape.len() {
            3 if raw_shape[0] == 1 => raw_shape[1..].to_vec(),
            2 => raw_shape.clone(),
            _ => anyhow::bail!("plate detector output {raw_shape:?} not 2-D/3-D"),
        };
        let data: Vec<f32> = t.iter().copied().collect();
        let (d0, d1) = (dims2[0], dims2[1]);
        let (channels, nboxes, channels_first) = if self.end2end {
            // END2END is ALWAYS row-major [N, fields]: fields (7) is the LAST dim; N (0/1/2/…) is first.
            (d1, d0, false)
        } else {
            // Raw YOLO: fields are the SMALLER dim (anchors ≫ channels, e.g. 8400 ≫ 5..17).
            if d0 <= d1 { (d0, d1, true) } else { (d1, d0, false) }
        };
        let at = |c: usize, i: usize| -> f32 {
            if channels_first {
                data[c * nboxes + i]
            } else {
                data[i * channels + c]
            }
        };
        // Map a model-input (letterboxed 640) coordinate back to crop pixels: undo the centered pad,
        // then the resize scale. x/y differ only by which pad offset applies.
        let unpad = |v: f32, off: f32| -> f32 { (v - off) / scale };

        let mut cands: Vec<PlateBox> = Vec::new();
        if self.end2end {
            // open-image-models YOLOv9 end2end: each row = [batch, x1, y1, x2, y2, class, score] in
            // model-input px, NMS already applied. Requires ≥6 fields (batch+box+score); 7 with class.
            anyhow::ensure!(channels >= 6, "end2end plate output has {channels} fields, expected ≥6");
            let score_col = channels - 1; // score is the LAST field (6 with class, else 5)
            for i in 0..nboxes {
                let score = at(score_col, i);
                if score < self.score_threshold {
                    continue;
                }
                let x1 = unpad(at(1, i), dw);
                let y1 = unpad(at(2, i), dh);
                let x2 = unpad(at(3, i), dw);
                let y2 = unpad(at(4, i), dh);
                let (w, h) = (x2 - x1, y2 - y1);
                if w <= 1.0 || h <= 1.0 {
                    continue;
                }
                cands.push(PlateBox {
                    bbox: [x1.max(0.0), y1.max(0.0), w, h],
                    score,
                    corners: None, // end2end bbox-only → axis-aligned crop (no deskew)
                });
            }
        } else {
            // Raw YOLOv8/11: [C,N] cxcywh + conf, and ≥8 trailing channels ⇒ 4 corner keypoints.
            anyhow::ensure!(channels >= 5, "plate detector has {channels} channels, expected ≥5");
            let kpt_block = channels.saturating_sub(5);
            let (has_corners, kstride) = if kpt_block >= 12 {
                (true, 3) // (x,y,visibility)×4
            } else if kpt_block >= 8 {
                (true, 2) // (x,y)×4
            } else {
                (false, 0)
            };
            for i in 0..nboxes {
                let conf = at(4, i);
                if conf < self.score_threshold {
                    continue;
                }
                let (cx, cy, bw, bh) = (at(0, i), at(1, i), at(2, i), at(3, i));
                let x = unpad(cx - bw / 2.0, dw);
                let y = unpad(cy - bh / 2.0, dh);
                let w = bw / scale;
                let h = bh / scale;
                if w <= 1.0 || h <= 1.0 {
                    continue;
                }
                let corners = if has_corners {
                    let mut c = [[0.0f32; 2]; 4];
                    for (k, cc) in c.iter_mut().enumerate() {
                        let base = 5 + k * kstride;
                        cc[0] = unpad(at(base, i), dw);
                        cc[1] = unpad(at(base + 1, i), dh);
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
        }
        Ok(geom::nms_by(cands, self.nms_iou, |p| p.bbox, |p| p.score))
    }
}
