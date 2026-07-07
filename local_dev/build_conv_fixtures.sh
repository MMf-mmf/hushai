#!/usr/bin/env bash
#
# build_conv_fixtures.sh — conversation-threading (migration 0025) eval fixtures with
# CONSTRUCTION-KNOWN ground truth. Companion to build_fixtures.sh (same render/mux/dur_ns
# idioms, same pinned BASE_NS); kept separate so regenerating the threading suite never
# touches the 16 existing fixtures' media.
#
# Signal coverage (T=time-gap, D=device, S=speaker-set, P=topic):
#   conv_gap_split          train/fast   T   — +600s injection gap, same voices → 2 convos
#   conv_pause_resume       train/full   T̄   — 60s pause inside the 300s gap → 1 convo
#   conv_two_devices_parallel train/full D   — fully overlapping A/B on two cameras → 2, never merged
#   conv_three_speakers     train/full   S̄   — 3-voice round-robin, one topic → 1 (anti-fragmentation)
#   conv_interleaved_same_mic STAGING    P+S — A1 B1 A2 B2 A3 B3 slotted on ONE mic → 2.
#                                             Hard-depends on the speaker lane separating the
#                                             cast voices (stage C clusters SPEAKER nodes);
#                                             promote to train only after the voice-separability
#                                             pre-check proves the cast separates (never loosen
#                                             gates to promote it).
#   conv_topic_shift        STAGING      P-vs-T probe — hard topic pivot, no gap, no interleave
#                                             → semantics say 1 (never split topic drift alone).
#                                             Info-only characterization.
#   conv_overlap_degrade    STAGING      double-talk (amix) — graceful-degradation probe: no
#                                             crash, no cross-topic fabrication.
#   conv_speaker_migrates   HOLDOUT/full D+S — Daniel moves cam1→cam2 mid-run (sealed).
#   conv_long_multiblock    HOLDOUT/full T   — 3 conversations via ≫gap injection offsets (sealed).
#
# Time gaps are ALWAYS injection offsets, never rendered silence (whisper hallucinates
# tokens into rendered silence — the silence_no_speech lesson). Two-device fixtures are one
# clean clip per device with overlapping capture_start_offset_ns.
#
# Usage: ./local_dev/build_conv_fixtures.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FX="$ROOT/hushai-eval/fixtures"
BASE_NS=1781784000000000000 # 2026-06-15T12:00Z — same pinned base as build_fixtures.sh

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing required tool: $1" >&2; exit 1; }; }
need say; need ffmpeg; need ffprobe; need python3

render() { say -v "$1" -o "$3" "$2"; }
dur_ns() { python3 -c "import sys;print(int(float(sys.argv[1])*1e9))" "$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$1")"; }
mux() {
  ffmpeg -y -loglevel error -i "$1" -f lavfi -i "color=c=black:s=320x240:r=5" \
    -map 1:v -map 0:a -shortest -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac "$2"
}
# concat_wav <out.wav> <in1> <in2> ...
concat_wav() {
  local out="$1"; shift
  local inputs=() filter="" i=0
  for f in "$@"; do inputs+=(-i "$f"); filter+="[$i:a]"; i=$((i+1)); done
  ffmpeg -y -loglevel error "${inputs[@]}" -filter_complex "${filter}concat=n=$i:v=0:a=1" "$out"
}
# pad_to_slot <in.aiff> <slot_secs> <out.wav> — turn padded with trailing silence to an
# exact slot so GT windows are construction-known. Asserts the render fits the slot.
pad_to_slot() {
  local in="$1" slot="$2" out="$3"
  local dur pad
  dur="$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$in")"
  pad="$(python3 -c "import sys;d=float(sys.argv[1]);s=float(sys.argv[2]);assert d < s - 0.3, f'render {d}s too long for {s}s slot';print(f'{s-d:.3f}')" "$dur" "$slot")"
  ffmpeg -y -loglevel error -i "$in" -f lavfi -t "$pad" -i "anullsrc=r=22050:cl=mono" \
    -filter_complex "[0:a][1:a]concat=n=2:v=0:a=1" "$out"
}

# Cast. Conversation A = the proven pair; conversation B = the step-0 candidates (swap
# after the separability pre-check if needed — regenerate + re-baseline in one move).
A1="Samantha"; A2="Daniel"; B1="Karen"; B2="Rishi"; C3="Moira"

mkdir -p "$FX/train" "$FX/holdout" "$FX/staging"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

