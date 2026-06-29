#!/usr/bin/env bash
#
# build_fixtures.sh — generate hushai-eval fixtures with CONSTRUCTION-KNOWN ground truth.
#
# Audio fixtures use macOS `say` (offline, deterministic, many distinct voices) rendered over a
# tiny black video so feed_segments.py can mux them as h264+aac MUXED segments. Because we feed a
# KNOWN script through TTS, the transcript / speaker-count / turn-boundaries ARE the ground truth —
# no human labeling needed. The committed artifacts are this recipe + each meta.json/expected.json;
# the media files themselves are gitignored (regenerate any time with `./local_dev/build_fixtures.sh`).
#
# Vision fixtures (faces/objects/plates) are added by separate recipes once their weights are
# provisioned (Phase 0); see make_object_clip.sh for the object/still pattern.
#
# Usage: ./local_dev/build_fixtures.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FX="$ROOT/hushai-eval/fixtures"
# Fixed capture base (any constant; partitioning is by insertion time, not this). 2026-06-15T12:00Z.
BASE_NS=1781784000000000000

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing required tool: $1" >&2; exit 1; }; }
need say; need ffmpeg; need ffprobe; need python3

# render <voice> <text> <out.aiff>
render() { say -v "$1" -o "$3" "$2"; }
# duration_ns <audiofile>  -> integer nanoseconds
dur_ns() { python3 -c "import sys;print(int(float(sys.argv[1])*1e9))" "$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$1")"; }
# mux <audio> <out.mp4>  (black 320x240@5fps video matched to audio length)
mux() {
  ffmpeg -y -loglevel error -i "$1" -f lavfi -i "color=c=black:s=320x240:r=5" \
    -map 1:v -map 0:a -shortest -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac "$2"
}

mkdir -p "$FX/train" "$FX/holdout"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

# ---------------------------------------------------------------------------
# 1) asr_short (train, fast) — single speaker, known transcript.
# ---------------------------------------------------------------------------
echo "[fixtures] asr_short"
ASR_TEXT="the quick brown fox jumps over the lazy dog. pack my box with five dozen liquor jugs."
D="$FX/train/asr_short"; mkdir -p "$D"
render "Samantha" "$ASR_TEXT" "$TMP/asr.aiff"
mux "$TMP/asr.aiff" "$D/media.mp4"
python3 - "$D" "$BASE_NS" "$ASR_TEXT" <<'PY'
import json,sys
d,base,text=sys.argv[1],int(sys.argv[2]),sys.argv[3]
json.dump({"case_id":"asr_short","description":"single TTS speaker, known transcript",
 "device_id":"eval-asr-short","media_file":"media.mp4","media_kind":"muxed","seg_seconds":2,
 "base_capture_unix_nanos":base,"modalities":["transcript"],"tier":"fast",
 "poll":{"timeout_secs":120,"interval_secs":2}}, open(d+"/meta.json","w"), indent=2)
json.dump({"transcript":{"full_text":text,"max_wer":0.30,"min_similarity":0.70}},
 open(d+"/expected.json","w"), indent=2)
PY

# ---------------------------------------------------------------------------
# 2) two_speakers (train, full) — two distinct voices, known turns + speech events.
# ---------------------------------------------------------------------------
echo "[fixtures] two_speakers"
A_VOICE="Samantha"; A_TEXT="hello there. how are you doing today? it is a beautiful morning."
B_VOICE="Daniel";   B_TEXT="honestly i am not doing well at all. everything feels terrible right now."
D="$FX/train/two_speakers"; mkdir -p "$D"
render "$A_VOICE" "$A_TEXT" "$TMP/a.aiff"
render "$B_VOICE" "$B_TEXT" "$TMP/b.aiff"
ANS=$(dur_ns "$TMP/a.aiff")
ffmpeg -y -loglevel error -i "$TMP/a.aiff" -i "$TMP/b.aiff" \
  -filter_complex "[0:a][1:a]concat=n=2:v=0:a=1" "$TMP/ab.wav"
mux "$TMP/ab.wav" "$D/media.mp4"
TOT=$(dur_ns "$TMP/ab.wav")
python3 - "$D" "$BASE_NS" "$A_TEXT" "$B_TEXT" "$ANS" "$TOT" <<'PY'
import json,sys
d,base,a,b,asplit,tot=sys.argv[1],int(sys.argv[2]),sys.argv[3],sys.argv[4],int(sys.argv[5]),int(sys.argv[6])
# NOTE: "speakers" is intentionally NOT in modalities yet — the local speaker lane currently mints
# 0 speakers on available clips (TTS *and* real audio; ~0.31s post-VAD speech, all 'marginal'). The
# diarization ground truth below stays in expected.json and the scorer is wired; add "speakers" to
# modalities once that lane is investigated/fixed (do NOT loosen mint gates just to pass this).
json.dump({"case_id":"two_speakers","description":"two distinct TTS voices, known turns (ASR+events; diarization staged)",
 "device_id":"eval-two-speakers","media_file":"media.mp4","media_kind":"muxed","seg_seconds":2,
 "base_capture_unix_nanos":base,"modalities":["transcript","events"],"tier":"full",
 "poll":{"timeout_secs":180,"interval_secs":2}}, open(d+"/meta.json","w"), indent=2)
json.dump({
 "transcript":{"full_text":a+" "+b,"max_wer":0.30,"min_similarity":0.70},
 "speakers":{"distinct_count":2,"count_tolerance":0,"min_purity":0.75,"utterances":[
   {"label":"A","text_contains":"how are you","window_ns":[0,asplit]},
   {"label":"B","text_contains":"not doing well","window_ns":[asplit,tot]}]},
 "events":{"expected":[{"event_type":"speech","min_count":1}]}
}, open(d+"/expected.json","w"), indent=2)
PY

# ---------------------------------------------------------------------------
# 3) silence_no_speech (holdout, full) — counter-fixture: must NOT hallucinate speech/speakers.
# ---------------------------------------------------------------------------
echo "[fixtures] silence_no_speech (holdout)"
D="$FX/holdout/silence_no_speech"; mkdir -p "$D"
ffmpeg -y -loglevel error -f lavfi -i "anullsrc=r=16000:cl=mono" -t 6 -c:a pcm_s16le "$TMP/sil.wav"
mux "$TMP/sil.wav" "$D/media.mp4"
python3 - "$D" "$BASE_NS" <<'PY'
import json,sys
d,base=sys.argv[1],int(sys.argv[2])
json.dump({"case_id":"silence_no_speech","description":"6s silence — must produce no speech/speakers",
 "device_id":"eval-silence","media_file":"media.mp4","media_kind":"muxed","seg_seconds":2,
 "base_capture_unix_nanos":base,"modalities":["transcript","speakers"],"tier":"full",
 "poll":{"timeout_secs":120,"interval_secs":2}}, open(d+"/meta.json","w"), indent=2)
# Empty reference: a clean pipeline emits ~0 words. max_wer here = max spurious words tolerated
# (WER over an empty ref is hyp_word_count, since ref length floors to 1). Whisper is known to
# HALLUCINATE a few tokens on pure silence; the floor tolerates today's behavior while the baseline
# delta catches any INCREASE. The hard counter-fixture gate is speakers.distinct_count == 0 (no
# phantom speaker minted from noise).
json.dump({"transcript":{"full_text":"","max_wer":6.0,"min_similarity":0.0},
 "speakers":{"distinct_count":0,"count_tolerance":0}}, open(d+"/expected.json","w"), indent=2)
PY

echo "[fixtures] done. Generated media (gitignored) + meta/expected under $FX"
ls -1 "$FX"/train "$FX"/holdout
