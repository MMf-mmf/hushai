#!/usr/bin/env bash
# Fetch the Silero VAD ONNX model the worker uses to strip static/silence before computing
# speaker embeddings (the fix for static fragmenting one person into many "unknown" voices).
#
# Small (~2 MB) and gitignored under models/ — run once after a fresh checkout, then point
# the worker at it with VAD_MODEL_PATH=./models/silero_vad.onnx (the default). It runs in the
# SAME already-linked sherpa-onnx native lib as the TitaNet speaker model (no extra crate).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/models/silero_vad.onnx"
URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx"

if [ -f "$DEST" ]; then
  echo "Silero VAD model already present at $DEST — nothing to do."
  echo "(delete the file to force a re-download)"
  exit 0
fi

echo "Downloading Silero VAD model (~2 MB)…"
mkdir -p "$ROOT/models"
curl -fSL -o "$DEST" "$URL"

echo "Done:"
ls -lh "$DEST"
echo "sha256:"
shasum -a 256 "$DEST"