# Topic scripts (distinct, concrete vocabulary so topic affinity has signal).
DINNER_A="let us plan the dinner party for saturday. i want to serve roasted salmon and a lemon tart."
DINNER_B="good idea. i will buy the salmon at the fish market and pick up wine on the way home."
DINNER_A2="perfect. remember the tart needs fresh lemons and we should set the table by six."
CAR_A="the car is making a strange rattling noise near the front wheel again."
CAR_B="that sounds like the brake pads. we should take it to the mechanic on monday morning."
CAR_A2="alright. i will book the mechanic appointment and ask about the engine oil as well."
RENO_A="the kitchen renovation starts next week. the contractor wants to install the cabinets first."
RENO_B="good. the countertop and the new sink arrive on thursday so the cabinets must be done by then."
RENO_A2="i will confirm the cabinet delivery and make sure the plumber comes for the sink."
BALL_A="did you watch the football match last night? the referee gave two penalties in the second half."
BALL_B="yes. the goalkeeper saved the first penalty but the striker scored the winning goal at the end."
BALL_B2="our team climbs to second place in the league table with that goal."
VET_A="the dog needs his vaccination this month. the veterinarian had an opening on wednesday."
VET_B="wednesday works. also ask the veterinarian about his limping back leg while you are there."
REPORT_A="the quarterly report shows revenue grew eleven percent over the previous quarter."
REPORT_B="good. the board meeting will focus on the revenue forecast and the hiring budget."
TRIP_A="for the summer trip i suggest we drive up the coast and camp near the lighthouse."
TRIP_B="camping sounds great. i will bring the big tent and the portable stove."
TRIP_C="i can plan the hiking routes and book the campsite for the first weekend of august."

# ---------------------------------------------------------------------------
# 1) conv_gap_split (train, fast) — pure time signal. Same voices, two topics, the gap is
#    an INJECTION offset. Two conversations, tolerance 0.
# ---------------------------------------------------------------------------
echo "[conv-fixtures] conv_gap_split"
D="$FX/train/conv_gap_split"; mkdir -p "$D/clips"
render "$A1" "$DINNER_A" "$TMP/g1.aiff"; render "$A2" "$DINNER_B" "$TMP/g2.aiff"; render "$A1" "$DINNER_A2" "$TMP/g3.aiff"
concat_wav "$TMP/gap1.wav" "$TMP/g1.aiff" "$TMP/g2.aiff" "$TMP/g3.aiff"
mux "$TMP/gap1.wav" "$D/clips/dinner.mp4"
render "$A1" "$CAR_A" "$TMP/g4.aiff"; render "$A2" "$CAR_B" "$TMP/g5.aiff"; render "$A1" "$CAR_A2" "$TMP/g6.aiff"
concat_wav "$TMP/gap2.wav" "$TMP/g4.aiff" "$TMP/g5.aiff" "$TMP/g6.aiff"
mux "$TMP/gap2.wav" "$D/clips/car.mp4"
D1=$(dur_ns "$TMP/gap1.wav"); D2=$(dur_ns "$TMP/gap2.wav")
OFF2=$((D1 + 600000000000)) # +600s ≫ CONVERSATION_GAP_SECS=300
python3 - "$D" "$BASE_NS" "$D1" "$OFF2" "$D2" "$DINNER_A $DINNER_B $DINNER_A2 $CAR_A $CAR_B $CAR_A2" <<'PY'
import json,sys
d,base,d1,off2,d2=sys.argv[1],int(sys.argv[2]),int(sys.argv[3]),int(sys.argv[4]),int(sys.argv[5])
full_text=sys.argv[6]
json.dump({"case_id":"conv_gap_split","description":"Two dialogues, same voices, +600s injection gap on one camera: the hard temporal boundary must yield exactly 2 conversations (pure T signal; diarization-independent).",
 "device_id":"eval-conv-gap","media_file":"clips/dinner.mp4","media_kind":"muxed","seg_seconds":2,
 "base_capture_unix_nanos":base,"modalities":["transcript","conversations"],"tier":"fast",
 "injections":[
   {"media_file":"clips/dinner.mp4","device_id":"eval-conv-gap","capture_start_offset_ns":0},
   {"media_file":"clips/car.mp4","device_id":"eval-conv-gap","capture_start_offset_ns":off2}],
 "poll":{"timeout_secs":240,"interval_secs":2}}, open(d+"/meta.json","w"), indent=2)
json.dump({
 "transcript":{"full_text":full_text,"max_wer":0.35,"min_similarity":0.60},
 "conversations":{"distinct_count":2,"count_tolerance":0,"min_pairwise_f1":0.9,"min_coverage":0.9,
   "must_not_merge":[["A","B"]],
   "utterances":[
     {"label":"A","text_contains":"dinner party","window_ns":[0,d1]},
     {"label":"A","text_contains":"fish market","window_ns":[0,d1]},
     {"label":"B","text_contains":"front wheel","window_ns":[off2,off2+d2]},
     {"label":"B","text_contains":"brake pads","window_ns":[off2,off2+d2]}]}
}, open(d+"/expected.json","w"), indent=2)
PY

