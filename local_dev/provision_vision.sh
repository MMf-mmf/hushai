#!/usr/bin/env bash
#
# provision_vision.sh — one-time provisioning of the object-detection + plate-OCR ONNX weights
# the hushai-eval vision fixtures need. Uses an isolated venv that REUSES host torch/cv2/numpy
# (--system-site-packages) so we don't reinstall torch or pollute the global env. Each step is
# best-effort: a failing package/export is logged and the script continues. Runtime inference is
# pure ONNX (no Python), so only the exported .onnx files matter afterward.
#
# Plate DETECTOR (lp_detector.onnx) has no public default source — set PLATE_DETECTOR_ONNX_URL to a
# model you choose (licensing is your call) and fetch_plate_detector.sh will grab it; skipped here.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENV="$ROOT/local_dev/.venv-vision"
PY="$VENV/bin/python"
PIP="$VENV/bin/pip"
log() { echo "[provision $(date +%H:%M:%S)] $*"; }

log "creating venv (--system-site-packages, reuses host torch) at $VENV"
python3 -m venv --system-site-packages "$VENV" || { log "FATAL: venv create failed"; exit 1; }
"$PIP" install --quiet --upgrade pip 2>&1 | tail -2

# Install the export-time deps (best effort, one at a time so a failure is attributable).
for pkg in onnx open_clip_torch rfdetr fast-plate-ocr; do
  log "pip install $pkg ..."
  if "$PIP" install --quiet "$pkg" 2>&1 | tail -3; then
    log "  $pkg OK"
  else
    log "  $pkg FAILED (continuing)"
  fi
done

cd "$ROOT"
run_export() {  # name script outfile
  local name="$1" script="$2" out="$3"
  log "export $name: $script"
  if "$PY" "$script" 2>&1 | tail -6; then
    [[ -f "$out" ]] && log "  $name OK -> $out ($(du -h "$out" | cut -f1))" || log "  $name ran but $out missing"
  else
    log "  $name export FAILED (continuing)"
  fi
}
run_export "rf-detr (objects)" local_dev/export_rf_detr.py models/rf-detr-nano.onnx
run_export "clip (objects embed)" local_dev/export_clip.py models/clip_vit_b32_image.onnx
run_export "plate-ocr" local_dev/export_plate_ocr.py models/lp_ocr_cct.onnx

if [[ -n "${PLATE_DETECTOR_ONNX_URL:-}" ]]; then
  log "fetching plate detector from PLATE_DETECTOR_ONNX_URL"
  PLATE_DETECTOR_ONNX_URL="$PLATE_DETECTOR_ONNX_URL" bash local_dev/fetch_plate_detector.sh 2>&1 | tail -4
else
  log "plate DETECTOR skipped (set PLATE_DETECTOR_ONNX_URL to a model source to enable ALPR)"
fi

log "=== provisioning summary (models/) ==="
for f in rf-detr-nano.onnx rf-detr-classes.json clip_vit_b32_image.onnx clip_vit_b32_text.onnx \
         lp_ocr_cct.onnx lp_ocr_charset.json lp_detector.onnx; do
  if [[ -f "models/$f" ]]; then echo "  PRESENT  $f"; else echo "  missing  $f"; fi
done
log "done."
