//! Cross-segment motion gate: skip the (expensive) vision model pipeline on a camera whose scene
//! hasn't changed since we last analyzed it. A static camera staring at an empty hallway re-runs
//! face/object/plate detection on every ~2s segment otherwise — the single biggest source of
//! wasted vision compute.
//!
//! Each camera (`device_id`) keeps one tiny fingerprint of its last-analyzed frame: an NxN
//! grayscale tile + its mean. The distance between two fingerprints is a mean-subtracted MSE —
//! identical frames score ~0; a person/vehicle entering spikes it. Subtracting each tile's own
//! mean (DC removal) makes it robust to UNIFORM brightness drift (IR illuminator / auto-exposure
//! ramps at night) while still catching structural change. Intra-segment diff is too weak with
//! only ~2 frames per segment, so the comparison is deliberately CROSS-segment, keyed by camera.
//!
//! Pure + tiny so it unit-tests with no models (like `vad`/`face_embed::sharpness`).

use super::enhance;
use image::RgbImage;

/// A camera's last-analyzed-frame fingerprint: an NxN grayscale tile plus its mean (the DC term
/// subtracted before comparison). ~1 KB per camera at the default 32x32.
#[derive(Debug, Clone)]
pub struct Fingerprint {
    tile: Vec<u8>,
    mean: f32,
}

/// Downscale `img` to `side`x`side` grayscale (BT.601 luma) and capture its mean. `side` is
/// clamped to >= 1. Cheap: a Lanczos resize to a tiny tile then one luma pass.
pub fn fingerprint(img: &RgbImage, side: usize) -> Fingerprint {
    let side = side.max(1) as u32;
    let small = enhance::resize_rgb(img, side, side);
    let mut tile = Vec::with_capacity((side * side) as usize);
    let mut sum: f64 = 0.0;
    for p in small.pixels() {
        let [r, g, b] = p.0;
        let y = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
        let yv = y.round().clamp(0.0, 255.0) as u8;
        tile.push(yv);
        sum += yv as f64;
    }
    let mean = if tile.is_empty() {
        0.0
    } else {
        (sum / tile.len() as f64) as f32
    };
    Fingerprint { tile, mean }
}

/// Mean-subtracted mean-squared-error between two fingerprints (0 = structurally identical). Each
/// tile is centered on its own mean first, so a uniform brightness shift contributes ~0. Returns
/// `f32::MAX` ("changed") if the tiles differ in size (e.g. a resolution change) or are empty.
pub fn distance(a: &Fingerprint, b: &Fingerprint) -> f32 {
    if a.tile.len() != b.tile.len() || a.tile.is_empty() {
        return f32::MAX;
    }
    let mut sum_sq: f64 = 0.0;
    for (x, y) in a.tile.iter().zip(b.tile.iter()) {
        let da = *x as f32 - a.mean;
        let db = *y as f32 - b.mean;
        let d = (da - db) as f64;
        sum_sq += d * d;
    }
    (sum_sq / a.tile.len() as f64) as f32
}

/// True when `cur` is close enough to `prev` to skip vision inference (distance at/under
/// `threshold`). Pure decision so the gate's policy unit-tests without decoding video.
pub fn should_skip(prev: &Fingerprint, cur: &Fingerprint, threshold: f32) -> bool {
    distance(prev, cur) <= threshold
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    /// Solid-color WxH image.
    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(w, h, Rgb(rgb))
    }

    #[test]
    fn identical_frames_have_zero_distance_and_skip() {
        let img = solid(160, 120, [40, 80, 120]);
        let a = fingerprint(&img, 32);
        let b = fingerprint(&img, 32);
        assert!(distance(&a, &b) < 1e-3, "identical => ~0, got {}", distance(&a, &b));
        assert!(should_skip(&a, &b, 8.0));
    }

    #[test]
    fn uniform_brightness_shift_is_absorbed() {
        // Every pixel +30: mean subtraction should make this read as ~no structural change.
        let dark = solid(160, 120, [40, 40, 40]);
        let bright = solid(160, 120, [70, 70, 70]);
        let a = fingerprint(&dark, 32);
        let b = fingerprint(&bright, 32);
        assert!(distance(&a, &b) < 1.0, "brightness ramp should be ~0, got {}", distance(&a, &b));
        assert!(should_skip(&a, &b, 8.0));
    }

    #[test]
    fn structural_change_is_detected_and_not_skipped() {
        // A bright square dropped into one quadrant (someone walks in) is a structural change.
        let base = solid(160, 120, [40, 40, 40]);
        let mut changed = base.clone();
        for y in 0..60 {
            for x in 0..80 {
                changed.put_pixel(x, y, Rgb([220, 220, 220]));
            }
        }
        let a = fingerprint(&base, 32);
        let b = fingerprint(&changed, 32);
        assert!(distance(&a, &b) > 8.0, "structural change should exceed threshold, got {}", distance(&a, &b));
        assert!(!should_skip(&a, &b, 8.0));
    }

    #[test]
    fn different_tile_sizes_never_skip() {
        let img = solid(160, 120, [10, 20, 30]);
        let a = fingerprint(&img, 32);
        let b = fingerprint(&img, 16);
        assert_eq!(distance(&a, &b), f32::MAX);
        assert!(!should_skip(&a, &b, 8.0));
    }
}
