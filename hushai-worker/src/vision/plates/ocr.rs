//! Plate OCR via an ONNX recognizer (fast-plate-ocr CCT primary; PaddleOCR-rec / CRNN+CTC alt).
//! NOT tesseract. Emits a string + per-character confidences for temporal voting.
//!
//! ⚠️ DECODE-VALIDATED-AT-PROVISIONING. Recognizer exports differ in layout (NCHW vs NHWC, grayscale
//! vs RGB) and head (fixed-length softmax vs CTC). This decoder inspects the session's declared I/O
//! to pick the layout, runs greedy per-timestep argmax with CTC blank/duplicate collapse, and maps
//! indices through the provisioned charset (`models/lp_ocr_charset.json`). Validate against the real
//! export with `tests/vision_pipeline.rs::inspect_plate_model_io_shapes`.

use anyhow::{Context, Result};
use image::RgbImage;
use ndarray::ArrayD;
use ort::session::Session;

use super::normalize::PlateRead;

pub struct PlateOcr {
    session: Session,
    charset: Vec<char>,
    // Resolved input geometry.
    c: usize,
    h: usize,
    w: usize,
    nchw: bool,
}

impl PlateOcr {
    /// `charset` is the ordered class→character map (index = class id), loaded from the export's
    /// sidecar JSON. Input geometry is read from the session, with sane fallbacks for dynamic axes.
    pub fn new(session: Session, charset: Vec<char>) -> Self {
        let dims = session
            .inputs
            .first()
            .and_then(|i| i.input_type.tensor_dimensions().map(|d| d.to_vec()))
            .unwrap_or_default();
        // Identify layout: a channel axis is 1 (gray) or 3 (RGB).
        let (mut c, mut h, mut w, mut nchw) = (3usize, 48usize, 160usize, true);
        if dims.len() == 4 {
            let d = |i: usize| dims[i];
            let is_ch = |v: i64| v == 1 || v == 3;
            if is_ch(d(1)) && !is_ch(d(3)) {
                nchw = true;
                c = d(1) as usize;
                if d(2) > 0 {
                    h = d(2) as usize;
                }
                if d(3) > 0 {
                    w = d(3) as usize;
                }
            } else if is_ch(d(3)) {
                nchw = false;
                c = d(3) as usize;
                if d(1) > 0 {
                    h = d(1) as usize;
                }
                if d(2) > 0 {
                    w = d(2) as usize;
                }
            }
        }
        Self {
            session,
            charset,
            c: c.max(1),
            h: h.max(8),
            w: w.max(8),
            nchw,
        }
    }

    /// Read a (rectified, enhanced) plate image. CPU-bound; call inside `spawn_blocking`.
    pub fn read(&self, img: &RgbImage) -> Result<PlateRead> {
        let resized = crate::vision::enhance::resize_rgb(img, self.w as u32, self.h as u32);
        let pixel = |x: usize, y: usize, ch: usize| -> f32 {
            let p = resized.get_pixel(x as u32, y as u32).0;
            if self.c == 1 {
                // luma
                (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) / 255.0
            } else {
                p[ch.min(2)] as f32 / 255.0
            }
        };
        let shape: Vec<usize> = if self.nchw {
            vec![1, self.c, self.h, self.w]
        } else {
            vec![1, self.h, self.w, self.c]
        };
        let mut input = ArrayD::<f32>::zeros(ndarray::IxDyn(&shape));
        for y in 0..self.h {
            for x in 0..self.w {
                for ch in 0..self.c {
                    let v = pixel(x, y, ch);
                    if self.nchw {
                        input[[0, ch, y, x]] = v;
                    } else {
                        input[[0, y, x, ch]] = v;
                    }
                }
            }
        }

        let outputs = self
            .session
            .run(ort::inputs![input].context("plate-ocr inputs")?)
            .context("plate-ocr inference")?;
        let out_name = self
            .session
            .outputs
            .first()
            .map(|o| o.name.clone())
            .context("plate ocr has no outputs")?;
        let t = outputs[out_name.as_str()]
            .try_extract_tensor::<f32>()
            .context("extracting plate-ocr output")?;
        let oshape: Vec<usize> = t.shape().iter().copied().filter(|&d| d != 1).collect();
        anyhow::ensure!(
            oshape.len() == 2,
            "plate-ocr output (squeezed) {oshape:?} not 2-D [T,C]/[C,T]"
        );
        let data: Vec<f32> = t.iter().copied().collect();
        // The class axis matches the charset size (±1 for a CTC blank); the other axis is time.
        let nc = self.charset.len();
        let (a, b) = (oshape[0], oshape[1]);
        let class_is_0 = a == nc || a == nc + 1;
        let (classes, steps, classes_first) = if class_is_0 {
            (a, b, true)
        } else {
            (b, a, false)
        };
        let blank = if classes == nc + 1 { Some(nc) } else { None };
        let at = |step: usize, cls: usize| -> f32 {
            if classes_first {
                data[cls * steps + step]
            } else {
                data[step * classes + cls]
            }
        };

        let mut text = String::new();
        let mut confs: Vec<f32> = Vec::new();
        let mut prev: Option<usize> = None;
        for s in 0..steps {
            // softmax over classes for this timestep
            let mut max = f32::NEG_INFINITY;
            let mut argmax = 0usize;
            for cls in 0..classes {
                let v = at(s, cls);
                if v > max {
                    max = v;
                    argmax = cls;
                }
            }
            // CTC collapse: skip blank + consecutive duplicates.
            if Some(argmax) == blank {
                prev = None;
                continue;
            }
            if prev == Some(argmax) {
                continue;
            }
            prev = Some(argmax);
            if let Some(&ch) = self.charset.get(argmax) {
                // softmax prob for confidence
                let mut denom = 0.0f32;
                for cls in 0..classes {
                    denom += (at(s, cls) - max).exp();
                }
                let prob = if denom > 0.0 { 1.0 / denom } else { 0.0 };
                text.push(ch);
                confs.push(prob);
            }
        }
        let norm = super::normalize::normalize(&text);
        // Re-pair confidences with the kept (alphanumeric) characters.
        let kept_confs: Vec<f32> = if confs.len() == norm.chars().count() {
            confs
        } else {
            vec![
                if confs.is_empty() {
                    0.0
                } else {
                    confs.iter().sum::<f32>() / confs.len() as f32
                };
                norm.chars().count()
            ]
        };
        Ok(PlateRead::new(norm, kept_confs))
    }
}
