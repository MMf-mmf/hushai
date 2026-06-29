//! Shared image-cleanup core used by BOTH recognition lanes (faces and license plates).
//!
//! The pipeline mandate is "whenever we identify a person or a car, clean up the image — zoom and
//! crop — to get the clearest image before we recognize it." This module owns the model-agnostic
//! primitives for that: context-margin cropping, high-quality resize, perspective rectification
//! (deskew), local-contrast normalization (CLAHE), unsharp masking, plus thin ONNX wrappers for a
//! super-resolution model (Real-ESRGAN) and a blind-face-restoration model (GFPGAN / CodeFormer).
//!
//! Face lane:  detect → `crop_with_margin` → `Upscaler` (if tiny) → `FaceRestorer` → align → ArcFace.
//! Plate lane: detect → `crop_with_margin` (vehicle ROI) → plate-detect → `homography_warp` (rectify)
//!             → `Upscaler` (if small) → `clahe_gray` + `unsharp_mask` → OCR.
//!
//! ⚠️ The two ONNX wrappers are OPERATOR-PROVISIONED + DECODE-VALIDATED-AT-PROVISIONING (like
//! `objects.rs`): the exact export I/O is confirmed by `tests/vision_pipeline.rs` at provisioning,
//! not committed. They read outputs by ORDER and are defensive about shape/normalization. Any
//! failure is surfaced as an `Err` so the caller can fall back (restoration is recall-recovery, never
//! load-bearing).

use std::str::FromStr;

use anyhow::{Context, Result};
use image::RgbImage;
use ndarray::{Array1, Array4};
use ort::session::Session;

/// Which face detector to run. Resolved from config; both implement `detect::FaceDetect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectorKind {
    /// SCRFD-10GF (InsightFace) — best small/distant-face recall; the default.
    Scrfd,
    /// YuNet (OpenCV Zoo) — lighter, proven; configurable fallback.
    YuNet,
}

impl FromStr for DetectorKind {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "scrfd" => Ok(DetectorKind::Scrfd),
            "yunet" => Ok(DetectorKind::YuNet),
            other => Err(format!("unknown face detector kind '{other}' (want scrfd|yunet)")),
        }
    }
}

/// Which blind-face-restoration model the `FaceRestorer` wraps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorerKind {
    /// GFPGANv1.4 — single image input, more identity-faithful (lower embedding drift). Default.
    Gfpgan,
    /// CodeFormer — stronger on severe degradation; takes an optional fidelity weight `w`.
    CodeFormer,
}

impl FromStr for RestorerKind {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gfpgan" => Ok(RestorerKind::Gfpgan),
            "codeformer" | "code_former" => Ok(RestorerKind::CodeFormer),
            other => Err(format!(
                "unknown face restorer kind '{other}' (want gfpgan|codeformer)"
            )),
        }
    }
}

