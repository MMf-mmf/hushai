#!/usr/bin/env bash
# Fetch a Real-ESRGAN x4 super-resolution ONNX for the worker's cleanup stage (shared by the FACE
# lane — zooming tiny crops before restoration — and the PLATE lane — super-resolving small rectified
# plates before OCR). enhance.rs::Upscaler expects RGB NCHW input in [0,1] and an RGB NCHW output in
# [0,1]; the upscale factor is inferred from the output/input spatial ratio.
#
# Validate the real export's I/O with:
#   cargo test -p hushai-worker --test vision_pipeline inspect_enhance_model_io_shapes -- --nocapture
#
# Default FACE_UPSCALE_MODEL_PATH / PLATE super-res source = models/realesrgan_x4plus.onnx.
# LICENSE: Real-ESRGAN is BSD-3-Clause; weights gitignored under models/. Run once after checkout.
#
# NOTE: there is no single canonical Microsoft-style release asset for the Real-ESRGAN ONNX, so set
# REALESRGAN_ONNX_URL to a source you trust (a community export of RealESRGAN_x4plus), or export it
# yourself from the `realesrgan`/`basicsr` packages. The script verifies + prints the sha256.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/models/realesrgan_x4plus.onnx"
URL="${REALESRGAN_ONNX_URL:-}"

if [ -f "$DEST" ]; then
  echo "Real-ESRGAN ONNX already present at: $DEST"
  exit 0
fi
if [ -z "$URL" ]; then
  cat >&2 <<'EOF'
Set REALESRGAN_ONNX_URL to a RealESRGAN_x4plus ONNX export, e.g.:
  REALESRGAN_ONNX_URL=https://.../realesrgan_x4plus.onnx local_dev/fetch_realesrgan.sh

Or export it yourself (RGB NCHW [0,1] in/out, dynamic H/W):
  pip install realesrgan basicsr torch onnx
  # load RRDBNet x4 + RealESRGAN_x4plus.pth, torch.onnx.export with dynamic_axes on H,W.
Super-resolution is OPTIONAL: the worker self-disables the upscaler if this file is absent (faces are
restored from the Lanczos-resized crop; plates skip SR). Provision it for best small-object results.
EOF
  exit 1
fi

echo "Downloading Real-ESRGAN ONNX from $URL …"
mkdir -p "$ROOT/models"
curl -fSL -o "$DEST" "$URL"
echo "sha256: $(shasum -a 256 "$DEST" | awk '{print $1}')  (pin this once stable)"
echo "Wrote $DEST"