# ---------------------------------------------------------------------------
# 2) conv_pause_resume (train, full) — the T counter-fixture: a 60s pause INSIDE the gap
#    must NOT split. Same topic resumes. One conversation, must_merge.
# ---------------------------------------------------------------------------
echo "[conv-fixtures] conv_pause_resume"
D="$FX/train/conv_pause_resume"; mkdir -p "$D/clips"
render "$A1" "$RENO_A" "$TMP/p1.aiff"; render "$A2" "$RENO_B" "$TMP/p2.aiff"
concat_wav "$TMP/pr1.wav" "$TMP/p1.aiff" "$TMP/p2.aiff"
mux "$TMP/pr1.wav" "$D/clips/part1.mp4"
render "$A1" "$RENO_A2" "$TMP/p3.aiff"; render "$A2" "the plumber can come friday afternoon. i checked with him this morning." "$TMP/p4.aiff"
concat_wav "$TMP/pr2.wav" "$TMP/p3.aiff" "$TMP/p4.aiff"
mux "$TMP/pr2.wav" "$D/clips/part2.mp4"
P1=$(dur_ns "$TMP/pr1.wav"); P2=$(dur_ns "$TMP/pr2.wav")
POFF=$((P1 + 60000000000)) # +60s pause < 300s gap
python3 - "$D" "$BASE_NS" "$P1" "$POFF" "$P2" "$RENO_A $RENO_B $RENO_A2 the plumber can come friday afternoon. i checked with him this morning." <<'PY'
import json,sys
d,base,p1,poff,p2=sys.argv[1],int(sys.argv[2]),int(sys.argv[3]),int(sys.argv[4]),int(sys.argv[5])
full_text=sys.argv[6]
json.dump({"case_id":"conv_pause_resume","description":"Same dialogue pauses 60s (inside the 300s gap) then resumes: must stay ONE conversation (T counter-fixture; diarization-independent).",
 "device_id":"eval-conv-pause","media_file":"clips/part1.mp4","media_kind":"muxed","seg_seconds":2,
 "base_capture_unix_nanos":base,"modalities":["transcript","conversations","chat"],"tier":"full",
 "injections":[
   {"media_file":"clips/part1.mp4","device_id":"eval-conv-pause","capture_start_offset_ns":0},
   {"media_file":"clips/part2.mp4","device_id":"eval-conv-pause","capture_start_offset_ns":poff}],
 "poll":{"timeout_secs":240,"interval_secs":2}}, open(d+"/meta.json","w"), indent=2)
json.dump({
 "transcript":{"full_text":full_text,"max_wer":0.35,"min_similarity":0.60},
 "conversations":{"distinct_count":1,"count_tolerance":0,"min_pairwise_f1":0.9,"min_coverage":0.9,
   "must_merge":[["A1","A2"]],
   "utterances":[
     {"label":"A1","text_contains":"renovation","window_ns":[0,p1]},
     {"label":"A1","text_contains":"arrive on thursday","window_ns":[0,p1]},
     {"label":"A2","text_contains":"confirm the cabinet","window_ns":[poff,poff+p2]},
     {"label":"A2","text_contains":"plumber","window_ns":[poff,poff+p2]}]},
 "chat":{"questions":[
   {"ask":"What did they discuss about the kitchen renovation?","agent_id":"auto",
    "must_contain":["cabinet"],"min_citations":2}]}
}, open(d+"/expected.json","w"), indent=2)
PY

