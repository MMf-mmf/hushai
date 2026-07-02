#!/usr/bin/env bash
# Fetch / export a license-plate DETECTOR ONNX for the worker's ALPR lane
# (hushai-worker/src/vision/plates/detect.rs). The detector runs inside each vehicle ROI (RF-DETR
# car/truck/bus/motorcycle) and emits plate boxes — ideally with 4 corner keypoints so the plate can
# be perspective-RECTIFIED (deskewed) before OCR.
#
# DEFAULT MODEL (2026-07-01): open-image-models `yolo-v9-t-640-license-plate` — a lightweight YOLOv9-t
# plate detector, **MIT-licensed** (clean for a shipped product), that pairs natively with our
# fast-plate-ocr CCT recognizer (same author, ankandrew). Verified end-to-end: detects at conf ~0.90,
# reads real plates correctly. It is an END2END export: output `[N,7] = [batch,x1,y1,x2,y2,class,score]`
# (NMS baked in) — so run the worker with PLATE_DETECT_END2END=true (the default). It is bbox-only (no
# corner keypoints → axis-aligned crop, no deskew), which the fast-plate-ocr CCT handles fine.
#
# detect.rs decodes BOTH end2end ([N,7] xyxy) and raw YOLOv8/11 ([C,N] cxcywh [+ 4 corner keypoints];
# set PLATE_DETECT_END2END=false). A 4-keypoint pose model would additionally enable rectification.
# Validate any model against the real export:
#   cargo test -p hushai-worker --test vision_pipeline inspect_plate_model_io_shapes -- --nocapture
#
# Default PLATE_DETECT_MODEL_PATH = models/lp_detector.onnx. Override PLATE_DETECTOR_ONNX_URL to fetch
# a different model (e.g. a larger yolo-v9-s-608, or an AGPL Ultralytics export — your licensing call).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/models/lp_detector.onnx"
# MIT-licensed default; a plain ONNX (no pickle) so it's safe to fetch + load.
DEFAULT_URL="https://github.com/ankandrew/open-image-models/releases/download/assets/yolo-v9-t-640-license-plates-end2end.onnx"
URL="${PLATE_DETECTOR_ONNX_URL:-$DEFAULT_URL}"

if [ -f "$DEST" ]; then
  echo "Plate detector already present at: $DEST"
  exit 0
fi
if [ -z "$URL" ]; then
  cat >&2 <<'EOF'
Provision a license-plate detector ONNX one of two ways:

1) Fetch a pre-exported ONNX:
     PLATE_DETECTOR_ONNX_URL=https://.../lp_detector.onnx local_dev/fetch_plate_detector.sh

2) Export from an Ultralytics LP model (bbox or, preferred, a 4-keypoint pose model):
     pip install ultralytics
     yolo export model=license-plate-detector.pt format=onnx opset=17 imgsz=640
     mv license-plate-detector.onnx models/lp_detector.onnx

Then validate the decode: cargo test -p hushai-worker --test vision_pipeline \
    inspect_plate_model_io_shapes -- --nocapture
The plate lane is OPTIONAL: without this (or the OCR model) the worker self-disables ALPR and faces +
objects still run.
EOF
  exit 1
fi

echo "Downloading plate detector ONNX from $URL …"
mkdir -p "$ROOT/models"
curl -fSL -o "$DEST" "$URL"
echo "sha256: $(shasum -a 256 "$DEST" | awk '{print $1}')  (pin this once stable)"
echo "Wrote $DEST"
