#!/usr/bin/env bash
# Fetch the Vosk models the hushai-android voice assistant bundles as APK assets.
# These are large binaries (~82 MB) and are gitignored — run this once after a
# fresh checkout so `:app:assembleDebug` can package them.
#
#   assets/vosk/model-en   = vosk-model-small-en-us-0.15  (acoustic + STT + wake word)
#   assets/vosk/model-spk  = vosk-model-spk-0.4           (speaker x-vectors for owner ID)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ASSETS="$ROOT/hushai-android/app/src/main/assets/vosk"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

EN_URL="https://alphacephei.com/vosk/models/vosk-model-small-en-us-0.15.zip"
SPK_URL="https://alphacephei.com/vosk/models/vosk-model-spk-0.4.zip"

mkdir -p "$ASSETS"
echo "Downloading Vosk acoustic model…"
curl -fSL -o "$TMP/en.zip" "$EN_URL"
echo "Downloading Vosk speaker model…"
curl -fSL -o "$TMP/spk.zip" "$SPK_URL"

echo "Unpacking…"
( cd "$TMP" && unzip -q -o en.zip && unzip -q -o spk.zip )
rm -rf "$ASSETS/model-en" "$ASSETS/model-spk"
mv "$TMP/vosk-model-small-en-us-0.15" "$ASSETS/model-en"
mv "$TMP/vosk-model-spk-0.4" "$ASSETS/model-spk"

echo "Done:"
du -sh "$ASSETS/model-en" "$ASSETS/model-spk"
