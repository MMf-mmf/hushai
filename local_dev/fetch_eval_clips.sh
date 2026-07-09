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
  # Same window as jfk_moon on purpose: it's the verified mints-exactly-1-voice substrate, and the
  # case doubles the file as its own enrollment ref (identical audio ⇒ the case injection is
  # guaranteed to match the enrolled "Mendel" centroid).
  "clip_speaker_roster|https://upload.wikimedia.org/wikipedia/commons/5/50/Jfk_rice_university_we_choose_to_go_to_the_moon.ogg|334|14"
  "fdr_infamy|https://upload.wikimedia.org/wikipedia/commons/7/7e/Roosevelt_Infamy.ogg|0|16"
  "armstrong_step|https://upload.wikimedia.org/wikipedia/commons/d/dd/Armstrong_Small_Step.ogg|0|24"
  # LONG-CLIP RAG fixture: ~4 min of the same Rice speech (source window 300-540s). The proven
  # jfk_moon mints-1-voice window (334-348s) sits at clip offsets 34-48s — the known content
  # anchor its chat questions target. Cache hit: same source file as jfk_moon.
  "jfk_long|https://upload.wikimedia.org/wikipedia/commons/5/50/Jfk_rice_university_we_choose_to_go_to_the_moon.ogg|300|240"
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
  # loudnorm: archival PD audio arrives at wildly varying levels; quiet static-heavy clips
  # (Armstrong moon radio) fell below the worker's audio VAD skip-silent gate after an encode
  # drift and got SKIPPED (11/12 segments) — normalize the FIXTURE level (a real camera has
  # AGC) instead of ever touching the gate.
  # Exact OUTPUT duration (-t after the maps): the old `-shortest` raced the lavfi black-video
  # generator against audio EOF, so the container duration varied run-to-run (19.4s vs 20.4s
  # from identical audio) → a different segment count → different whisper tail hallucinations
  # → WER drift on regeneration. Output -t pins both streams.
  ffmpeg -y -loglevel error -ss "$ss" -t "$t" -i "$src" \
    -f lavfi -i "color=c=black:s=320x240:r=5" \
    -map 1:v -map 0:a -t "$t" \
    -af "loudnorm=I=-20:TP=-3:LRA=11" \
    -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -b:a 192k \
    "$dest/media.mp4"
done

echo "[done] regenerated real-audio fixture media under $FX"

# jfk_long's speaker-enrollment ref: the SAME proven mints-exactly-1-voice window jfk_moon /
# clip_speaker_roster use (identical audio ⇒ the case injection matches the enrolled "Mendel").
JL="$FX/jfk_long"
if [[ -d "$JL" ]]; then
  mkdir -p "$JL/refs"
  ffmpeg -y -loglevel error -ss 334 -t 14 -i "$CACHE/jfk_long.src" \
    -f lavfi -i "color=c=black:s=320x240:r=5" \
    -map 1:v -map 0:a -t 14 \
    -af "loudnorm=I=-20:TP=-3:LRA=11" \
    -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -b:a 192k \
    "$JL/refs/enroll.mp4"
  echo "[mux] jfk_long enrollment ref → $JL/refs/enroll.mp4"
fi

