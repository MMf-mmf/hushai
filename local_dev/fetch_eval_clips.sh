#!/usr/bin/env bash
#
# fetch_eval_clips.sh — regenerate the REAL-MEDIA eval fixtures' media from public-domain sources.
#
# The fixture ground truth (meta.json / expected.json) is committed and human-verified; the media
# itself is gitignored. This script re-fetches each source from Wikimedia Commons (public domain):
#   * AUDIO clips: trimmed to the ground-truth window + muxed over a tiny black video.
#   * VISION clips: a still PD photo ken-burns'd (slow pan/zoom so every sampled frame contains the
#     object) into a 1280x720 h264 + silent-aac MUXED clip — exercises the RF-DETR object lane.
# Both emit h264+aac MUXED segments via feed_segments.py, like build_fixtures.sh's synthetic clips.
# Run after a fresh checkout (or alongside ./local_dev/build_fixtures.sh).
#
# Usage: ./local_dev/fetch_eval_clips.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FX="$ROOT/hushai-eval/fixtures/train"
CACHE="$ROOT/hushai-eval/.work/clip-cache"
mkdir -p "$CACHE"

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing required tool: $1" >&2; exit 1; }; }
need curl; need ffmpeg

# case | source URL | ss (start secs) | t (duration secs)
CLIPS=(
  "jfk_moon|https://upload.wikimedia.org/wikipedia/commons/5/50/Jfk_rice_university_we_choose_to_go_to_the_moon.ogg|334|14"
  "fdr_infamy|https://upload.wikimedia.org/wikipedia/commons/7/7e/Roosevelt_Infamy.ogg|0|16"
  "armstrong_step|https://upload.wikimedia.org/wikipedia/commons/d/dd/Armstrong_Small_Step.ogg|0|24"
)

for row in "${CLIPS[@]}"; do
  IFS='|' read -r case url ss t <<<"$row"
  dest="$FX/$case"
  [[ -d "$dest" ]] || { echo "[skip] $case — no fixture dir (ground truth not committed?)"; continue; }
  src="$CACHE/$case.src"
  if [[ ! -s "$src" ]]; then
    echo "[fetch] $case ← $url"
    curl -fSL -m 180 -o "$src" "$url"
  fi
  echo "[mux] $case  (ss=$ss t=$t) → $dest/media.mp4"
  ffmpeg -y -loglevel error -ss "$ss" -t "$t" -i "$src" \
    -f lavfi -i "color=c=black:s=320x240:r=5" \
    -map 1:v -map 0:a -shortest \
    -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac \
    "$dest/media.mp4"
done

echo "[done] regenerated real-audio fixture media under $FX"

# --- VISION (object) fixtures: a still PD photo ken-burns'd into a muxed clip ---
# case | source image URL (PD, Wikimedia) | duration secs
VISION_CLIPS=(
  "car_object|https://upload.wikimedia.org/wikipedia/commons/1/18/Public_domain_image_-_Peugeot_iOn_electric_car_in_front_of_wind_turbines.JPG|6"
)
FPS=25
for row in "${VISION_CLIPS[@]}"; do
  IFS='|' read -r case url secs <<<"$row"
  dest="$FX/$case"
  [[ -d "$dest" ]] || { echo "[skip] $case — no fixture dir (ground truth not committed?)"; continue; }
  src="$CACHE/$case.jpg"
  if [[ ! -s "$src" ]]; then
    echo "[fetch] $case ← $url"
    curl -fSL -m 180 -o "$src" "$url"
  fi
  frames=$(( secs * FPS ))
  echo "[kenburns] $case (${secs}s) → $dest/media.mp4"
  ffmpeg -y -loglevel error -loop 1 -i "$src" -f lavfi -i "anullsrc=r=16000:cl=mono" \
    -vf "scale=2560:1440:force_original_aspect_ratio=increase,crop=2560:1440,zoompan=z='min(zoom+0.0008,1.3)':d=${frames}:s=1280x720:fps=${FPS},format=yuv420p" \
    -t "$secs" -map 0:v -map 1:a -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -shortest \
    "$dest/media.mp4"
done

echo "[done] regenerated vision fixture media under $FX"
