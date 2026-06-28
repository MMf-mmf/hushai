#!/usr/bin/env bash
# Fetch the ONNX Runtime dylib that the vision worker's `ort` crate dlopen()s at runtime.
#
# WHY a separate dylib (see AGENTS.md "vision ONNX runtime"): the worker already links sherpa-rs,
# which STATICALLY bundles its own libonnxruntime 1.17.1. `ort` (ort-sys rc.9) targets ONNX Runtime
# 1.20.0 — a different version — so it CANNOT reuse sherpa's dylib. We build `ort` with the
# `load-dynamic` feature (no link-time onnxruntime) and point it at THIS 1.20 dylib via
# ORT_DYLIB_PATH. The two onnxruntimes are distinct Mach-O images and coexist under macOS two-level
# namespaces (proven by hushai-worker/tests/ort_coexistence.rs).
#
# Gitignored under models/ — run once after a fresh checkout. Default target is macOS arm64; adjust
# ORT_OS/ORT_ARCH for other hosts. Runtime is fully offline once fetched.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ORT_VERSION="1.20.0"
ORT_ASSET="onnxruntime-osx-arm64-${ORT_VERSION}"   # Microsoft official release asset (macOS arm64)
DEST_DIR="$ROOT/models/onnxruntime"
DYLIB="$DEST_DIR/${ORT_ASSET}/lib/libonnxruntime.${ORT_VERSION}.dylib"
URL="https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/${ORT_ASSET}.tgz"
# sha256 of the official ${ORT_ASSET}.tgz (verify on download):
EXPECTED_TGZ_SHA="2bcfaafa9ff0a3a94f78e3af2f135ffde5bb2d79b08e83a50dbc450b0d20ddae"

if [ -f "$DYLIB" ]; then
  echo "ONNX Runtime ${ORT_VERSION} dylib already present at:"
  echo "  $DYLIB"
  echo "(delete models/onnxruntime to force a re-download)"
  exit 0
fi

echo "Downloading ONNX Runtime ${ORT_VERSION} (${ORT_ASSET}, ~7.5 MB)…"
mkdir -p "$DEST_DIR"
TGZ="$DEST_DIR/${ORT_ASSET}.tgz"
curl -fSL -o "$TGZ" "$URL"

echo "Verifying sha256…"
GOT_SHA="$(shasum -a 256 "$TGZ" | awk '{print $1}')"
if [ "$GOT_SHA" != "$EXPECTED_TGZ_SHA" ]; then
  echo "ERROR: sha256 mismatch for $TGZ" >&2
  echo "  expected $EXPECTED_TGZ_SHA" >&2
  echo "  got      $GOT_SHA" >&2
  exit 1
fi

tar -xzf "$TGZ" -C "$DEST_DIR"
echo "Done. Point the worker at it (this is the default ORT_DYLIB_PATH):"
echo "  ORT_DYLIB_PATH=$DYLIB"
ls -lh "$DYLIB"
