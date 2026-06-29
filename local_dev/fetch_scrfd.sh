#!/usr/bin/env bash
# Fetch the SCRFD-10GF face detector (InsightFace `buffalo_l/det_10g.onnx`) for the worker's default
# face detector (hushai-worker/src/vision/detect_scrfd.rs). SCRFD has materially better small/distant
# face recall than YuNet and emits the SAME 5-point landmarks ArcFace's buffalo_l pack was tuned with.
#
# The `buffalo_l` analysis pack ships SCRFD-10GF-with-keypoints as `det_10g.onnx`; we extract just
# that file to models/scrfd_10g_bnkps.onnx (the worker default FACE_SCRFD_MODEL_PATH). After fetching,
# VALIDATE the decode against the real export:
#   cargo test -p hushai-worker --test vision_pipeline inspect_enhance_model_io_shapes -- --nocapture
#
# LICENSE: InsightFace models are released for NON-COMMERCIAL research use (same posture already
# accepted for ArcFace w600k_r50). Weights stay gitignored under models/; the code path is MIT.
# Gitignored under models/ — run once after checkout; fully offline at runtime thereafter.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/models/scrfd_10g_bnkps.onnx"
PACK_URL="https://github.com/deepinsight/insightface/releases/download/v0.7/buffalo_l.zip"

if [ -f "$DEST" ]; then
  echo "SCRFD detector already present at: $DEST"
  echo "(delete it to force a re-download)"
  exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo "Downloading InsightFace buffalo_l pack (~280 MB) for det_10g.onnx (SCRFD-10GF)…"
curl -fSL -o "$TMP/buffalo_l.zip" "$PACK_URL"
echo "Extracting det_10g.onnx…"
unzip -o -j "$TMP/buffalo_l.zip" '*det_10g.onnx' -d "$TMP" >/dev/null
SRC="$(find "$TMP" -name 'det_10g.onnx' | head -1)"
if [ -z "$SRC" ]; then
  echo "ERROR: det_10g.onnx not found in the pack — adjust the glob or source URL." >&2
  exit 1
fi
mkdir -p "$ROOT/models"
mv "$SRC" "$DEST"
echo "sha256: $(shasum -a 256 "$DEST" | awk '{print $1}')  (pin this once stable)"
echo "Wrote $DEST"
echo "Set FACE_DETECTOR_KIND=scrfd (default) to use it; validate with inspect_enhance_model_io_shapes."
