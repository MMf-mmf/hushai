#!/usr/bin/env bash
# Fetch / export a license-plate DETECTOR ONNX for the worker's ALPR lane
# (hushai-worker/src/vision/plates/detect.rs). The detector runs inside each vehicle ROI (RF-DETR
# car/truck/bus/motorcycle) and emits plate boxes — ideally with 4 corner keypoints so the plate can
# be perspective-RECTIFIED (deskewed) before OCR.
#
# detect.rs is a defensive YOLOv8/YOLO11-style decoder: it reads the largest 2-D output [C,N]/[N,C],
# takes channels 0..4 as cx,cy,w,h (model-input px), channel 4 as confidence, and — when there are
# >=8 trailing channels — the next four (x,y[,vis]) groups as plate corners. Validate the real export:
#   cargo test -p hushai-worker --test vision_pipeline inspect_plate_model_io_shapes -- --nocapture
#
# Default PLATE_DETECT_MODEL_PATH = models/lp_detector.onnx. A 4-keypoint (pose) plate model enables
# true rectification; a bbox-only model still works (axis-aligned crop, no deskew).
#
# LICENSE: prefer a permissively-licensed plate detector. Ultralytics-derived weights are AGPL — fine
# for personal/research, a copyleft obligation for a shipped product (this repo's posture is
# best-accuracy-any-license: weights gitignored under models/, code path MIT). Document your source.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/models/lp_detector.onnx"
URL="${PLATE_DETECTOR_ONNX_URL:-}"

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