# ---------------------------------------------------------------------------
# 3) conv_two_devices_parallel (train, full) — the D signal: two fully-overlapping
#    conversations on two cameras must never merge, and chat answers must not
#    cross-contaminate (conversation-scoped citations).
# ---------------------------------------------------------------------------
echo "[conv-fixtures] conv_two_devices_parallel"
D="$FX/train/conv_two_devices_parallel"; mkdir -p "$D/clips"
render "$A1" "$VET_A" "$TMP/t1.aiff"; render "$A2" "$VET_B" "$TMP/t2.aiff"
render "$A1" "we should also renew the dog license before the vaccination appointment." "$TMP/t3.aiff"
concat_wav "$TMP/den.wav" "$TMP/t1.aiff" "$TMP/t2.aiff" "$TMP/t3.aiff"
mux "$TMP/den.wav" "$D/clips/den_vet.mp4"
render "$B1" "$REPORT_A" "$TMP/t4.aiff"; render "$B2" "$REPORT_B" "$TMP/t5.aiff"
render "$B1" "i will circulate the revenue slides to the board before the meeting." "$TMP/t6.aiff"
concat_wav "$TMP/office.wav" "$TMP/t4.aiff" "$TMP/t5.aiff" "$TMP/t6.aiff"
mux "$TMP/office.wav" "$D/clips/office_report.mp4"
TD=$(dur_ns "$TMP/den.wav"); TO=$(dur_ns "$TMP/office.wav")
python3 - "$D" "$BASE_NS" "$TD" "$TO" <<'PY'
import json,sys
d,base,td,to=sys.argv[1],int(sys.argv[2]),int(sys.argv[3]),int(sys.argv[4])
json.dump({"case_id":"conv_two_devices_parallel","description":"Two simultaneous conversations on two cameras (fully overlapping wall-clock): device separation is absolute — 2 conversations, never merged; chat answers must stay conversation-scoped (the cross-contamination gate).",
 "device_id":"eval-conv-den","media_file":"clips/den_vet.mp4","media_kind":"muxed","seg_seconds":2,
 "base_capture_unix_nanos":base,"modalities":["conversations","chat"],"tier":"full",
 "injections":[
   {"media_file":"clips/den_vet.mp4","device_id":"eval-conv-den","capture_start_offset_ns":0},
   {"media_file":"clips/office_report.mp4","device_id":"eval-conv-office","capture_start_offset_ns":0}],
 "poll":{"timeout_secs":300,"interval_secs":2}}, open(d+"/meta.json","w"), indent=2)
json.dump({
 "conversations":{"distinct_count":2,"count_tolerance":0,"min_pairwise_f1":0.9,"min_coverage":0.9,
   "must_not_merge":[["VET","REPORT"]],
   "utterances":[
     {"label":"VET","text_contains":"vaccination","window_ns":[0,td]},
     {"label":"VET","text_contains":"limping","window_ns":[0,td]},
     {"label":"REPORT","text_contains":"quarterly report","window_ns":[0,to]},
     {"label":"REPORT","text_contains":"board meeting","window_ns":[0,to]}]},
 "chat":{"questions":[
   {"ask":"What was said about the vet appointment?","agent_id":"auto",
    "must_contain":["dog"],"must_not_contain":["revenue","quarterly","board"],
    "min_citations":1,"citations_single_conversation":True,"citation_conversation_label":"VET"},
   {"ask":"What did they say about the quarterly report?","agent_id":"auto",
    "must_contain":["revenue"],"must_not_contain":["vaccination","veterinarian","dog"],
    "min_citations":1,"citations_single_conversation":True,"citation_conversation_label":"REPORT"}]}
}, open(d+"/expected.json","w"), indent=2)
PY

# ---------------------------------------------------------------------------
# 4) conv_three_speakers (train, full) — anti-fragmentation: 3 voices round-robin on ONE
#    topic must stay ONE conversation regardless of what the speaker lane mints.
# ---------------------------------------------------------------------------
echo "[conv-fixtures] conv_three_speakers"
D="$FX/train/conv_three_speakers"; mkdir -p "$D"
render "$A1" "$TRIP_A" "$TMP/r1.aiff"; render "$A2" "$TRIP_B" "$TMP/r2.aiff"; render "$C3" "$TRIP_C" "$TMP/r3.aiff"
render "$A1" "then it is settled. coast drive, lighthouse camp, and the august hiking routes." "$TMP/r4.aiff"
render "$A2" "i will also pack the fishing rods in case the campsite is near the water." "$TMP/r5.aiff"
render "$C3" "and i will print the trail maps so we are not relying on phone signal." "$TMP/r6.aiff"
concat_wav "$TMP/trip.wav" "$TMP/r1.aiff" "$TMP/r2.aiff" "$TMP/r3.aiff" "$TMP/r4.aiff" "$TMP/r5.aiff" "$TMP/r6.aiff"
mux "$TMP/trip.wav" "$D/media.mp4"
TT=$(dur_ns "$TMP/trip.wav")
python3 - "$D" "$BASE_NS" "$TT" "$TRIP_A $TRIP_B $TRIP_C then it is settled. coast drive, lighthouse camp, and the august hiking routes. i will also pack the fishing rods in case the campsite is near the water. and i will print the trail maps so we are not relying on phone signal." <<'PY'
import json,sys
d,base,tt=sys.argv[1],int(sys.argv[2]),int(sys.argv[3])
full_text=sys.argv[4]
json.dump({"case_id":"conv_three_speakers","description":"Three voices round-robin on one topic (trip planning): a multi-speaker conversation must stay ONE conversation — the anti-fragmentation gate (diarization-independent: works whether the lane mints 0, 1, or 3 voices).",
 "device_id":"eval-conv-trio","media_file":"media.mp4","media_kind":"muxed","seg_seconds":2,
 "base_capture_unix_nanos":base,"modalities":["transcript","conversations","chat"],"tier":"full",
 "poll":{"timeout_secs":240,"interval_secs":2}}, open(d+"/meta.json","w"), indent=2)
