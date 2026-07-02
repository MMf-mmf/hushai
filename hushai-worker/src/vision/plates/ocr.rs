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
    /// Input dtype: `true` = the model takes RAW uint8 pixels 0-255 (fast-plate-ocr CCT normalizes
    /// internally), `false` = float32 normalized 0-1 (CRNN/PaddleOCR). Detected from the ONNX input
    /// element type; feeding the wrong dtype makes ORT reject the run ("plate-ocr inference").
    u8_input: bool,
    /// Decode head: `true` = CTC/CRNN (collapse consecutive duplicates), `false` = fixed-length
    /// per-slot softmax (fast-plate-ocr CCT — a real double letter like "BB1234" MUST survive, so we
    /// do NOT collapse). Auto-default is fixed-length (the primary model); set `PLATE_OCR_CTC=true`
    /// for a CRNN/PaddleOCR recognizer.
    ctc: bool,
}

impl PlateOcr {
    /// `charset` is the ordered class→character map (index = class id), loaded from the export's
    /// sidecar JSON. Input geometry is read from the session, with sane fallbacks for dynamic axes.
    pub fn new(session: Session, charset: Vec<char>) -> Self {
        let input0 = session.inputs.first();
        let dims = input0
            .and_then(|i| i.input_type.tensor_dimensions().map(|d| d.to_vec()))
            .unwrap_or_default();
        let u8_input = input0.and_then(|i| i.input_type.tensor_type())
            == Some(ort::tensor::TensorElementType::Uint8);
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
            u8_input,
            ctc: false,
        }
    }

    /// Select the decode head: `true` = CTC duplicate-collapse (CRNN/PaddleOCR), `false` = fixed-length
    /// per-slot (fast-plate-ocr CCT). Default is fixed-length.
    pub fn with_ctc(mut self, ctc: bool) -> Self {
        self.ctc = ctc;
        self
    }

    /// Read a (rectified, enhanced) plate image. CPU-bound; call inside `spawn_blocking`.
    pub fn read(&self, img: &RgbImage) -> Result<PlateRead> {
        let resized = crate::vision::enhance::resize_rgb(img, self.w as u32, self.h as u32);
        // Raw 0-255 pixel (luma when the model wants 1 channel).
        let raw = |x: usize, y: usize, ch: usize| -> u8 {
            let p = resized.get_pixel(x as u32, y as u32).0;
            if self.c == 1 {
                (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32)
                    .round()
                    .clamp(0.0, 255.0) as u8
            } else {
                p[ch.min(2)]
            }
        };
        let shape: Vec<usize> = if self.nchw {
            vec![1, self.c, self.h, self.w]
        } else {
            vec![1, self.h, self.w, self.c]
        };
        let idx = |ch: usize, y: usize, x: usize| -> [usize; 4] {
            if self.nchw { [0, ch, y, x] } else { [0, y, x, ch] }
        };
        // Feed the dtype the model declares: RAW uint8 (fast-plate-ocr CCT normalizes internally) or
        // float32 normalized to 0-1 (CRNN/PaddleOCR). Feeding the wrong one makes ORT reject the run.
        let outputs = if self.u8_input {
            let mut input = ArrayD::<u8>::zeros(ndarray::IxDyn(&shape));
            for y in 0..self.h {
                for x in 0..self.w {
                    for ch in 0..self.c {
                        input[idx(ch, y, x)] = raw(x, y, ch);
                    }
                }
            }
            self.session
                .run(ort::inputs![input].context("plate-ocr inputs")?)
                .context("plate-ocr inference")?
        } else {
            let mut input = ArrayD::<f32>::zeros(ndarray::IxDyn(&shape));
            for y in 0..self.h {
                for x in 0..self.w {
                    for ch in 0..self.c {
                        input[idx(ch, y, x)] = raw(x, y, ch) as f32 / 255.0;
                    }
                }
            }
            self.session
                .run(ort::inputs![input].context("plate-ocr inputs")?)
                .context("plate-ocr inference")?
        };
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

        let (text, confs) = greedy_decode(&at, steps, classes, blank, &self.charset, self.ctc);
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

/// Greedy decode of a recognizer's `[steps, classes]` logits into a raw string + per-char softmax
/// confidences. `at(step, cls)` returns the logit; `blank=Some(i)` skips index `i` (CTC blank / the
/// fixed-length pad slot). `ctc=true` collapses consecutive duplicate classes (CRNN/PaddleOCR);
/// `ctc=false` keeps them — a fixed-length per-slot head (fast-plate-ocr CCT) must preserve real
/// double letters like "BB1234". Pure + side-effect-free so the load-bearing decode is unit-tested.
fn greedy_decode(
    at: &impl Fn(usize, usize) -> f32,
    steps: usize,
    classes: usize,
    blank: Option<usize>,
    charset: &[char],
    ctc: bool,
) -> (String, Vec<f32>) {
    let mut text = String::new();
    let mut confs: Vec<f32> = Vec::new();
    let mut prev: Option<usize> = None;
    for s in 0..steps {
        let mut max = f32::NEG_INFINITY;
        let mut argmax = 0usize;
        for cls in 0..classes {
            let v = at(s, cls);
            if v > max {
                max = v;
                argmax = cls;
            }
        }
        if Some(argmax) == blank {
            prev = None;
            continue;
        }
        if ctc && prev == Some(argmax) {
            continue; // CTC duplicate-collapse — ONLY for CTC heads
        }
        prev = Some(argmax);
        if let Some(&ch) = charset.get(argmax) {
            // Confidence = the winning class probability. If the head already output softmax
            // probabilities (max ≤ 1, e.g. fast-plate-ocr CCT), use it directly; only softmax when
            // the values are raw logits (max > 1, e.g. a CRNN). Double-softmaxing a probability output
            // collapsed confidence to ~0.07 and would trip the OCR quality gates on good reads.
            let conf = if max <= 1.0 {
                max
            } else {
                let denom: f32 = (0..classes).map(|cls| (at(s, cls) - max).exp()).sum();
                if denom > 0.0 { 1.0 / denom } else { 0.0 }
            };
            text.push(ch);
            confs.push(conf);
        }
    }
    (text, confs)
}

#[cfg(test)]
mod tests {
    use super::greedy_decode;

    // Build a [steps,classes] (class-last) one-hot logit grid from a slot→class sequence.
    fn grid(seq: &[usize], classes: usize) -> impl Fn(usize, usize) -> f32 + '_ {
        move |s: usize, c: usize| if seq[s] == c { 10.0 } else { 0.0 }
    }

    #[test]
    fn fixed_length_preserves_double_letters() {
        // charset 0..=9 then A..Z (36); model class 36 = pad/blank. "BB1234" then 3 pad slots.
        let charset: Vec<char> = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ".chars().collect();
        let b = |ch: char| charset.iter().position(|&c| c == ch).unwrap();
        let seq = vec![b('B'), b('B'), b('1'), b('2'), b('3'), b('4'), 36, 36, 36];
        let at = grid(&seq, 37);
        let (text, confs) = greedy_decode(&at, 9, 37, Some(36), &charset, false);
        assert_eq!(text, "BB1234", "fixed-length head must keep the double B");
        assert_eq!(confs.len(), 6);
    }

    #[test]
    fn ctc_collapses_runs_but_blank_separates() {
        let charset: Vec<char> = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ".chars().collect();
        let b = |ch: char| charset.iter().position(|&c| c == ch).unwrap();
        // CTC stream: A A <blank> A 1 -> "AA1" (blank separates the two A-runs).
        let seq = vec![b('A'), b('A'), 36, b('A'), b('1')];
        let at = grid(&seq, 37);
        let (text, _) = greedy_decode(&at, 5, 37, Some(36), &charset, true);
        assert_eq!(text, "AA1");
        // Without the separating blank, the run collapses to one A.
        let seq2 = vec![b('A'), b('A'), b('A'), b('1'), 36];
        let at2 = grid(&seq2, 37);
        let (text2, _) = greedy_decode(&at2, 5, 37, Some(36), &charset, true);
        assert_eq!(text2, "A1");
    }
}