# --- VISION (object) fixtures: a still PD photo ken-burns'd into a muxed clip ---
# case | source image URL (PD, Wikimedia) | duration secs
VISION_CLIPS=(
  "car_object|https://upload.wikimedia.org/wikipedia/commons/1/18/Public_domain_image_-_Peugeot_iOn_electric_car_in_front_of_wind_turbines.JPG|6"
  "face_id|https://commons.wikimedia.org/wiki/Special:FilePath/Judith%20A.%20Resnik,%20official%20portrait%20(cropped).jpg|6"
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

# --- visit_coalesce: a 60s continuous-face clip (same Judith Resnik portrait as face_id) — the
# visit-coalescing probe injects it twice, 4h apart. ~30 raw per-segment sightings per injection;
# the chat answer must be 2 VISITS, never the raw count. Slower zoom than face_id so the face
# stays large and detectable for the full minute.
VC="$FX/visit_coalesce"
if [[ -d "$VC" ]]; then
  mkdir -p "$VC/clips"
  vsrc="$CACHE/face_id.jpg"
  [[ -s "$vsrc" ]] || { echo "[fetch] visit_coalesce ← face_id portrait"; curl -fSL -m 180 -o "$vsrc" "https://commons.wikimedia.org/wiki/Special:FilePath/Judith%20A.%20Resnik,%20official%20portrait%20(cropped).jpg"; }
  vframes=$(( 60 * FPS ))
  echo "[kenburns] visit_coalesce (60s) → $VC/clips/visit.mp4"
  ffmpeg -y -loglevel error -loop 1 -i "$vsrc" -f lavfi -i "anullsrc=r=16000:cl=mono" \
    -vf "scale=2560:1440:force_original_aspect_ratio=increase,crop=2560:1440,zoompan=z='min(zoom+0.0001,1.15)':d=${vframes}:s=1280x720:fps=${FPS},format=yuv420p" \
    -t 60 -map 0:v -map 1:a -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -shortest \
    "$VC/clips/visit.mp4"
else
  echo "[skip] visit_coalesce — no fixture dir (ground truth not committed?)"
fi

# --- PLATE (ALPR) fixture: crop a full car + readable plate from a PD street photo, into a muxed clip.
# The crop keeps the WHOLE car so RF-DETR boxes it (the plate lane runs inside vehicle ROIs), with the
# plate 'EMD774' large enough to OCR. Coords are for the 5184x3456 source; adjust if the source changes.
PLATE_CASE="plate_ocr"
PLATE_URL="https://commons.wikimedia.org/wiki/Special:FilePath/Cars_in_traffic_in_Auckland,_New_Zealand_-_copyright-free_photo_released_to_public_domain.jpg"
PLATE_CROP="crop=1100:900:3950:2450"
pdest="$FX/$PLATE_CASE"
if [[ -d "$pdest" ]]; then
  psrc="$CACHE/$PLATE_CASE.jpg"
  [[ -s "$psrc" ]] || { echo "[fetch] $PLATE_CASE ← $PLATE_URL"; curl -fSL -m 180 -o "$psrc" "$PLATE_URL"; }
  echo "[plate] $PLATE_CASE → $pdest/media.mp4"
  ffmpeg -y -loglevel error -loop 1 -i "$psrc" -f lavfi -i "anullsrc=r=16000:cl=mono" \
    -vf "${PLATE_CROP},scale=1280:720:force_original_aspect_ratio=decrease,pad=1280:720:(ow-iw)/2:(oh-ih)/2,format=yuv420p" \
    -t 6 -r 5 -map 0:v -map 1:a -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -shortest \
    "$pdest/media.mp4"
  echo "[done] regenerated plate fixture media under $FX"
else
  echo "[skip] $PLATE_CASE — no fixture dir (ground truth not committed?)"
fi

# --- Gotham entity-graph fixtures (F1–F3, `graph` eval modality, split=train) --------------------
# Reuse the proven PD substrates: the Judith Resnik portrait (face_id → "Alice"), a Sally Ride
# portrait (→ "Bob"/"Mallory" — a DISTINCT face so re-ID mints a second person), the Auckland plate
# crop (→ EMD774, which the ALPR reads as "EM0774"), and the JFK Rice speech (→ Alice's voice). Same
# ffmpeg recipes as above so the pipeline output — and thus the frozen graph baselines — reproduce.
STG="$ROOT/hushai-eval/fixtures/train"
BOB_URL="https://commons.wikimedia.org/wiki/Special:FilePath/Sally_Ride_(1984).jpg"
if [[ -d "$STG/graph_cross_camera_fusion" ]]; then
  # sources (cache-shared with face_id / plate_ocr / jfk_moon above)
  jsrc="$CACHE/face_id.jpg"; bsrc="$CACHE/bob.jpg"; psrc="$CACHE/plate_ocr.jpg"; ssrc="$CACHE/jfk_moon.src"
  [[ -s "$jsrc" ]] || curl -fSL -m 180 -o "$jsrc" "https://commons.wikimedia.org/wiki/Special:FilePath/Judith%20A.%20Resnik,%20official%20portrait%20(cropped).jpg"
  [[ -s "$bsrc" ]] || { echo "[fetch] graph:bob ← $BOB_URL"; curl -fSL -m 180 -o "$bsrc" "$BOB_URL"; }
  [[ -s "$psrc" ]] || curl -fSL -m 180 -o "$psrc" "$PLATE_URL"
  [[ -s "$ssrc" ]] || curl -fSL -m 180 -o "$ssrc" "https://upload.wikimedia.org/wikipedia/commons/5/50/Jfk_rice_university_we_choose_to_go_to_the_moon.ogg"

  # 6s silent ken-burns face clip (portrait stays large + detectable). $1=src $2=dst
  kenburns_face() {
    ffmpeg -y -loglevel error -loop 1 -i "$1" -f lavfi -i "anullsrc=r=16000:cl=mono" \
      -vf "scale=2560:1440:force_original_aspect_ratio=increase,crop=2560:1440,zoompan=z='min(zoom+0.0008,1.3)':d=150:s=1280x720:fps=25,format=yuv420p" \
      -t 6 -map 0:v -map 1:a -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -shortest "$2"
  }

  # F2 graph_cross_camera_fusion: Alice's face (one clip, injected on two devices).
  mkdir -p "$STG/graph_cross_camera_fusion/clips"
  echo "[graph] F2 alice_face.mp4"; kenburns_face "$jsrc" "$STG/graph_cross_camera_fusion/clips/alice_face.mp4"

  # F3 graph_person_vehicle: Alice face + Bob face + the EMD774 plate crop.
  mkdir -p "$STG/graph_person_vehicle/clips"
  echo "[graph] F3 alice_face.mp4 + bob_face.mp4 + plate.mp4"
  kenburns_face "$jsrc" "$STG/graph_person_vehicle/clips/alice_face.mp4"
  kenburns_face "$bsrc" "$STG/graph_person_vehicle/clips/bob_face.mp4"
  ffmpeg -y -loglevel error -loop 1 -i "$psrc" -f lavfi -i "anullsrc=r=16000:cl=mono" \
    -vf "${PLATE_CROP},scale=1280:720:force_original_aspect_ratio=decrease,pad=1280:720:(ow-iw)/2:(oh-ih)/2,format=yuv420p" \
    -t 6 -r 5 -map 0:v -map 1:a -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -shortest \
    "$STG/graph_person_vehicle/clips/plate.mp4"

  # F1 graph_face_voice_bind: Alice face + JFK speech (14s muxed) + a silent Mallory (Bob's) face.
  mkdir -p "$STG/graph_face_voice_bind/clips"
  echo "[graph] F1 alice_talks.mp4 + mallory_silent.mp4"
  ffmpeg -y -loglevel error -loop 1 -i "$jsrc" -ss 334 -t 14 -i "$ssrc" \
    -vf "scale=2560:1440:force_original_aspect_ratio=increase,crop=2560:1440,zoompan=z='min(zoom+0.0004,1.2)':d=350:s=1280x720:fps=25,format=yuv420p" \
    -map 0:v -map 1:a -t 14 -af "loudnorm=I=-20:TP=-3:LRA=11" \
    -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -b:a 192k "$STG/graph_face_voice_bind/clips/alice_talks.mp4"
  kenburns_face "$bsrc" "$STG/graph_face_voice_bind/clips/mallory_silent.mp4"
  echo "[done] regenerated Gotham graph fixture media under $STG"
else
  echo "[skip] graph_* — no train fixture dirs"
fi

# --- Gotham G2 baseline/anomaly fixtures (F4-F6, `graph` modality) --------------------------------
# Reuse the same face substrates: Judith (→ Alice, the enrolled weekly regular) + Sally (→ the F6
# stranger, a DISTINCT face). Weekly-cadence multi-injection stages the rhythm; the silent-face
# ffmpeg recipe matches G1 so the pipeline output — and thus the frozen baselines — reproduce.
G2_TRAIN="$ROOT/hushai-eval/fixtures/train"
G2_HOLD="$ROOT/hushai-eval/fixtures/holdout"
if [[ -d "$G2_TRAIN/graph_baseline_rhythm" ]]; then
  jsrc="$CACHE/face_id.jpg"
  bsrc="$CACHE/bob.jpg"
  [[ -s "$jsrc" ]] || curl -fSL -m 180 -o "$jsrc" "https://commons.wikimedia.org/wiki/Special:FilePath/Judith%20A.%20Resnik,%20official%20portrait%20(cropped).jpg"
  [[ -s "$bsrc" ]] || curl -fSL -m 180 -o "$bsrc" "https://commons.wikimedia.org/wiki/Special:FilePath/Sally_Ride_(1984).jpg"
  kb_face() {
    ffmpeg -y -loglevel error -loop 1 -i "$1" -f lavfi -i "anullsrc=r=16000:cl=mono" \
      -vf "scale=2560:1440:force_original_aspect_ratio=increase,crop=2560:1440,zoompan=z='min(zoom+0.0008,1.3)':d=150:s=1280x720:fps=25,format=yuv420p" \
      -t 6 -map 0:v -map 1:a -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -shortest "$2"
  }
  # F7 briefing_daily shares the same Judith→Alice substrate (multi-week rhythm; the digest for a
  # pinned date summarizes it). Regenerate its clip alongside F4/F5/F6.
  for d in "$G2_TRAIN/graph_baseline_rhythm" "$G2_TRAIN/anomaly_novel_time" "$G2_TRAIN/briefing_daily" "$G2_HOLD/anomaly_negatives"; do
    [[ -d "$d" ]] || continue
    mkdir -p "$d/clips"
    echo "[graph] G2 alice_face.mp4 → ${d##*/}"
    kb_face "$jsrc" "$d/clips/alice_face.mp4"
  done
  mkdir -p "$G2_HOLD/anomaly_negatives/clips"
  echo "[graph] G2 stranger_face.mp4 (F6 unknown)"
  kb_face "$bsrc" "$G2_HOLD/anomaly_negatives/clips/stranger_face.mp4"
  echo "[done] regenerated Gotham G2 fixture media"
else
  echo "[skip] graph G2 — no fixture dirs"
fi

# --- Gotham G2 first_time_pairing fixture (train) ------------------------------------------------
# Two DISTINCT enrolled regulars (Judith→Alice, Sally→Bob) each staged 5× alone then once together;
# the first co_present edge between two mature regulars fires first_time_pairing. Same face recipe as
# F4-F6 so the pipeline output reproduces. Calibrated live + gate x2 → train. The deterministic
# wiring proof is hushai-backend/tests/graph_db.rs.
G2_PAIR="$ROOT/hushai-eval/fixtures/train"
if [[ -d "$G2_PAIR/anomaly_first_pairing" ]]; then
  jsrc="$CACHE/face_id.jpg"
  bsrc="$CACHE/bob.jpg"
  [[ -s "$jsrc" ]] || curl -fSL -m 180 -o "$jsrc" "https://commons.wikimedia.org/wiki/Special:FilePath/Judith%20A.%20Resnik,%20official%20portrait%20(cropped).jpg"
  [[ -s "$bsrc" ]] || curl -fSL -m 180 -o "$bsrc" "https://commons.wikimedia.org/wiki/Special:FilePath/Sally_Ride_(1984).jpg"
  kb_face_s() {
    ffmpeg -y -loglevel error -loop 1 -i "$1" -f lavfi -i "anullsrc=r=16000:cl=mono" \
      -vf "scale=2560:1440:force_original_aspect_ratio=increase,crop=2560:1440,zoompan=z='min(zoom+0.0008,1.3)':d=150:s=1280x720:fps=25,format=yuv420p" \
      -t 6 -map 0:v -map 1:a -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -shortest "$2"
  }
  mkdir -p "$G2_PAIR/anomaly_first_pairing/clips"
  echo "[graph] G2 first_pairing alice_face.mp4 + bob_face.mp4"
  kb_face_s "$jsrc" "$G2_PAIR/anomaly_first_pairing/clips/alice_face.mp4"
  kb_face_s "$bsrc" "$G2_PAIR/anomaly_first_pairing/clips/bob_face.mp4"
  echo "[done] regenerated Gotham G2 first_time_pairing fixture media"
else
  echo "[skip] graph G2 first_pairing — no train fixture dir"
fi
