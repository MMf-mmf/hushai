#!/usr/bin/env bash
# Fetch the CLIP BPE tokenizer that hushai-rag's CLIP TEXT tower uses to turn a query phrase
# ("when did I see a car") into the [1,77] int64 token ids the exported text graph expects.
#
# WHY (see AGENTS.md "vision"): the text tower is exported from OpenCLIP ViT-B/32 (openai weights)
# by local_dev/export_clip.py. Its tokenization MUST match OpenAI CLIP's BPE exactly, or the text
# embeddings land in the wrong place and "a car" retrieves nothing. The HF `openai/clip-vit-base-
# patch32` tokenizer.json is that exact BPE (vocab 49408, BOS 49406, EOS 49407, context length 77),
# loadable directly by the Rust `tokenizers` crate. The decisive cross-modal test
# (clip_text_matches_image_on_fixtures) confirms the pairing end-to-end.
#
# Gitignored under models/ — run once after a fresh checkout; offline at runtime thereafter.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/models/clip_tokenizer.json"
URL="https://huggingface.co/openai/clip-vit-base-patch32/resolve/main/tokenizer.json"
# Fallbacks if tokenizer.json isn't served (older repos only ship vocab.json + merges.txt):
VOCAB_URL="https://huggingface.co/openai/clip-vit-base-patch32/resolve/main/vocab.json"
MERGES_URL="https://huggingface.co/openai/clip-vit-base-patch32/resolve/main/merges.txt"

if [ -f "$DEST" ]; then
  echo "CLIP tokenizer already present at $DEST — nothing to do."
  echo "(delete the file to force a re-download)"
  echo "sha256:"; shasum -a 256 "$DEST"
  exit 0
fi

mkdir -p "$ROOT/models"
echo "Downloading CLIP tokenizer.json from openai/clip-vit-base-patch32…"
if curl -fSL -o "$DEST" "$URL"; then
  echo "Done:"
  ls -lh "$DEST"
  echo "sha256:"; shasum -a 256 "$DEST"
else
  echo "tokenizer.json not available; falling back to vocab.json + merges.txt." >&2
  curl -fSL -o "$ROOT/models/clip_vocab.json" "$VOCAB_URL"
  curl -fSL -o "$ROOT/models/clip_merges.txt" "$MERGES_URL"
  echo "Fetched clip_vocab.json + clip_merges.txt. NOTE: clip_text.rs prefers tokenizer.json;" >&2
  echo "build one with the 'tokenizers' CLI or huggingface, or point CLIP_TOKENIZER_PATH at it." >&2
  exit 1
fi
