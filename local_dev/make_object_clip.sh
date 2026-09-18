#!/usr/bin/env bash
# Synthesize object_clip.mp4 — a short video that RELIABLY contains a COCO object — so the worker's
# object-lane decode + the cross-modal CLIP test have a deterministic fixture (the always-on capture
# corpus is mostly empty desks; AGENTS.md notes faces/objects are often absent there).
#
# Mirrors the build_demo.sh convention: generated media, gitignored, operator-made.
# It's a slow "ken-burns" pan/zoom over a still image (so multiple sampled frames all contain the
# object), encoded H.264/yuv420p like the real capture client's segments.
#
# LICENSING (operator-provisioned, like the models): you supply the still image. Pass a path, or
# drop one at models/fixtures/object_still.jpg. Use a CC0/public-domain photo of a clear COCO object
# (a car is the canonical choice — it's what the test prompts compare). Good CC0 sources: Wikimedia
# Commons "PD" / "CC0" car photos, or any photo you own. We deliberately DON'T auto-download an image
# to avoid baking a licensing assumption into the repo.
#
# Usage:
#   bash local_dev/make_object_clip.sh path/to/car.jpg
#   bash local_dev/make_object_clip.sh                 # uses models/fixtures/object_still.jpg
#   OBJECT_CLIP_OUT=/tmp/car.mp4 bash local_dev/make_object_clip.sh car.jpg
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STILL="${1:-$ROOT/models/fixtures/object_still.jpg}"
OUT="${OBJECT_CLIP_OUT:-$ROOT/object_clip.mp4}"
FFMPEG="${FFMPEG_BIN:-ffmpeg}"
SECONDS_LEN="${OBJECT_CLIP_SECS:-6}"
FPS=25

if [ -f "$OUT" ]; then
  echo "$OUT already present — nothing to do (delete it to regenerate)."
  exit 0
fi

if [ ! -f "$STILL" ]; then
  echo "ERROR: still image not found at: $STILL" >&2
  echo "  Provide one:  bash local_dev/make_object_clip.sh /path/to/car.jpg" >&2
  echo "  or place a CC0 photo of a COCO object at models/fixtures/object_still.jpg" >&2
  exit 1
fi

if ! command -v "$FFMPEG" >/dev/null 2>&1; then
  echo "ERROR: ffmpeg not found (set FFMPEG_BIN)." >&2
  exit 1
fi

echo "Building $OUT from $STILL (${SECONDS_LEN}s ken-burns, H.264)…"
TOTAL_FRAMES=$(( SECONDS_LEN * FPS ))
# Scale the still up, then zoompan slowly across it so every sampled frame contains the object;
# output 1280x720 yuv420p H.264 (matches the capture client's segment codec).
"$FFMPEG" -nostdin -v error -y \
  -loop 1 -i "$STILL" \
  -vf "scale=2560:1440:force_original_aspect_ratio=increase,crop=2560:1440,zoompan=z='min(zoom+0.0008,1.3)':d=${TOTAL_FRAMES}:s=1280x720:fps=${FPS},format=yuv420p" \
  -t "$SECONDS_LEN" -c:v libx264 -preset veryfast -pix_fmt yuv420p \
  "$OUT"

echo "Done:"
ls -lh "$OUT"
echo "Validate the object lane against it:"
echo "  OBJECT_TEST_MP4=$OUT cargo test -p hushai-worker --test vision_pipeline detect_objects_from_real_video -- --nocapture"
