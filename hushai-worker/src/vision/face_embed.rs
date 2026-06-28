//! Face embedding via ArcFace (`w600k_r50` / garavv `arc.onnx`). Input is an ALIGNED 112×112 RGB
//! crop, NHWC `[1,112,112,3]`, normalized `(x-127.5)/128.0`; output is a 512-d vector we
//! L2-normalize for cosine comparison (mirrors the speaker voiceprint design). Alignment is a
//! closed-form 2D similarity transform (rotation + uniform scale + translation, no reflection)
//! mapping the 5 detected landmarks onto the canonical ArcFace template, then a bilinear warp.
//!
//! Quality gates (the visual analogue of the VAD/SNR gate) reject crops that would poison a
//! centroid: low detection score, too-small faces, motion-blurred crops. A `Reject` writes no row
//! and folds no centroid; only a clean `Mint`-grade face may create a NEW person identity.

use anyhow::{Context, Result};
use image::RgbImage;
use ndarray::Array4;
use ort::session::Session;

use super::detect::Face;

const SIZE: usize = 112;

/// Canonical 5-point ArcFace template for a 112×112 crop (InsightFace).
/// Order: [left-eye, right-eye, nose, left-mouth-corner, right-mouth-corner] in IMAGE coords
/// (index 0 ≈ x=38 is the LEFT side of the image). YuNet emits [right-eye, left-eye, nose,
/// right-mouth, left-mouth] from the PERSON's perspective — i.e. the person's right eye lands on
/// the image-left, matching template[0]. So YuNet's native order already aligns to the template.
/// If calibration shows mirrored/garbage embeddings, swap the eye pair (0,1) and mouth pair (3,4).
const TEMPLATE: [[f32; 2]; 5] = [
    [38.2946, 51.6963],
    [73.5318, 51.5014],
    [56.0252, 71.7366],
    [41.5493, 92.3655],
    [70.7299, 92.2041],
];

/// Input-crop quality, mirroring `vad::SpeakerQuality`. Only `Mint` may create a new identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaceQuality {
    Mint,
    AttachOnly,
    Reject,
}

/// The gates a face crop must clear (from config).
#[derive(Debug, Clone, Copy)]
pub struct FaceGates {
    pub min_det_score: f32,
    pub min_px: f32,
    pub min_sharpness: f32,
}

/// Classify a detected face by detector confidence, size, and crop sharpness. `Mint` requires
/// comfortable headroom above every reject gate (a clean, large, sharp, confident face); marginal
/// faces `AttachOnly` (can identify a known person but never mint); failures `Reject`.
pub fn assess_quality(face: &Face, sharpness: f32, g: &FaceGates) -> FaceQuality {
    if face.score < g.min_det_score || face.min_side() < g.min_px || sharpness < g.min_sharpness {
        return FaceQuality::Reject;
    }
    let confident = face.score >= (g.min_det_score + 0.2).min(0.95);
    let large = face.min_side() >= g.min_px * 1.5;
    let sharp = sharpness >= g.min_sharpness * 1.5;
    if confident && large && sharp {
        FaceQuality::Mint
    } else {
        FaceQuality::AttachOnly
    }
}

pub struct FaceEmbedder {
    session: Session,
}

impl FaceEmbedder {
    pub fn new(session: Session) -> Self {
        Self { session }
    }

    /// Embed one detected face into a 512-d L2-normalized vector. CPU-bound; call in spawn_blocking.
    pub fn embed(&self, frame: &RgbImage, face: &Face) -> Result<Vec<f32>> {
        let crop = align_crop(frame, &face.landmarks);
        let mut input = Array4::<f32>::zeros((1, SIZE, SIZE, 3)); // NHWC, RGB
        for y in 0..SIZE {
            for x in 0..SIZE {
                let p = crop.get_pixel(x as u32, y as u32).0;
                for c in 0..3 {
                    input[[0, y, x, c]] = (p[c] as f32 - 127.5) / 128.0;
                }
            }
        }
        let outputs = self
            .session
            .run(ort::inputs![input].context("arcface inputs")?)
            .context("arcface inference")?;
        let t = outputs["embedding"]
            .try_extract_tensor::<f32>()
            .context("extracting arcface embedding")?;
        let mut v: Vec<f32> = t.iter().copied().collect();
        anyhow::ensure!(
            v.len() == 512,
            "arcface returned {} dims, expected 512",
            v.len()
        );
        anyhow::ensure!(
            v.iter().all(|x| x.is_finite()),
            "arcface returned non-finite embedding"
        );
        crate::vad::l2_normalize(&mut v);
        Ok(v)
    }
}

