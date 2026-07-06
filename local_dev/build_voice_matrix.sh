#!/usr/bin/env bash
#
# build_voice_matrix.sh — generate the voice-assistant acoustic test matrix (deterministic recipe;
# media gitignored, same policy as the eval fixtures).
#
# Material (macOS `say` TTS proxy): Samantha = OWNER, Daniel = STRANGER.
#   * 7 guided-enrollment utterances (the app's 6 sample prompts + the verify phrase)
#   * 5 owner wake+question clips ("computer, …") + the same 5 texts in the stranger voice
#   * noise beds (brown / pink / 60Hz-hum / TV-speech from the cached PD FDR clip)
#   * owner+stranger question clips mixed with each bed at SNR 20 / 10 / 5 dB
#   * matrix_manifest.json — the trial list local_dev/voice_assistant_loop.py consumes
#
# REALISM CAVEAT (documented, deliberate): TTS-through-Mac-speaker validates the enroll/verify
# PLUMBING, threshold behavior under channel+noise degradation, and cross-restart persistence.
# It does NOT validate human-voice discrimination. The cosine CSV the loop script emits is the
# first LABELED capture set RECURSIVE_TESTING.md §4 requires before any SPEAKER_THRESHOLD
# calibration; re-run the matrix with the real owner's recorded voice when available (drop the
# replacement WAVs into the same filenames). A PD music bed is a noted future addition.
#
# Usage: ./local_dev/build_voice_matrix.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/local_dev/captures/voice_matrix"
CACHE="$ROOT/hushai-eval/.work/clip-cache"
mkdir -p "$OUT"

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing required tool: $1" >&2; exit 1; }; }
need say; need ffmpeg; need ffprobe; need python3

OWNER_VOICE="Samantha"
STRANGER_VOICE="Daniel"

# say → 16k mono wav normalized to -20 dBFS mean (a stable speech reference level, so the
# noise-bed levels below produce true SNRs).
render() { # voice text out.wav
  say -v "$1" -o "$OUT/.tmp.aiff" "$2"
  ffmpeg -y -loglevel error -i "$OUT/.tmp.aiff" -ar 16000 -ac 1 \
    -af "loudnorm=I=-20:TP=-3:LRA=11" "$3"
}

echo "[matrix] enrollment utterances (the app's guided prompts)"
ENROLL_TEXTS=(
  "the quick brown fox jumps over the lazy dog"
  "my voice is my passport, please verify me"
  "I am teaching this assistant to know my voice"
  "seven green apples fell from the old oak tree"
  "I am speaking from across the room"
  "this is how I usually talk every day"
  "it's really me, open up"
)
i=1
for t in "${ENROLL_TEXTS[@]}"; do
  render "$OWNER_VOICE" "$t" "$OUT/owner_enroll_$(printf %02d $i).wav"
  i=$((i+1))
done

echo "[matrix] question clips (wake word, a beat of silence, then the question)"
# Run-1 finding: "computer, <question>" as ONE utterance blends the wake word into the sentence
# for some TTS renders — Vosk never emits a final containing the bare wake word and 32/49 trials
# were excluded. A separate wake utterance + 1s gap gives a clean wake final, then the question
# arrives as its own (longer, better-for-verification) utterance via the AWAIT_QUESTION path.
QUESTIONS=(
  "what did the recordings say about money?"
  "how many minutes of video do we have today?"
  "who was speaking in the last hour?"
  "what have we talked about this morning?"
  "did anyone mention a delivery today?"
)
ffmpeg -y -loglevel error -f lavfi -t 1.0 -i "anullsrc=r=16000:cl=mono" -c:a pcm_s16le "$OUT/.gap.wav"
render "$OWNER_VOICE" "computer" "$OUT/.wake_owner.wav"
render "$STRANGER_VOICE" "computer" "$OUT/.wake_stranger.wav"
i=1
for q in "${QUESTIONS[@]}"; do
  render "$OWNER_VOICE" "$q" "$OUT/.q_owner.wav"
  render "$STRANGER_VOICE" "$q" "$OUT/.q_stranger.wav"
  ffmpeg -y -loglevel error -i "$OUT/.wake_owner.wav" -i "$OUT/.gap.wav" -i "$OUT/.q_owner.wav"     -filter_complex "[0:a][1:a][2:a]concat=n=3:v=0:a=1" "$OUT/owner_q$(printf %02d $i).wav"
  ffmpeg -y -loglevel error -i "$OUT/.wake_stranger.wav" -i "$OUT/.gap.wav" -i "$OUT/.q_stranger.wav"     -filter_complex "[0:a][1:a][2:a]concat=n=3:v=0:a=1" "$OUT/stranger_q$(printf %02d $i).wav"
  i=$((i+1))
