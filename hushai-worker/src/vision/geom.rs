//! Shared 2D geometry helpers for the vision lanes: IoU + greedy non-max suppression.
//!
//! Extracted from `detect.rs` so the YuNet face detector, the SCRFD face detector, and the
//! license-plate detector all share ONE NMS implementation (and so the plate lane can NMS its own
//! `PlateBox` type without re-deriving the math). `nms_by` is generic over the candidate type via
//! bbox/score accessors; boxes are `[x, y, w, h]` (top-left + size).

/// IoU of two `[x, y, w, h]` boxes. 0 when disjoint or degenerate.
pub fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
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

/// Greedy non-max suppression by descending score. Keeps a candidate iff it does not overlap any
/// already-kept candidate above `iou_thresh`. Generic over `T` via `bbox`/`score` accessors so faces,
/// plates, and objects can all reuse it. Returns the kept candidates, highest score first.
pub fn nms_by<T>(
    mut items: Vec<T>,
    iou_thresh: f32,
    bbox: impl Fn(&T) -> [f32; 4],
    score: impl Fn(&T) -> f32,
) -> Vec<T> {
    items.sort_by(|a, b| {
        score(b)
            .partial_cmp(&score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut keep: Vec<T> = Vec::new();
    for it in items {
        let bb = bbox(&it);
        if keep.iter().all(|k| iou(&bbox(k), &bb) <= iou_thresh) {
            keep.push(it);
        }
    }
    keep
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
        // (bbox, score) candidates: two heavily overlapping (keep higher) + one far away (keep).
        let cands = vec![
            ([0.0, 0.0, 10.0, 10.0], 0.9f32),
            ([1.0, 0.0, 10.0, 10.0], 0.8),
            ([50.0, 0.0, 10.0, 10.0], 0.7),
        ];
        let out = nms_by(cands, 0.3, |c| c.0, |c| c.1);
        assert_eq!(out.len(), 2);
        assert!((out[0].1 - 0.9).abs() < 1e-6);
    }
}
