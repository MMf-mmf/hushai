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
    assess_quality_parts(face.score, face.min_side(), sharpness, g)
}

/// Same gate as `assess_quality` but on raw parts — used after restoration re-measures a crop whose
/// effective size/sharpness changed (the detector score is unchanged).
pub fn assess_quality_parts(det_score: f32, min_side: f32, sharpness: f32, g: &FaceGates) -> FaceQuality {
    if det_score < g.min_det_score || min_side < g.min_px || sharpness < g.min_sharpness {
        return FaceQuality::Reject;
    }
    let confident = det_score >= (g.min_det_score + 0.2).min(0.95);
    let large = min_side >= g.min_px * 1.5;
    let sharp = sharpness >= g.min_sharpness * 1.5;
    if confident && large && sharp {
        FaceQuality::Mint
    } else {
        FaceQuality::AttachOnly
    }
}

/// Approximate head pose in degrees, estimated closed-form from the 5 landmarks (no extra model).
/// `roll` = eye-line tilt; `yaw` = nose horizontal offset between the eyes; `pitch` = nose vertical
/// position between the eye line and the mouth line. Coarse but good enough to gate minting and
/// weight best-shot selection. Landmark order: [eye-image-left, eye-image-right, nose, mouth-left,
/// mouth-right] (the convention both YuNet and SCRFD emit: index 0 sits on the image-left).
#[derive(Debug, Clone, Copy)]
pub struct Pose {
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
}

pub fn pose_from_landmarks(l: &[[f32; 2]; 5]) -> Pose {
    let (eye_l, eye_r, nose, mouth_l, mouth_r) = (l[0], l[1], l[2], l[3], l[4]);
    let eye_mid = [(eye_l[0] + eye_r[0]) * 0.5, (eye_l[1] + eye_r[1]) * 0.5];
    let mouth_mid = [(mouth_l[0] + mouth_r[0]) * 0.5, (mouth_l[1] + mouth_r[1]) * 0.5];
    let eye_dx = eye_r[0] - eye_l[0];
    let eye_dy = eye_r[1] - eye_l[1];
    let roll = eye_dy.atan2(eye_dx).to_degrees();
    let eye_dist = (eye_dx * eye_dx + eye_dy * eye_dy).sqrt().max(1e-3);
    // Yaw: nose horizontal offset from the eye midpoint, normalized by half the inter-eye distance.
    let r_yaw = ((nose[0] - eye_mid[0]) / (eye_dist * 0.5)).clamp(-1.0, 1.0);
    let yaw = r_yaw * 60.0;
    // Pitch: nose vertical position between eye line (≈0) and mouth line (≈1); neutral ≈0.5.
    let face_h = (mouth_mid[1] - eye_mid[1]).abs().max(1e-3);
    let r_pitch = (((nose[1] - eye_mid[1]) / face_h) - 0.5).clamp(-1.0, 1.0);
    let pitch = r_pitch * 60.0;
    Pose { yaw, pitch, roll }
}

/// True when the pose is frontal enough to MINT a new identity.
pub fn is_frontal(pose: &Pose, max_yaw_deg: f32, max_pitch_deg: f32) -> bool {
    pose.yaw.abs() <= max_yaw_deg && pose.pitch.abs() <= max_pitch_deg
}

pub struct FaceEmbedder {
    session: Session,
    flip_tta: bool,
}

impl FaceEmbedder {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            flip_tta: true,
        }
    }

    pub fn with_flip_tta(mut self, on: bool) -> Self {
        self.flip_tta = on;
        self
    }

    /// Embed one detected face from the raw frame: align via landmarks → ArcFace (+flip-TTA).
    pub fn embed(&self, frame: &RgbImage, face: &Face) -> Result<Vec<f32>> {
        let crop = align_crop(frame, &face.landmarks);
        self.embed_aligned(&crop)
    }

    /// Embed an already-aligned 112×112 RGB face crop (the restoration cascade aligns on restored
    /// pixels, then calls this). Flip-TTA averages the crop's embedding with its horizontal mirror.
    pub fn embed_aligned(&self, crop: &RgbImage) -> Result<Vec<f32>> {
        let mut v = self.embed_once(crop)?;
        if self.flip_tta {
            let flipped = image::imageops::flip_horizontal(crop);
            let v2 = self.embed_once(&flipped)?;
            for (a, b) in v.iter_mut().zip(v2.iter()) {
                *a += *b;
            }
            crate::vad::l2_normalize(&mut v);
        }
        Ok(v)
    }

    fn embed_once(&self, crop: &RgbImage) -> Result<Vec<f32>> {
        let crop = if crop.width() == SIZE as u32 && crop.height() == SIZE as u32 {
            std::borrow::Cow::Borrowed(crop)
        } else {
            std::borrow::Cow::Owned(super::enhance::resize_rgb(crop, SIZE as u32, SIZE as u32))
        };
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

    let mut out = RgbImage::new(SIZE as u32, SIZE as u32);
    for oy in 0..SIZE {
        for ox in 0..SIZE {
            let vx = ox as f32 - tx;
            let vy = oy as f32 - ty;
            let sx = (c * vx + d * vy) / det;
            let sy = (-d * vx + c * vy) / det;
            let px = super::enhance::bilinear_sample(frame, sx, sy);
            out.put_pixel(ox as u32, oy as u32, image::Rgb(px));
        }
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