done
rm -f "$OUT/.gap.wav" "$OUT/.wake_owner.wav" "$OUT/.wake_stranger.wav" "$OUT/.q_owner.wav" "$OUT/.q_stranger.wav"

echo "[matrix] noise beds (30s each)"
ffmpeg -y -loglevel error -f lavfi -t 30 -i "anoisesrc=color=brown:r=16000" \
  -af "loudnorm=I=-20:TP=-3:LRA=11" -ac 1 "$OUT/bed_brown.wav"
ffmpeg -y -loglevel error -f lavfi -t 30 -i "anoisesrc=color=pink:r=16000" \
  -af "loudnorm=I=-20:TP=-3:LRA=11" -ac 1 "$OUT/bed_pink.wav"
# Mains hum: 60 Hz + 3rd harmonic.
ffmpeg -y -loglevel error -f lavfi -t 30 -i "sine=frequency=60:sample_rate=16000" \
  -f lavfi -t 30 -i "sine=frequency=180:sample_rate=16000" \
  -filter_complex "[0:a][1:a]amix=inputs=2:normalize=0,loudnorm=I=-20:TP=-3:LRA=11" \
  -ac 1 "$OUT/bed_hum.wav"
# TV-speech: the cached PD FDR clip (fetched by fetch_eval_clips.sh). Skip bed if absent.
if [[ -s "$CACHE/fdr_infamy.src" ]]; then
  ffmpeg -y -loglevel error -t 30 -i "$CACHE/fdr_infamy.src" -ar 16000 -ac 1 \
    -af "loudnorm=I=-20:TP=-3:LRA=11" "$OUT/bed_tv.wav"
else
  echo "[matrix] WARN: no cached fdr_infamy.src (run fetch_eval_clips.sh) — skipping the tv bed"
fi

echo "[matrix] SNR mixes"
# Speech is at -20 dBFS mean; a bed at (-20 - SNR) gives the target SNR. amix normalize=0
# preserves both levels.
mix() { # speech.wav bed.wav snr_db out.wav
  ffmpeg -y -loglevel error -i "$1" -stream_loop -1 -i "$2" \
    -filter_complex "[1:a]volume=-${3}dB[n];[0:a][n]amix=inputs=2:duration=first:normalize=0" \
    -ac 1 "$4"
}

BEDS=(brown pink hum)
[[ -s "$OUT/bed_tv.wav" ]] && BEDS+=(tv)
for bed in "${BEDS[@]}"; do
  for snr in 20 10 5; do
    for i in 01 02 03; do
      mix "$OUT/owner_q$i.wav" "$OUT/bed_$bed.wav" "$snr" "$OUT/owner_q${i}_${bed}_snr${snr}.wav"
    done
  done
  # Stranger under TV-speech (the discriminative bed) — must still reject.
  if [[ "$bed" == "tv" ]]; then
    for i in 01 02 03; do
      mix "$OUT/stranger_q$i.wav" "$OUT/bed_tv.wav" 10 "$OUT/stranger_q${i}_tv_snr10.wav"
    done
  fi
done
rm -f "$OUT/.tmp.aiff"

echo "[matrix] manifest"
python3 - "$OUT" <<'PY'
import glob, json, os, subprocess, sys
out = sys.argv[1]

def dur(p):
    return float(subprocess.check_output(
        ["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", p],
        text=True).strip())

rows = []
def add(path, speaker, condition, snr, expect):
    rows.append({"clip": os.path.basename(path), "speaker": speaker, "condition": condition,
                 "snr": snr, "expect": expect, "secs": round(dur(path), 2)})

for i in ["01", "02", "03", "04", "05"]:
    add(f"{out}/owner_q{i}.wav", "owner", "clean", None, "accept")
    add(f"{out}/stranger_q{i}.wav", "stranger", "clean", None, "reject")
for p in sorted(glob.glob(f"{out}/owner_q0[123]_*_snr*.wav")):
    base = os.path.basename(p)[:-4]           # owner_q01_brown_snr20
    _, _, bed, snr = base.split("_")
    add(p, "owner", bed, int(snr[3:]), "accept")
for p in sorted(glob.glob(f"{out}/stranger_q0[123]_tv_snr10.wav")):
    add(p, "stranger", "tv", 10, "reject")

enroll = [os.path.basename(p) for p in sorted(glob.glob(f"{out}/owner_enroll_*.wav"))]
json.dump({"enroll_clips": enroll, "trials": rows},
          open(f"{out}/matrix_manifest.json", "w"), indent=2)
print(f"[matrix] {len(rows)} trials, {len(enroll)} enrollment clips → {out}/matrix_manifest.json")
PY

echo "[matrix] done. Operator setup: Mac volume ~70%, phone mic ~30cm from the speakers, quiet room."