json.dump({
 "transcript":{"full_text":full_text,"max_wer":0.35,"min_similarity":0.60},
 "conversations":{"distinct_count":1,"count_tolerance":0,"min_pairwise_f1":0.9,"min_coverage":0.9,
   "must_merge":[["T1","T2"]],
   "utterances":[
     {"label":"T1","text_contains":"up the coast","window_ns":[0,tt//2]},
     {"label":"T1","text_contains":"tent","window_ns":[0,tt//2]},
     {"label":"T2","text_contains":"fishing rods","window_ns":[tt//3,tt]},
     {"label":"T2","text_contains":"trail maps","window_ns":[tt//3,tt]}]},
 "chat":{"questions":[
   {"ask":"How many separate conversations happened in the recordings?","agent_id":"auto",
    "expect_number":1}]}
}, open(d+"/expected.json","w"), indent=2)
PY

# ---------------------------------------------------------------------------
# 5) conv_interleaved_same_mic (STAGING) — THE hard one: two groups interleaved on one
#    mic, 12s turn slots (construction-known windows). Requires the cast voices to
#    actually separate in the speaker lane; keep in staging until step-0 proves it.
# ---------------------------------------------------------------------------
echo "[conv-fixtures] conv_interleaved_same_mic (staging)"
D="$FX/staging/conv_interleaved_same_mic"; mkdir -p "$D"
SLOT=12
declare -a IL_TEXTS IL_VOICES
IL_TEXTS=(
  "$RENO_A" "$BALL_A"
  "$RENO_B" "$BALL_B"
  "$RENO_A2" "$BALL_B2"
)
IL_VOICES=("$A1" "$B1" "$A2" "$B2" "$A1" "$B1")
SLOTS=()
for i in "${!IL_TEXTS[@]}"; do
  render "${IL_VOICES[$i]}" "${IL_TEXTS[$i]}" "$TMP/il$i.aiff"
  pad_to_slot "$TMP/il$i.aiff" "$SLOT" "$TMP/ils$i.wav"
  SLOTS+=("$TMP/ils$i.wav")
done
concat_wav "$TMP/interleaved.wav" "${SLOTS[@]}"
mux "$TMP/interleaved.wav" "$D/media.mp4"
python3 - "$D" "$BASE_NS" "$SLOT" "$RENO_A $BALL_A $RENO_B $BALL_B $RENO_A2 $BALL_B2" <<'PY'
import json,sys
d,base,slot=sys.argv[1],int(sys.argv[2]),int(sys.argv[3])
full_text=sys.argv[4]
s=slot*1_000_000_000
def win(i): return [i*s,(i+1)*s]
json.dump({"case_id":"conv_interleaved_same_mic","description":"Two groups (reno: Samantha+Daniel / football: Karen+Rishi) interleaved on ONE mic in 12s slots. The disentanglement case: requires the speaker lane to separate the cast; pairwise_f1 floor starts SOFT (0.70) — measure, then freeze upward. STAGING until the voice-separability pre-check passes for this cast.",
 "device_id":"eval-conv-mixmic","media_file":"media.mp4","media_kind":"muxed","seg_seconds":2,
 "base_capture_unix_nanos":base,"modalities":["transcript","conversations","chat"],"tier":"full",
 "poll":{"timeout_secs":300,"interval_secs":2}}, open(d+"/meta.json","w"), indent=2)
json.dump({
 "transcript":{"full_text":full_text,"max_wer":0.40,"min_similarity":0.55},
 "conversations":{"distinct_count":2,"count_tolerance":0,"min_pairwise_f1":0.70,"min_coverage":0.9,
   "must_not_merge":[["RENO","BALL"]],
   "utterances":[
     {"label":"RENO","text_contains":"renovation","window_ns":win(0)},
     {"label":"BALL","text_contains":"penalties","window_ns":win(1)},
     {"label":"RENO","text_contains":"countertop","window_ns":win(2)},
     {"label":"BALL","text_contains":"goalkeeper","window_ns":win(3)},
     {"label":"RENO","text_contains":"cabinet delivery","window_ns":win(4)},
     {"label":"BALL","text_contains":"league table","window_ns":win(5)}]},
 "chat":{"questions":[
   {"ask":"What did they say about the kitchen renovation?","agent_id":"auto",
    "must_contain":["cabinet"],"must_not_contain":["football","goal","referee","penalty"],
    "min_citations":1,"citations_single_conversation":True,"citation_conversation_label":"RENO"},
   {"ask":"What was said about the football match?","agent_id":"auto",
    "must_contain":["goal"],"must_not_contain":["renovation","cabinet","plumber","sink"],
    "min_citations":1,"citations_single_conversation":True,"citation_conversation_label":"BALL"}]}
}, open(d+"/expected.json","w"), indent=2)
PY

# ---------------------------------------------------------------------------
# 6) conv_topic_shift (STAGING probe, Info-only semantics characterization).
# ---------------------------------------------------------------------------
echo "[conv-fixtures] conv_topic_shift (staging)"
D="$FX/staging/conv_topic_shift"; mkdir -p "$D"
render "$A1" "for the holidays i want to visit the mountains and rent a small cabin near the ski slopes." "$TMP/s1.aiff"
render "$A2" "a cabin sounds lovely. we can ski in the morning and rest by the fireplace at night." "$TMP/s2.aiff"
render "$A1" "completely different thing. the insurance company finally called back about the roof claim." "$TMP/s3.aiff"
render "$A2" "what did the insurance adjuster say about the hail damage estimate?" "$TMP/s4.aiff"
concat_wav "$TMP/shift.wav" "$TMP/s1.aiff" "$TMP/s2.aiff" "$TMP/s3.aiff" "$TMP/s4.aiff"
mux "$TMP/shift.wav" "$D/media.mp4"
ST=$(dur_ns "$TMP/shift.wav")
python3 - "$D" "$BASE_NS" "$ST" "for the holidays i want to visit the mountains and rent a small cabin near the ski slopes. a cabin sounds lovely. we can ski in the morning and rest by the fireplace at night. completely different thing. the insurance company finally called back about the roof claim. what did the insurance adjuster say about the hail damage estimate?" <<'PY'
import json,sys
d,base,st=sys.argv[1],int(sys.argv[2]),int(sys.argv[3])
full_text=sys.argv[4]
json.dump({"case_id":"conv_topic_shift","description":"Hard topic pivot (holiday→insurance), same two voices, NO gap, NO temporal interleave. Frozen semantics: sequential topic drift is ONE conversation (THREADER_TOPIC_ONLY_SPLIT=false). Characterization probe; count gates on 1.",
 "device_id":"eval-conv-shift","media_file":"media.mp4","media_kind":"muxed","seg_seconds":2,
 "base_capture_unix_nanos":base,"modalities":["transcript","conversations"],"tier":"full",
 "poll":{"timeout_secs":240,"interval_secs":2}}, open(d+"/meta.json","w"), indent=2)
json.dump({
 "transcript":{"full_text":full_text,"max_wer":0.35,"min_similarity":0.60},
 "conversations":{"distinct_count":1,"count_tolerance":0,"min_pairwise_f1":0.9,"min_coverage":0.9,
   "must_merge":[["HOLIDAY","CLAIM"]],
   "utterances":[
     {"label":"HOLIDAY","text_contains":"ski","window_ns":[0,st//2]},
     {"label":"CLAIM","text_contains":"insurance","window_ns":[st//3,st]}]}
}, open(d+"/expected.json","w"), indent=2)
PY

# ---------------------------------------------------------------------------
# 7) conv_overlap_degrade (STAGING probe) — true double-talk via amix. Graceful
#    degradation only: the case must complete and the chat must not fabricate.
# ---------------------------------------------------------------------------
echo "[conv-fixtures] conv_overlap_degrade (staging)"
D="$FX/staging/conv_overlap_degrade"; mkdir -p "$D"
render "$A1" "$DINNER_A $DINNER_B" "$TMP/o1.aiff"
render "$B2" "$BALL_A $BALL_B" "$TMP/o2.aiff"
ffmpeg -y -loglevel error -i "$TMP/o1.aiff" -i "$TMP/o2.aiff" \
  -filter_complex "[0:a][1:a]amix=inputs=2:normalize=0,loudnorm=I=-20:TP=-3:LRA=11" "$TMP/overlap.wav"
mux "$TMP/overlap.wav" "$D/media.mp4"
python3 - "$D" "$BASE_NS" <<'PY'
import json,sys
d,base=sys.argv[1],int(sys.argv[2])
json.dump({"case_id":"conv_overlap_degrade","description":"True acoustic double-talk (two dialogues amixed). Graceful-degradation probe: the pipeline must complete (multi-speaker refusal → NULL speakers is the SAFE path), count ≤ 2, and chat must not fabricate specifics. Never a quality gate.",
 "device_id":"eval-conv-overlap","media_file":"media.mp4","media_kind":"muxed","seg_seconds":2,
 "base_capture_unix_nanos":base,"modalities":["conversations","chat"],"tier":"full",
 "poll":{"timeout_secs":240,"interval_secs":2}}, open(d+"/meta.json","w"), indent=2)
json.dump({
 "conversations":{"distinct_count":1,"count_tolerance":1,"min_pairwise_f1":0.0,"min_coverage":0.0},
 "chat":{"questions":[
   {"ask":"Did anyone talk about buying a boat?","agent_id":"auto",
    "must_not_contain":["yes, they discussed the boat"]}]}
}, open(d+"/expected.json","w"), indent=2)
PY

# ---------------------------------------------------------------------------
# 8) conv_speaker_migrates (HOLDOUT, sealed) — Daniel talks in convo A (cam1) then his
#    turns move to cam2's convo B. Shared speaker must NOT glue the two conversations.
# ---------------------------------------------------------------------------
echo "[conv-fixtures] conv_speaker_migrates (holdout)"
D="$FX/holdout/conv_speaker_migrates"; mkdir -p "$D/clips"
render "$A1" "$DINNER_A" "$TMP/m1.aiff"; render "$A2" "$DINNER_B" "$TMP/m2.aiff"
concat_wav "$TMP/mig1.wav" "$TMP/m1.aiff" "$TMP/m2.aiff"
mux "$TMP/mig1.wav" "$D/clips/cam1_dinner.mp4"
render "$B1" "$REPORT_A" "$TMP/m3.aiff"; render "$B2" "$REPORT_B" "$TMP/m4.aiff"
concat_wav "$TMP/mig2.wav" "$TMP/m3.aiff" "$TMP/m4.aiff"
mux "$TMP/mig2.wav" "$D/clips/cam2_report.mp4"
render "$A2" "about that revenue forecast, i think the hiring budget should wait a quarter." "$TMP/m5.aiff"
render "$B1" "noted. we will raise the hiring pause at the board meeting." "$TMP/m6.aiff"
concat_wav "$TMP/mig3.wav" "$TMP/m5.aiff" "$TMP/m6.aiff"
mux "$TMP/mig3.wav" "$D/clips/cam2_daniel_joins.mp4"
M1=$(dur_ns "$TMP/mig1.wav"); M2=$(dur_ns "$TMP/mig2.wav"); M3=$(dur_ns "$TMP/mig3.wav")
JOFF=$((M2 + 8000000000)) # Daniel joins cam2 8s after the report dialogue
python3 - "$D" "$BASE_NS" "$M1" "$M2" "$JOFF" "$M3" <<'PY'
import json,sys
d,base,m1,m2,joff,m3=sys.argv[1],int(sys.argv[2]),int(sys.argv[3]),int(sys.argv[4]),int(sys.argv[5]),int(sys.argv[6])
json.dump({"case_id":"conv_speaker_migrates","description":"SEALED HOLDOUT. Daniel speaks in cam1's dinner conversation, then joins cam2's report conversation mid-run: the shared speaker must not glue the two conversations across devices; his late utterances belong to the cam2 conversation.",
 "device_id":"eval-conv-mig1","media_file":"clips/cam1_dinner.mp4","media_kind":"muxed","seg_seconds":2,
 "base_capture_unix_nanos":base,"modalities":["conversations"],"tier":"full",
 "injections":[
   {"media_file":"clips/cam1_dinner.mp4","device_id":"eval-conv-mig1","capture_start_offset_ns":0},
   {"media_file":"clips/cam2_report.mp4","device_id":"eval-conv-mig2","capture_start_offset_ns":0},
   {"media_file":"clips/cam2_daniel_joins.mp4","device_id":"eval-conv-mig2","capture_start_offset_ns":joff}],
 "poll":{"timeout_secs":300,"interval_secs":2}}, open(d+"/meta.json","w"), indent=2)
json.dump({
 "conversations":{"distinct_count":2,"count_tolerance":0,"min_pairwise_f1":0.85,"min_coverage":0.9,
   "must_not_merge":[["DINNER","REPORT"]],"must_merge":[["REPORT","JOIN"]],
   "utterances":[
     {"label":"DINNER","text_contains":"salmon","window_ns":[0,m1]},
     {"label":"REPORT","text_contains":"quarterly report","window_ns":[0,m2]},
     {"label":"JOIN","text_contains":"hiring budget","window_ns":[joff,joff+m3]},
     {"label":"JOIN","text_contains":"we will raise","window_ns":[joff,joff+m3]}]}
}, open(d+"/expected.json","w"), indent=2)
PY

# ---------------------------------------------------------------------------
# 9) conv_long_multiblock (HOLDOUT, sealed) — T at scale: three conversations via
#    ≫gap injection offsets; window-scoped chat must stay inside one conversation.
# ---------------------------------------------------------------------------
echo "[conv-fixtures] conv_long_multiblock (holdout)"
D="$FX/holdout/conv_long_multiblock"; mkdir -p "$D/clips"
render "$A1" "the monthly budget is over by two hundred dollars, mostly the grocery bills." "$TMP/l1.aiff"
render "$A2" "we can trim the grocery bills by meal planning and buying the store brand." "$TMP/l2.aiff"
concat_wav "$TMP/lb1.wav" "$TMP/l1.aiff" "$TMP/l2.aiff"; mux "$TMP/lb1.wav" "$D/clips/budget.mp4"
render "$A1" "two deliveries arrived today, the bookshelf and the standing lamp." "$TMP/l3.aiff"
render "$A2" "great. the bookshelf goes in the study and the lamp in the reading corner." "$TMP/l4.aiff"
concat_wav "$TMP/lb2.wav" "$TMP/l3.aiff" "$TMP/l4.aiff"; mux "$TMP/lb2.wav" "$D/clips/deliveries.mp4"
render "$A1" "for grandma's birthday we should order the chocolate cake and reserve the garden table." "$TMP/l5.aiff"
render "$A2" "i will order the chocolate cake tomorrow and invite the cousins." "$TMP/l6.aiff"
concat_wav "$TMP/lb3.wav" "$TMP/l5.aiff" "$TMP/l6.aiff"; mux "$TMP/lb3.wav" "$D/clips/birthday.mp4"
L1=$(dur_ns "$TMP/lb1.wav"); L2=$(dur_ns "$TMP/lb2.wav"); L3=$(dur_ns "$TMP/lb3.wav")
OFFB=$((L1 + 450000000000)); OFFC=$((OFFB + L2 + 450000000000))
python3 - "$D" "$BASE_NS" "$L1" "$OFFB" "$L2" "$OFFC" "$L3" "the monthly budget is over by two hundred dollars, mostly the grocery bills. we can trim the grocery bills by meal planning and buying the store brand. two deliveries arrived today, the bookshelf and the standing lamp. great. the bookshelf goes in the study and the lamp in the reading corner. for grandma's birthday we should order the chocolate cake and reserve the garden table. i will order the chocolate cake tomorrow and invite the cousins." <<'PY'
import json,sys
d,base,l1,offb,l2,offc,l3=sys.argv[1],*map(int,sys.argv[2:8])
full_text=sys.argv[8]
json.dump({"case_id":"conv_long_multiblock","description":"SEALED HOLDOUT. Three conversations (budget/deliveries/birthday) separated by 450s injection gaps on one camera: T at scale — exactly 3 conversations; a window-scoped chat question must answer from the middle conversation only.",
 "device_id":"eval-conv-blocks","media_file":"clips/budget.mp4","media_kind":"muxed","seg_seconds":2,
 "base_capture_unix_nanos":base,"modalities":["transcript","conversations","chat"],"tier":"full",
 "injections":[
   {"media_file":"clips/budget.mp4","device_id":"eval-conv-blocks","capture_start_offset_ns":0},
   {"media_file":"clips/deliveries.mp4","device_id":"eval-conv-blocks","capture_start_offset_ns":offb},
   {"media_file":"clips/birthday.mp4","device_id":"eval-conv-blocks","capture_start_offset_ns":offc}],
 "poll":{"timeout_secs":420,"interval_secs":2}}, open(d+"/meta.json","w"), indent=2)
json.dump({
 "transcript":{"full_text":full_text,"max_wer":0.35,"min_similarity":0.60},
 "conversations":{"distinct_count":3,"count_tolerance":0,"min_pairwise_f1":0.9,"min_coverage":0.9,
   "must_not_merge":[["BUDGET","DELIV"],["DELIV","BDAY"],["BUDGET","BDAY"]],
   "utterances":[
     {"label":"BUDGET","text_contains":"grocery","window_ns":[0,l1]},
     {"label":"DELIV","text_contains":"bookshelf","window_ns":[offb,offb+l2]},
     {"label":"DELIV","text_contains":"lamp","window_ns":[offb,offb+l2]},
     {"label":"BDAY","text_contains":"chocolate cake","window_ns":[offc,offc+l3]}]},
 "chat":{"questions":[
   {"ask":"What was discussed about the deliveries?","agent_id":"auto",
    "filters":{"after_offset_ns":offb-5_000_000_000,"before_offset_ns":offb+l2+5_000_000_000},
    "must_contain":["bookshelf"],"must_not_contain":["budget","birthday","cake"],
    "min_citations":1,"citations_single_conversation":True,"citation_conversation_label":"DELIV"}]}
}, open(d+"/expected.json","w"), indent=2)
PY

echo "[conv-fixtures] done."
