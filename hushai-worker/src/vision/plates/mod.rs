//! License-plate recognition (ALPR) lane. Runs after vehicle detection (RF-DETR): for each car /
//! truck / bus / motorcycle ROI we ZOOM IN (crop the vehicle), detect the plate, RECTIFY it to a
//! fronto-parallel image, super-resolve + contrast-normalize, OCR it, vote across frames, and
//! match-or-mint into the `license_plates` catalog by normalized string.
//!
//! Mirrors the face lane's module shape: `detect` (plate boxes/corners), `rectify` (the clean-up
//! core), `ocr` (recognizer), `normalize` (pure string/vote/quality), `plate_match` (catalog). The
//! whole lane is OPTIONAL + NON-FATAL: it needs RF-DETR + a plate detector + a plate OCR model; any
//! missing model self-disables it while faces + objects keep running.

pub mod detect;
pub mod normalize;
pub mod ocr;
pub mod plate_match;
pub mod rectify;

/// COCO vehicle classes RF-DETR emits that can carry a plate.
pub fn is_vehicle(label: &str) -> bool {
    matches!(label, "car" | "truck" | "bus" | "motorcycle")
}
