//! Plate rectification + enhancement — the ALPR "clean up + zoom" core. A tilted/distorted plate is
//! perspective-warped to a fronto-parallel canonical rectangle (when the detector gave 4 corners),
//! then optionally super-resolved and contrast-normalized so the OCR reads clean glyphs.

use image::RgbImage;

use super::detect::PlateBox;
use crate::vision::enhance::{self, Upscaler};

/// Canonical single-line plate size fed to OCR (the rectifier output).
pub const PLATE_W: u32 = 256;
pub const PLATE_H: u32 = 64;

/// Rectify a detected plate from the frame to a `dst_w × dst_h` fronto-parallel image. With 4
/// corners we solve a homography (true deskew); with only a bbox we crop + resize (axis-aligned).
pub fn rectify(frame: &RgbImage, plate: &PlateBox, dst_w: u32, dst_h: u32) -> RgbImage {
    match &plate.corners {
        Some(c) => enhance::homography_warp(frame, &order_corners(c), dst_w, dst_h),
        None => {
            let (crop, _) = enhance::crop_with_margin(frame, &plate.bbox, 0.05);
            enhance::resize_rgb(&crop, dst_w, dst_h)
        }
    }
}

/// Clean up a rectified plate for OCR: optional super-resolution for small plates, then CLAHE local
/// contrast + a mild unsharp. Grayscale output (plate OCR is glyph-shape, not color).
pub fn enhance_plate(img: &RgbImage, upscaler: Option<&Upscaler>, sr_min_side: f32) -> RgbImage {
    let upscaled;
    let src = match upscaler {
        Some(up) if (img.width().min(img.height()) as f32) < sr_min_side => {
            match up.upscale(img) {
                Ok(big) => {
                    upscaled = big;
                    &upscaled
                }
                Err(e) => {
                    tracing::warn!(error = %e, "plate super-res failed; using rectified crop");
                    img
                }
            }
        }
        _ => img,
    };
    let gray = enhance::clahe_gray(src);
    enhance::unsharp_mask(&gray, 0.8, 1.0)
}

/// Order 4 unordered quad corners as [top-left, top-right, bottom-right, bottom-left] (the
/// convention `enhance::homography_warp` expects). TL = min(x+y), BR = max(x+y), TR = max(x−y),
/// BL = min(x−y) — the standard OpenCV ordering.
fn order_corners(c: &[[f32; 2]; 4]) -> [[f32; 2]; 4] {
    let arg = |f: &dyn Fn([f32; 2]) -> f32, want_max: bool| -> [f32; 2] {
        let mut best = c[0];
        let mut bv = f(c[0]);
        for &p in &c[1..] {
            let v = f(p);
            if (want_max && v > bv) || (!want_max && v < bv) {
                bv = v;
                best = p;
            }
        }
        best
    };
    let sum = |p: [f32; 2]| p[0] + p[1];
    let diff = |p: [f32; 2]| p[0] - p[1];
    [
        arg(&sum, false),  // TL
        arg(&diff, true),  // TR
        arg(&sum, true),   // BR
        arg(&diff, false), // BL
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_corners_canonicalizes() {
        // Shuffled corners of a 100×40 rectangle.
        let shuffled = [[100.0, 40.0], [0.0, 0.0], [0.0, 40.0], [100.0, 0.0]];
        let o = order_corners(&shuffled);
        assert_eq!(o[0], [0.0, 0.0]); // TL
        assert_eq!(o[1], [100.0, 0.0]); // TR
        assert_eq!(o[2], [100.0, 40.0]); // BR
        assert_eq!(o[3], [0.0, 40.0]); // BL
    }
}