/// Bilinear-sample an RGB frame at (x, y); out-of-bounds returns black. Shared by every warp helper
/// here and by `face_embed::align_crop`, so there is ONE sampler.
pub fn bilinear_sample(frame: &RgbImage, x: f32, y: f32) -> [u8; 3] {
    let (w, h) = (frame.width() as i32, frame.height() as i32);
    if w < 1 || h < 1 || x < 0.0 || y < 0.0 || x > (w - 1) as f32 || y > (h - 1) as f32 {
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

/// Expand a `[x, y, w, h]` detector box by `margin_frac` of its size on every side, clamp to frame
/// bounds, and crop. Returns the crop plus its top-left `[x0, y0]` offset in original-frame pixels so
/// the caller can translate landmarks/corners into crop-local coordinates
/// (`local = original - offset`). Restoration models need facial/plate context (hairline, plate
/// border) that the tight recognition warp throws away — this gives them that context.
pub fn crop_with_margin(frame: &RgbImage, bbox: &[f32; 4], margin_frac: f32) -> (RgbImage, [f32; 2]) {
    let (fw, fh) = (frame.width() as f32, frame.height() as f32);
    let m = margin_frac.max(0.0);
    let mx = bbox[2] * m;
    let my = bbox[3] * m;
    let x0 = (bbox[0] - mx).clamp(0.0, (fw - 1.0).max(0.0));
    let y0 = (bbox[1] - my).clamp(0.0, (fh - 1.0).max(0.0));
    let x1 = (bbox[0] + bbox[2] + mx).clamp(x0 + 1.0, fw);
    let y1 = (bbox[1] + bbox[3] + my).clamp(y0 + 1.0, fh);
    let w = (x1 - x0).round().max(1.0) as u32;
    let h = (y1 - y0).round().max(1.0) as u32;
    let crop = image::imageops::crop_imm(frame, x0 as u32, y0 as u32, w, h).to_image();
    (crop, [x0, y0])
}

/// High-quality (Lanczos3) RGB resize. Used for upscaling tiny crops and normalizing model inputs.
pub fn resize_rgb(img: &RgbImage, w: u32, h: u32) -> RgbImage {
    image::imageops::resize(
        img,
        w.max(1),
        h.max(1),
        image::imageops::FilterType::Lanczos3,
    )
}

/// Unsharp mask: `out = orig + amount * (orig - gaussian_blur(orig, sigma))`, clamped. A mild
/// `amount` (~0.8) crisps soft edges after upscaling without ringing.
pub fn unsharp_mask(img: &RgbImage, amount: f32, sigma: f32) -> RgbImage {
    let blur = image::imageops::blur(img, sigma.max(0.1));
    let mut out = img.clone();
    for (po, (pi, pb)) in out.pixels_mut().zip(img.pixels().zip(blur.pixels())) {
        for c in 0..3 {
            let o = pi.0[c] as f32;
            let b = pb.0[c] as f32;
            po.0[c] = (o + amount * (o - b)).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// Rotate `img` about its center by `angle_rad` (positive = counter-clockwise), same output size,
/// bilinear sampling, black fill. Used to level a face's eye line or a plate's text baseline.
pub fn deskew(img: &RgbImage, angle_rad: f32) -> RgbImage {
    let (w, h) = (img.width(), img.height());
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let (s, c) = (angle_rad.sin(), angle_rad.cos());
    let mut out = RgbImage::new(w, h);
    for oy in 0..h {
        for ox in 0..w {
            let dx = ox as f32 - cx;
            let dy = oy as f32 - cy;
            // Sample the source at the inverse rotation of the destination pixel.
            let sx = cx + c * dx + s * dy;
            let sy = cy - s * dx + c * dy;
            out.put_pixel(ox, oy, image::Rgb(bilinear_sample(img, sx, sy)));
        }
    }
    out
}

/// Perspective-rectify a quadrilateral region to a fronto-parallel `dst_w × dst_h` image. `corners`
/// are the source quad in original-frame pixels, ordered to match the destination rectangle corners
/// `[[0,0], [W,0], [W,H], [0,H]]` (i.e. top-left, top-right, bottom-right, bottom-left). This is the
/// plate "deskew + zoom": a tilted plate becomes a clean rectangle the OCR can read.
pub fn homography_warp(
    frame: &RgbImage,
    corners: &[[f32; 2]; 4],
    dst_w: u32,
    dst_h: u32,
) -> RgbImage {
    let (w, h) = (dst_w.max(1), dst_h.max(1));
    let dst = [
        [0.0f64, 0.0],
        [w as f64, 0.0],
        [w as f64, h as f64],
        [0.0, h as f64],
    ];
    // Build the 8x8 system mapping dst-rectangle coords -> source corners, solve for h0..h7.
    let mut a = [[0.0f64; 8]; 8];
    let mut b = [0.0f64; 8];
    for i in 0..4 {
        let (x, y) = (dst[i][0], dst[i][1]);
        let (u, v) = (corners[i][0] as f64, corners[i][1] as f64);
        a[2 * i] = [x, y, 1.0, 0.0, 0.0, 0.0, -x * u, -y * u];
        b[2 * i] = u;
        a[2 * i + 1] = [0.0, 0.0, 0.0, x, y, 1.0, -x * v, -y * v];
        b[2 * i + 1] = v;
    }
    let mut out = RgbImage::new(w, h);
    let Some(hm) = solve8(a, b) else {
        // Degenerate quad: fall back to an axis-aligned crop of the corners' bounding box.
        let xs = corners.iter().map(|c| c[0]);
        let ys = corners.iter().map(|c| c[1]);
        let x0 = xs.clone().fold(f32::INFINITY, f32::min);
        let y0 = ys.clone().fold(f32::INFINITY, f32::min);
        let bw = xs.fold(f32::NEG_INFINITY, f32::max) - x0;
        let bh = ys.fold(f32::NEG_INFINITY, f32::max) - y0;
        let (crop, _) = crop_with_margin(frame, &[x0, y0, bw.max(1.0), bh.max(1.0)], 0.0);
        return resize_rgb(&crop, w, h);
    };
    for oy in 0..h {
        for ox in 0..w {
            let (x, y) = (ox as f64, oy as f64);
            let denom = hm[6] * x + hm[7] * y + 1.0;
            let denom = if denom.abs() < 1e-12 { 1e-12 } else { denom };
            let u = (hm[0] * x + hm[1] * y + hm[2]) / denom;
            let v = (hm[3] * x + hm[4] * y + hm[5]) / denom;
            out.put_pixel(ox, oy, image::Rgb(bilinear_sample(frame, u as f32, v as f32)));
        }
    }
    out
}

/// Contrast-Limited Adaptive Histogram Equalization on luminance, returned as an RGB grayscale image.
/// Tiled (8×8, adaptive for small inputs) with a clip limit and bilinear blending between tile
/// mappings — the standard CLAHE that makes low-contrast / glare-washed plates legible.
pub fn clahe_gray(img: &RgbImage) -> RgbImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w < 2 || h < 2 {
        return img.clone();
    }
    // Luma plane (Rec.601).
    let luma: Vec<u8> = img
        .pixels()
        .map(|p| {
            (0.299 * p.0[0] as f32 + 0.587 * p.0[1] as f32 + 0.114 * p.0[2] as f32)
                .round()
                .clamp(0.0, 255.0) as u8
        })
        .collect();

    let tiles_x = 8.min(w).max(1);
    let tiles_y = 8.min(h).max(1);
    let tw = w.div_ceil(tiles_x);
    let th = h.div_ceil(tiles_y);
    let clip_limit = 4.0f32; // multiples of the average bin height

    // Per-tile 256-entry mapping (CDF after histogram clipping).
    let mut maps = vec![[0u8; 256]; tiles_x * tiles_y];
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let mut hist = [0u32; 256];
            let mut count = 0u32;
            for yy in (ty * th)..((ty + 1) * th).min(h) {
                for xx in (tx * tw)..((tx + 1) * tw).min(w) {
                    hist[luma[yy * w + xx] as usize] += 1;
                    count += 1;
                }
            }
            if count == 0 {
                for (i, m) in maps[ty * tiles_x + tx].iter_mut().enumerate() {
                    *m = i as u8;
                }
                continue;
            }
            // Clip the histogram and redistribute the excess uniformly.
            let limit = ((clip_limit * count as f32) / 256.0).max(1.0) as u32;
            let mut excess = 0u32;
            for hbin in hist.iter_mut() {
                if *hbin > limit {
                    excess += *hbin - limit;
                    *hbin = limit;
                }
            }
            let add = excess / 256;
            let rem = excess % 256;
            for (i, hbin) in hist.iter_mut().enumerate() {
                *hbin += add + if (i as u32) < rem { 1 } else { 0 };
            }
            // CDF → mapping over [0,255].
            let mut cdf = 0u32;
            let total = count;
            let map = &mut maps[ty * tiles_x + tx];
            for i in 0..256 {
                cdf += hist[i];
                map[i] = ((cdf as f32 / total as f32) * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    // Bilinearly blend the 4 surrounding tile mappings for each pixel.
    let mut out = RgbImage::new(w as u32, h as u32);
    for y in 0..h {
        // tile-center coordinate space
        let gy = (y as f32 - th as f32 / 2.0) / th as f32;
        let ty0 = gy.floor();
        let fy = gy - ty0;
        let ty0 = (ty0 as isize).clamp(0, tiles_y as isize - 1) as usize;
        let ty1 = (ty0 + 1).min(tiles_y - 1);
        for x in 0..w {
            let gx = (x as f32 - tw as f32 / 2.0) / tw as f32;
            let tx0 = gx.floor();
            let fx = gx - tx0;
            let tx0 = (tx0 as isize).clamp(0, tiles_x as isize - 1) as usize;
            let tx1 = (tx0 + 1).min(tiles_x - 1);
            let v = luma[y * w + x] as usize;
            let m00 = maps[ty0 * tiles_x + tx0][v] as f32;
            let m01 = maps[ty0 * tiles_x + tx1][v] as f32;
            let m10 = maps[ty1 * tiles_x + tx0][v] as f32;
            let m11 = maps[ty1 * tiles_x + tx1][v] as f32;
            let top = m00 * (1.0 - fx) + m01 * fx;
            let bot = m10 * (1.0 - fx) + m11 * fx;
            let g = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
            out.put_pixel(x as u32, y as u32, image::Rgb([g, g, g]));
        }
    }
    out
}

/// Solve `A x = b` for an 8×8 system via Gaussian elimination with partial pivoting. `None` if
/// singular (degenerate correspondences).
fn solve8(mut a: [[f64; 8]; 8], mut b: [f64; 8]) -> Option<[f64; 8]> {
    for col in 0..8 {
        let mut piv = col;
        for r in (col + 1)..8 {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if a[piv][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, piv);
        b.swap(col, piv);
        for r in 0..8 {
            if r == col {
                continue;
            }
            let f = a[r][col] / a[col][col];
            for c in col..8 {
                a[r][c] -= f * a[col][c];
            }
            b[r] -= f * b[col];
        }
    }
    let mut x = [0.0f64; 8];
    for i in 0..8 {
        x[i] = b[i] / a[i][i];
    }
    Some(x)
}

/// Super-resolution wrapper (Real-ESRGAN x4plus). Input/output are RGB, NCHW, values in `[0,1]`;
/// the upscale factor is inferred from the output/input spatial ratio. Used to recover detail on
/// tiny face crops and small/distant plates before recognition.
pub struct Upscaler {
    session: Session,
}

impl Upscaler {
    pub fn new(session: Session) -> Self {
        Self { session }
    }

    /// Upscale an RGB image. Returns the model's higher-resolution output (RGB8).
    pub fn upscale(&self, img: &RgbImage) -> Result<RgbImage> {
        let (iw, ih) = (img.width() as usize, img.height() as usize);
        anyhow::ensure!(iw > 0 && ih > 0, "upscale: empty input");
        let mut input = Array4::<f32>::zeros((1, 3, ih, iw));
        for y in 0..ih {
            for x in 0..iw {
                let p = img.get_pixel(x as u32, y as u32).0;
                for c in 0..3 {
                    input[[0, c, y, x]] = p[c] as f32 / 255.0;
                }
            }
        }
        let outputs = self
            .session
            .run(ort::inputs![input].context("realesrgan inputs")?)
            .context("realesrgan inference")?;
        let out_name = self
            .session
            .outputs
            .first()
            .map(|o| o.name.clone())
            .context("realesrgan has no outputs")?;
        let t = outputs[out_name.as_str()]
            .try_extract_tensor::<f32>()
            .context("extracting realesrgan output")?;
        let shape = t.shape().to_vec();
        anyhow::ensure!(
            shape.len() == 4 && shape[1] == 3,
            "realesrgan output shape {shape:?} not [1,3,H,W]"
        );
        let (oh, ow) = (shape[2], shape[3]);
        let data: Vec<f32> = t.iter().copied().collect();
        let plane = oh * ow;
        let mut out = RgbImage::new(ow as u32, oh as u32);
        for y in 0..oh {
            for x in 0..ow {
                let mut px = [0u8; 3];
                for c in 0..3 {
                    let v = data[c * plane + y * ow + x];
                    px[c] = (v * 255.0).round().clamp(0.0, 255.0) as u8;
                }
                out.put_pixel(x as u32, y as u32, image::Rgb(px));
            }
        }
        Ok(out)
    }
}

/// Blind-face-restoration wrapper (GFPGAN / CodeFormer). Both take an aligned-ish 512×512 RGB face
/// (NCHW, normalized to `[-1,1]`) and return a restored 512×512 RGB face. CodeFormer exports may
/// expose a second `w` (fidelity) input; we pass `fidelity_w` when the session has 2 inputs.
pub struct FaceRestorer {
    session: Session,
    kind: RestorerKind,
    fidelity_w: f32,
}

const RESTORE_SIZE: usize = 512;

impl FaceRestorer {
    pub fn new(session: Session, kind: RestorerKind, fidelity_w: f32) -> Self {
        Self {
            session,
            kind,
            fidelity_w,
        }
    }

    pub fn kind(&self) -> RestorerKind {
        self.kind
    }

    /// Restore a face crop. Input is resized to 512×512; output is the restored 512×512 RGB image.
    pub fn restore(&self, face: &RgbImage) -> Result<RgbImage> {
        let resized = resize_rgb(face, RESTORE_SIZE as u32, RESTORE_SIZE as u32);
        let mut input = Array4::<f32>::zeros((1, 3, RESTORE_SIZE, RESTORE_SIZE));
        for y in 0..RESTORE_SIZE {
            for x in 0..RESTORE_SIZE {
                let p = resized.get_pixel(x as u32, y as u32).0;
                for c in 0..3 {
                    // [0,255] -> [-1,1]
                    input[[0, c, y, x]] = (p[c] as f32 / 255.0 - 0.5) / 0.5;
                }
            }
        }
        let n_in = self.session.inputs.len();
        let outputs = if matches!(self.kind, RestorerKind::CodeFormer) && n_in >= 2 {
            // CodeFormer fidelity weight: a scalar (double) second input.
            let w = Array1::<f64>::from_elem(1, self.fidelity_w as f64);
            self.session
                .run(ort::inputs![input, w].context("codeformer inputs")?)
                .context("codeformer inference")?
        } else {
            self.session
                .run(ort::inputs![input].context("face restore inputs")?)
                .context("face restore inference")?
        };
        let out_name = self
            .session
            .outputs
            .first()
            .map(|o| o.name.clone())
            .context("face restorer has no outputs")?;
        let t = outputs[out_name.as_str()]
            .try_extract_tensor::<f32>()
            .context("extracting face restorer output")?;
        let shape = t.shape().to_vec();
        anyhow::ensure!(
            shape.len() == 4 && shape[1] == 3,
            "face restorer output shape {shape:?} not [1,3,H,W]"
        );
        let (oh, ow) = (shape[2], shape[3]);
        let data: Vec<f32> = t.iter().copied().collect();
        let plane = oh * ow;
        let mut out = RgbImage::new(ow as u32, oh as u32);
        for y in 0..oh {
            for x in 0..ow {
                let mut px = [0u8; 3];
                for c in 0..3 {
                    // [-1,1] -> [0,255]
                    let v = data[c * plane + y * ow + x];
                    px[c] = ((v * 0.5 + 0.5) * 255.0).round().clamp(0.0, 255.0) as u8;
                }
                out.put_pixel(x as u32, y as u32, image::Rgb(px));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vision::face_embed::sharpness;

    fn gradient(w: u32, h: u32) -> RgbImage {
        let mut img = RgbImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let v = ((x * 256 / w.max(1)) as u8).wrapping_add((y * 7) as u8);
                img.put_pixel(x, y, image::Rgb([v, v, v]));
            }
        }
        img
    }

    #[test]
    fn crop_with_margin_expands_and_clamps() {
        let frame = gradient(100, 80);
        // A box in the middle expands by margin but stays within bounds.
        let (crop, off) = crop_with_margin(&frame, &[40.0, 30.0, 20.0, 20.0], 0.5);
        // 0.5 margin => +10px each side => 40px wide, 40px tall, origin (30,20).
        assert_eq!(off, [30.0, 20.0]);
        assert_eq!(crop.width(), 40);
        assert_eq!(crop.height(), 40);
        // A box at the corner clamps the origin to 0.
        let (_c2, off2) = crop_with_margin(&frame, &[0.0, 0.0, 10.0, 10.0], 1.0);
        assert_eq!(off2, [0.0, 0.0]);
    }

    #[test]
    fn deskew_zero_is_near_identity() {
        let img = gradient(32, 32);
        let out = deskew(&img, 0.0);
        let mut diff = 0i64;
        for (a, b) in img.pixels().zip(out.pixels()) {
            diff += (a.0[0] as i64 - b.0[0] as i64).abs();
        }
        assert!(diff < 32 * 32, "zero deskew should be ~identity, diff={diff}");
    }

    #[test]
    fn unsharp_increases_sharpness() {
        // Blur a gradient, then unsharp it: variance-of-Laplacian should rise.
        let img = gradient(48, 48);
        let blurred = image::imageops::blur(&img, 2.0);
        let before = sharpness(&blurred);
        let after = sharpness(&unsharp_mask(&blurred, 1.5, 2.0));
        assert!(after >= before, "unsharp should not reduce sharpness");
    }

    #[test]
    fn homography_identity_recovers_region() {
        // Corners == an axis-aligned rectangle => warp ≈ crop+resize of that rectangle.
        let frame = gradient(64, 64);
        let corners = [[8.0, 8.0], [40.0, 8.0], [40.0, 40.0], [8.0, 40.0]];
        let out = homography_warp(&frame, &corners, 32, 32);
        assert_eq!(out.width(), 32);
        assert_eq!(out.height(), 32);
        // Top-left output pixel should resemble the source near (8,8).
        let want = frame.get_pixel(8, 8).0[0] as i32;
        let got = out.get_pixel(0, 0).0[0] as i32;
        assert!((want - got).abs() < 24, "want≈{want}, got={got}");
    }

    #[test]
    fn clahe_preserves_size_and_spreads_contrast() {
        // A low-contrast image (values clustered) should gain spread after CLAHE.
        let mut img = RgbImage::new(40, 40);
        for (i, p) in img.pixels_mut().enumerate() {
            let v = 120 + (i % 8) as u8; // narrow band 120..127
            *p = image::Rgb([v, v, v]);
        }
        let out = clahe_gray(&img);
        assert_eq!(out.dimensions(), img.dimensions());
        let span = |im: &RgbImage| {
            let (mut lo, mut hi) = (255i32, 0i32);
            for p in im.pixels() {
                lo = lo.min(p.0[0] as i32);
                hi = hi.max(p.0[0] as i32);
            }
            hi - lo
        };
        assert!(
            span(&out) >= span(&img),
            "CLAHE should not shrink the value span"
        );
    }

    #[test]
    fn detector_kind_parse() {
        assert_eq!("scrfd".parse::<DetectorKind>().unwrap(), DetectorKind::Scrfd);
        assert_eq!("YuNet".parse::<DetectorKind>().unwrap(), DetectorKind::YuNet);
        assert!("nope".parse::<DetectorKind>().is_err());
    }

    #[test]
    fn restorer_kind_parse() {
        assert_eq!(
            "gfpgan".parse::<RestorerKind>().unwrap(),
            RestorerKind::Gfpgan
        );
        assert_eq!(
            "CodeFormer".parse::<RestorerKind>().unwrap(),
            RestorerKind::CodeFormer
        );
        assert!("nope".parse::<RestorerKind>().is_err());
    }
}
