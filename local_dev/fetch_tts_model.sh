#!/usr/bin/env bash
# Fetch the Kokoro-82M TTS model bundle that hushai-rag uses to synthesize the
# assistant's spoken answers (a natural neutral voice, generated on the backend).
#
# It's a large binary (~330 MB model.onnx) and is gitignored under models/ — run
# this once after a fresh checkout, then point the RAG service at it with
#   RAG_TTS_DIR=models/kokoro-en-v0_19
#
# Bundle contents (sherpa-onnx `kokoro-en-v0_19`, Apache-2.0, 11 English speakers):
#   model.onnx        ~330 MB   the Kokoro neural TTS model
#   voices.bin        ~5.5 MB   per-speaker style vectors
#   tokens.txt                  phoneme/token table
#   espeak-ng-data/             phonemizer data (text -> phonemes)
#   *.fst / lexicon*  (optional) text-normalization + lexicons
#
# Speaker ids (set RAG_TTS_SID): neutral American male is am_michael (sid 6);
# am_adam (sid 5) is an alternative. British males are bm_george/bm_lewis (9/10).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/models/kokoro-en-v0_19"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kokoro-en-v0_19.tar.bz2"

if [ -f "$DEST/model.onnx" ]; then
  echo "Kokoro model already present at $DEST — nothing to do."
  echo "(delete the directory to force a re-download)"
  exit 0
fi

echo "Downloading Kokoro TTS bundle (~330 MB)…"
curl -fSL -o "$TMP/kokoro.tar.bz2" "$URL"

echo "Unpacking…"
tar -xjf "$TMP/kokoro.tar.bz2" -C "$TMP"
rm -rf "$DEST"
mkdir -p "$ROOT/models"
mv "$TMP/kokoro-en-v0_19" "$DEST"

echo "Done:"
du -sh "$DEST"
ls -1 "$DEST"