/// Warp `frame` to a 112×112 RGB crop aligned to the ArcFace template via a 2D similarity transform
/// estimated from the 5 landmarks. Returns a 112×112 image; out-of-bounds samples are black.
pub fn align_crop(frame: &RgbImage, landmarks: &[[f32; 2]; 5]) -> RgbImage {
    // Closed-form similarity (z = c + i d = scale*rotation) mapping src(landmarks) -> dst(template).
    let mean = |pts: &[[f32; 2]; 5]| {
        let (mut sx, mut sy) = (0.0f32, 0.0f32);
        for p in pts {
            sx += p[0];
            sy += p[1];
        }
        [sx / 5.0, sy / 5.0]
    };
    let ms = mean(landmarks);
    let md = mean(&TEMPLATE);
    let (mut a, mut b, mut denom) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..5 {
        let px = landmarks[i][0] - ms[0];
        let py = landmarks[i][1] - ms[1];
        let qx = TEMPLATE[i][0] - md[0];
        let qy = TEMPLATE[i][1] - md[1];
        a += px * qx + py * qy; // Re(conj(p)*q)
        b += px * qy - py * qx; // Im(conj(p)*q)
        denom += px * px + py * py;
    }
    let denom = if denom.abs() < 1e-9 { 1e-9 } else { denom };
    let c = a / denom;
    let d = b / denom;
    // M = [[c,-d],[d,c]] maps src->dst; t = md - M*ms.
    let tx = md[0] - (c * ms[0] - d * ms[1]);
    let ty = md[1] - (d * ms[0] + c * ms[1]);
    // Inverse map (dst->src): src = M^{-1}(dst - t), M^{-1} = (1/det)[[c,d],[-d,c]], det = c²+d².
    let det = c * c + d * d;
    let det = if det.abs() < 1e-9 { 1e-9 } else { det };

    let (fw, fh) = (frame.width() as i32, frame.height() as i32);
    let mut out = RgbImage::new(SIZE as u32, SIZE as u32);
    for oy in 0..SIZE {
        for ox in 0..SIZE {
            let vx = ox as f32 - tx;
            let vy = oy as f32 - ty;
            let sx = (c * vx + d * vy) / det;
            let sy = (-d * vx + c * vy) / det;
            let px = bilinear(frame, sx, sy, fw, fh);
            out.put_pixel(ox as u32, oy as u32, image::Rgb(px));
        }
    }
    out
}

/// Bilinear-sample an RGB frame at (x, y); out-of-bounds returns black.
fn bilinear(frame: &RgbImage, x: f32, y: f32, w: i32, h: i32) -> [u8; 3] {
    if x < 0.0 || y < 0.0 || x > (w - 1) as f32 || y > (h - 1) as f32 {
        return [0, 0, 0];
    }
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let dx = x - x0 as f32;
    let dy = y - y0 as f32;
    let mut out = [0u8; 3];
    for c in 0..3 {
        let p00 = frame.get_pixel(x0 as u32, y0 as u32).0[c] as f32;
        let p10 = frame.get_pixel(x1 as u32, y0 as u32).0[c] as f32;
        let p01 = frame.get_pixel(x0 as u32, y1 as u32).0[c] as f32;
        let p11 = frame.get_pixel(x1 as u32, y1 as u32).0[c] as f32;
        let top = p00 * (1.0 - dx) + p10 * dx;
        let bot = p01 * (1.0 - dx) + p11 * dx;
        out[c] = (top * (1.0 - dy) + bot * dy).round().clamp(0.0, 255.0) as u8;
    }
    out
}

/// Variance of the Laplacian over the green channel — a blur/sharpness measure. Low => blurry.
pub fn sharpness(crop: &RgbImage) -> f32 {
    let (w, h) = (crop.width() as i32, crop.height() as i32);
    if w < 3 || h < 3 {
        return 0.0;
    }
    let g = |x: i32, y: i32| crop.get_pixel(x as u32, y as u32).0[1] as f32;
    let mut vals = Vec::with_capacity(((w - 2) * (h - 2)) as usize);
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            // Laplacian kernel [[0,1,0],[1,-4,1],[0,1,0]]
            let lap = g(x, y - 1) + g(x - 1, y) + g(x + 1, y) + g(x, y + 1) - 4.0 * g(x, y);
            vals.push(lap);
        }
    }
    let n = vals.len() as f32;
    if n == 0.0 {
        return 0.0;
    }
    let mean = vals.iter().sum::<f32>() / n;
    vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face_at(landmarks: [[f32; 2]; 5], score: f32, side: f32) -> Face {
        Face {
            bbox: [0.0, 0.0, side, side],
            score,
            landmarks,
        }
    }

    #[test]
    fn identity_alignment_when_landmarks_equal_template() {
        // A frame where the landmarks already sit on the template: the warp should be ~identity,
        // so a marker pixel placed at template[0] survives near template[0] in the crop.
        let mut frame = RgbImage::new(SIZE as u32, SIZE as u32);
        for p in frame.pixels_mut() {
            *p = image::Rgb([10, 10, 10]);
        }
        let (mx, my) = (TEMPLATE[0][0] as u32, TEMPLATE[0][1] as u32);
        frame.put_pixel(mx, my, image::Rgb([250, 0, 0]));
        let crop = align_crop(&frame, &TEMPLATE);
        // The bright marker should land at (about) the same spot.
        let here = crop.get_pixel(mx, my).0[0];
        assert!(
            here > 100,
            "expected the marker near template[0], got R={here}"
        );
    }

    #[test]
    fn quality_gates() {
        let g = FaceGates {
            min_det_score: 0.6,
            min_px: 40.0,
            min_sharpness: 30.0,
        };
        // too small / low score / blurry => Reject
        assert_eq!(
            assess_quality(&face_at(TEMPLATE, 0.5, 100.0), 100.0, &g),
            FaceQuality::Reject
        );
        assert_eq!(
            assess_quality(&face_at(TEMPLATE, 0.9, 20.0), 100.0, &g),
            FaceQuality::Reject
        );
        assert_eq!(
            assess_quality(&face_at(TEMPLATE, 0.9, 100.0), 5.0, &g),
            FaceQuality::Reject
        );
        // clean, large, sharp, confident => Mint
        assert_eq!(
            assess_quality(&face_at(TEMPLATE, 0.95, 100.0), 100.0, &g),
            FaceQuality::Mint
        );
        // passes gates but marginal (just above the floors) => AttachOnly
        assert_eq!(
            assess_quality(&face_at(TEMPLATE, 0.62, 45.0), 35.0, &g),
            FaceQuality::AttachOnly
        );
    }
}
