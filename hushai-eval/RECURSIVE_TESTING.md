# Recursive Testing — Agent Playbook

**Audience: a future agent (or human) working in this repo.** This is the how-to for HushAI's
recursive end-to-end testing loop: inject/capture *known* media through the **live** pipeline, score
the results against ground truth, and get a machine-readable improvement/regression verdict. Use it
to validate any change to the capture→worker→DB→read-API path before declaring it done.

Reference docs: `hushai-eval/README.md` (quick reference), `AGENTS.md` (orientation + findings).
This file is the actionable playbook; if they disagree, trust this file + the code.

---

## When to use it

- You changed anything in `hushai-worker` (ASR, speaker, sentiment, VAD, vision, events), the ingest
  path (`hushai-backend`), or the RAG read path → run the loop to prove you didn't regress quality.
- You're implementing a feature and want to "define a task and validate in real time" → this is that loop.
- You want to add a real-world test case → use the `probe` labeling flow (§3).
- You're debugging a perception bug → the harness + the per-segment instrumentation localize it (see §6).

It is NOT a unit-test replacement. It exercises the *real* models and DB; it answers "did the system's
perception get better or worse," which unit tests can't.

---

## Mental model: two tiers

```
Tier 1  DETERMINISTIC INJECTION  (the regression backbone — run constantly)
        known clip ──feed_segments.py──▶ POST /v1/segments ──▶ worker ──▶ Postgres
                                                                            │
        ground truth (expected.json) ◀── score ◀── query (DB) ◀────────────┘
        → improvement / regression / unchanged verdict + exit code.  Fully reproducible.

Tier 2  PHYSICAL CAMERA-AT-SCREEN  (the realism gate — run before "done")
        clip played FULLSCREEN ──▶ phone camera+mic ──▶ real encoder/uploader ──▶ same pipeline
        → tolerant presence/recall verdict.  Exercises what injection can't (sensor, mic, encoder).
```

Tier 1 (`cargo run -p hushai-eval`) is the workhorse: deterministic, fast, exact. Tier 2
(`local_dev/physical_loopback.py`) is the periodic realism check: noisy, tolerant, requires the phone.

---

## 0. Prerequisites & bring-up (do this first, every session)

Everything runs against an **isolated `hushai_test` database** (the harness TRUNCATEs result tables
each run — it refuses any DB whose name lacks `_test`). One-time:

```bash
createdb hushai_test
DATABASE_URL=postgres://mf@localhost:5432/hushai_test sqlx migrate run --source hushai-backend/migrations
./local_dev/build_fixtures.sh        # synthetic (macOS `say`) fixture media
./local_dev/fetch_eval_clips.sh      # real public-domain clip media (JFK/FDR/Armstrong)
```

Bring up the stack with the **determinism profile** (`local_dev/eval.env`). The verified manual launch
(run each from the repo root unless noted):

```bash
# backend (ingest) — CWD = hushai-backend
( cd hushai-backend && DATABASE_URL=postgres://mf@localhost:5432/hushai_test SQLX_OFFLINE=true \
    ../target/debug/hushai-backend ) &

# worker — CWD = repo root. The env below IS the determinism lockdown + vision wiring.
DATABASE_URL=postgres://mf@localhost:5432/hushai_test SQLX_OFFLINE=true RUST_LOG=info \
  WORKER_CONCURRENCY=1 SPEAKER_AUTOHEAL_ENABLED=false SPEAKER_BACKFILL_ON_START=false \
  SPEAKER_REPROCESS_REJECTS_ON_START=false POLL_INTERVAL_SECS=2 VISION_COREML=false \
  VISION_MOTION_SKIP_ENABLED=false \
  ORT_DYLIB_PATH="$PWD/models/onnxruntime/onnxruntime-osx-arm64-1.20.0/lib/libonnxruntime.1.20.0.dylib" \
  DYLD_FALLBACK_LIBRARY_PATH="$PWD/target/debug/deps:$PWD/target/debug:/usr/local/lib:/usr/lib" \
  ./target/debug/hushai-worker &
```
- `VISION_MOTION_SKIP_ENABLED=false` is **mandatory for determinism** (now in `eval.env`): the
  skip-static gate is stateful PER CAMERA, so a re-injected STATIC clip is skipped on the 2nd+ run (its
  frames match the last analyzed frame) → vision goes non-deterministic across reruns. A `plate_ocr`
  static fixture flaked 1.0/0.0/0.0 until this was set. (Ken-burns fixtures like `car_object` dodged it
  by having motion, which is why they didn't flake.)

- `DYLD_FALLBACK_LIBRARY_PATH` is **mandatory** (sherpa's bundled onnxruntime; the worker dies without it).
- `ORT_DYLIB_PATH` enables the vision lanes (face + object). Omit `VISION_ENABLED` (defaults on).
- Confirm in the worker log: `vision object lane enabled` + `vision enabled (face identity)`.
- `./local_dev/run_stack.sh --test-db` is the one-command convenience (also starts rag+viewer), but
  confirm it exported `ORT_DYLIB_PATH` (it may not) — if the worker log shows the object lane disabled,
  use the manual launch above.
- **Gotcha:** a worker/backend launched with a bare `&` inside a one-shot shell may be reaped when that
  shell exits. For long sessions launch it durably (a background task runner, `nohup …&`, or the
  `local_dev/com.hushai.worker.plist` launchd job) and re-check it's alive before each run.

Ollama must be up (embeddings + sentiment + RAG). Postgres on :5432.

---

## 1. The agent validation loop (define a task → validate it)

```
1. ESTABLISH BASELINE (once, or after an intentional pipeline/model change):
     cargo run -p hushai-eval -- run --tier full --fixtures all --update-baseline

2. INNER LOOP (fast, after each code change):
     <make your change>  →  cargo build -p <touched crate>
     cargo run -p hushai-eval -- run --tier fast --json
       exit 0 + no "regression" classifications  → candidate good; iterate toward your target metric
       exit 1 (regression / floor breach)        → fix it before continuing
       exit 2 (inconclusive / infra)             → stack/processing problem, not a result (see §7)

3. FULL GATE before declaring done (includes the sealed holdout split):
     cargo run -p hushai-eval -- run --tier full --fixtures all      # must be exit 0

4. (optional) REALISM GATE — physical capture (§2 of README / below).
```

Stop condition is **machine-checked**: your target metric improved AND nothing regressed, on train AND
holdout. The exit code lets a `/loop`, a Stop-hook, or a CI step gate iteration without re-reading prose.

**Verdict semantics** (`--json` → `{verdict, exit_code, config_hash, cases[]}`; each metric has
`{value, baseline, delta, classification, floor_ok}`):
- `pass` (0) — all metrics within band, floors met.
- `pass_improved` (0) — a metric improved, none regressed.
- `fail` (1) — a metric regressed beyond its band, OR breached its absolute floor in `expected.json`.
- `inconclusive` (2) — processing didn't complete / infra problem. **Never scored** — fix and re-run.

---

## 2. Command reference

```bash
# Tier 1 — deterministic suite
cargo run -p hushai-eval -- run --tier {fast|full} [--fixtures train|holdout|all] \
    [--case <case_id>] [--update-baseline [--force]] [--json]

# Labeling — run ONE clip through the live pipeline, print what it heard, draft a fixture (§3)
cargo run -p hushai-eval -- probe --audio <clip.wav|mp4> [--case NAME] [--vision]

# Tier 2 — physical camera-at-screen, scored (needs the phone plugged in, pointed at the screen)
python3 local_dev/physical_loopback.py --media <file|image> --case NAME --duration 25 \
    [--audio-only] [--expect-objects a,b] [--expect-text k1,k2] [--expect-face] \
    [--scenario <case_id>]
#   → plays the media fullscreen (ffplay), drives the phone (adb), waits for both lanes,
#     scores TOLERANTLY (presence/recall) → PASS / DEGRADED / FAIL.
#   --scenario also fires that fixture's `chat` questions at the LIVE RAG over the phone capture
#     (routing + must_contain/must_not_contain + min_citations); exact counts are NOT gated here
#     (optical/acoustic capture is lossy) — those stay the Tier-1 job. Needs RAG_TOKEN (pinned in
#     eval.env to dev-rag-token; run_stack.sh mints a random one otherwise → 401).

# Regenerate fixture media (committed = ground truth + recipe; media is gitignored)
./local_dev/build_fixtures.sh        # synthetic say-TTS clips
./local_dev/fetch_eval_clips.sh      # real public-domain clips
./local_dev/provision_vision.sh      # export RF-DETR + CLIP (object lane) into models/
```

---

## 3. Adding a test case (the human-verified labeling loop)

This is how the corpus grows. The pipeline's *own output* becomes the draft; a human corrects it; the
correction is frozen as ground truth. Steps:

```
1. cargo run -p hushai-eval -- probe --audio yourclip.wav --case my_case
     → prints transcript / speakers / sentiment / events / (objects/faces with --vision)
     → writes hushai-eval/fixtures/staging/my_case/{media.mp4,meta.json,expected.json}
       (expected.json PRE-FILLED from the pipeline's output)
2. A HUMAN reviews the printout and tells you what's right/wrong.
3. Edit expected.json to the CORRECT answer (the "way it should be"). Set:
     - transcript.full_text (human-verified words), max_wer / min_similarity floors
     - speakers.distinct_count, sentiment windows, persons/objects/plates/events as applicable
     - keep `modalities` to only the lanes you're scoring (a lane can stay unscored — see two_speakers)
4. Move staging/my_case → fixtures/train/my_case  (or holdout/ for a sealed counter-fixture)
5. cargo run -p hushai-eval -- run --tier full --case my_case --update-baseline   # establish baseline
```

**Ground truth is the human's judgment, never the system's own output left unchecked.** That's what
makes a regression meaningful. For a real-audio clip, add its fetch+trim recipe to
`fetch_eval_clips.sh` so the (gitignored) media is reproducible.

Fixture format: `meta.json` (device_id, `base_capture_unix_nanos`, `media_kind`, `seg_seconds`,
`modalities[]`, `tier`, `poll`) + `expected.json` (per-modality, every key optional; time windows are
ns offsets from the base). See `hushai-eval/src/fixtures.rs` for the full schema.

---

## 4. The seven trust invariants (DO NOT WEAKEN THESE)

The pipeline is online, stateful, self-healing, time-anchored, concurrent — each property silently
fakes a result unless neutralized. The harness enforces all seven; if you change the harness, preserve them:

1. **Clean pinned state** — `TRUNCATE … RESTART IDENTITY CASCADE` of all result/catalog/status/`segments`
   tables before each case (`src/reset.rs`). Catalog residue changes speaker/face match-vs-mint.
2. **Determinism lockdown** — `WORKER_CONCURRENCY=1`, auto-merge/backfill/reprocess off, CPU EP (eval.env).
3. **Fixture-pinned timestamps** — injector stamps a fixed `capture_start_unix_nanos`; never score
   wall-clock-relative output (`time_label`, "today/yesterday").
4. **Config-hash gate** — every run hashes models + Ollama digests + ORT + EP + knobs; baselines are
   keyed by it, so a model/knob change starts a new lineage instead of a bogus regression (`src/manifest.rs`).
5. **Quiescent completion** — wait until every segment is terminal in **both** lane status tables, then
   until event counts stop changing; timeout → inconclusive, never scored (`src/poll.rs`).
6. **Assignment-invariant scoring** — identity metrics use optimal label assignment + denormalized names,
   never minted UUIDs; float metrics use calibrated tolerance bands (`src/score.rs`, `src/baseline.rs`).
7. **Sealed holdout + counter-fixtures** — `fixtures/holdout/` scored only in the full gate; includes
   adversarial cases (e.g. `silence_no_speech` must mint **0** speakers).

**Cardinal rule: never "fix" a failing test by loosening a model gate/threshold to make the fixture pass.**
That's metric-gaming — it trades a green test for a real-world regression. If a gate seems wrong,
instrument it, gather data across diverse real captures, and calibrate to a *separating* threshold
(see the speaker-gate story in §6 for exactly how this played out).

---

## 5. Determinism profile & config-hash

`local_dev/eval.env` pins the knobs that otherwise make the same input produce different output. The
**config-hash** folds the determinism-relevant surface (model file fingerprints, Ollama digests, ORT
dylib, execution provider, matcher/threshold knobs) into one SHA that keys `baselines/<config-hash>/`.

Consequences you must understand:
- An ordinary **code change keeps the hash stable** → baseline comparisons stay valid (this is the point).
- Changing a **model, a threshold env, or provisioning a new lane changes the hash** → a NEW baseline
  lineage. You'll see metrics classified `new` (no baseline). Re-establish with `--update-baseline`.
  (e.g. provisioning the object lane on 2026-06-30 changed the hash — re-baseline after such changes.)
- The git SHA and migration head are recorded for attribution but deliberately NOT in the hash.

---

## 6. Current coverage & known findings (as of 2026-06-30)

**Lanes covered DETERMINISTICALLY (Tier 1):** ASR (transcript), speaker-count, sentiment, events,
**objects** (`car_object` → COCO-91 decode), **plates** (`plate_ocr` → full ALPR: detect→OCR→"EMD774"),
**faces** (`face_id` → SCRFD detect + ArcFace identity, distinct_count=1). ALL THREE vision lanes now
have a Tier-1 guard. Fixtures: `asr_short, two_speakers, jfk_moon, fdr_infamy, armstrong_step, car_object,
plate_ocr, face_id` (train) + `silence_no_speech` (holdout); all committable (media regenerated by
`fetch_eval_clips.sh` from PD Wikimedia sources — Peugeot iOn / Auckland street / Judith Resnik portrait).
NOTE for vision fixtures: prefer KEN-BURNS media (motion) or ensure `VISION_MOTION_SKIP_ENABLED=false`, else
a static clip is skip-static-gated on reruns (see §0).

**Harness-driven wins / open gaps (the recursive loop working):**
- ✅ **RF-DETR object-decode bug — found & fixed (2026-06-30).** Physical capture + direct injection of a
  clear person portrait both labeled it `bicycle/sports ball/refrigerator`. Root cause: the real export is
  `labels[1,300,91]` (C=91) where the class COLUMN INDEX is the COCO category id, but `objects.rs` mapped it
  through a **dense COCO-80** table → person(col 1)→"bicycle". Fixed to the COCO-91 layout
  (`coco91_class_names`, skips background/gap cols, loads `rf-detr-classes.json`); portrait now → `person`.
  Verified deterministically (`probe --vision`) AND live (phone-at-screen → `person`). Unit: `coco91_column_map_is_correct`.
- ✅ **Speaker VAD bug — found & fixed.** Harness showed 0 speakers everywhere (constant ~0.31s).
  `speaker.rs::detect()` fed the whole buffer to Silero VAD in one call → 1 tiny segment. Fixed to
  feed 512-sample windows + drain. JFK now mints 1 voice; `jfk_moon`/`silence_no_speech` gate it.
- ✅ **Face small/through-screen miss — FIXED by provisioning SCRFD (2026-06-30).** YuNet (the fallback,
  used because SCRFD wasn't provisioned) captured a screen-displayed face as **0** live detections;
  `fetch_scrfd.sh` → `scrfd_10g_bnkps.onnx` made the worker's intended default detector load and the SAME
  live phone capture now yields **3** faces (loopback PASS), with the easy direct-injection case still 1.
- ◐ **ASR hallucination tail (live) — knobs added, default unchanged; LOOP CAUGHT A REGRESSION.** JFK
  live capture appended invented text ("…we're going to go back to the end of the video. We have Bob.")
  after real speech. Added tunable `WHISPER_*` decode-quality params (`asr.rs::DecodeQuality`:
  no_speech/logprob/entropy/suppress_nst). Synthetic non-speech (noise/tone) does NOT repro — the Silero
  silence-gate already drops it — so there's no stable deterministic fixture for the looped-real-speech
  case. Crucially the eval caught that flipping `suppress_nst=true` REGRESSED `fdr_infamy` WER 0.562→0.625
  (it shifts whisper's greedy path on noisy audio), so the shipped default is whisper-default (zero
  behavior change). The knobs are a CALIBRATION surface — tune per-deployment + re-run the eval. Textbook
  "instrument + calibrate from data, never blind-tune."
- ⚠ **Speaker calibration on real room audio (OPEN, do not blind-tune).** Physical capture mints 0
  speakers: per-segment instrumentation (`process.rs:294`, logs speech_secs/voiced_frac/snr_db/quality
  for EVERY segment — use it) shows segments are `AttachOnly` because BOTH gates fail (voiced_frac
  ~0.37–0.40 < 0.5, snr_db ~2–2.7 < 3.0; the speaker-window aggregating non-speech neighbors deflates
  voiced_frac). An adversarial analysis proved lowering the gates makes a TV/laptop playing dialogue
  mint a SPURIOUS speaker. A safe calibration needs a LABELED real-capture set (proximate person vs TV
  vs music vs HVAC) and/or computing voiced_frac on the current segment, not the window.
- ⚠ **`two_speakers` merges 2 voices → 1** (short TTS turns + window crossing the boundary). Diarization
  staged off its `modalities`; target documented in its `expected.json`.
- ⚠ **Sentiment is noisy**; not gated tightly.
- ⚠ **PHYSICAL-RIG AIM caveat (2026-07-01) — verify the captured frame, don't trust a live MISS.** A live
  scenario sweep MISSED all objects on multi-object scenes (street traffic, living room) while single large
  subjects (car, person) PASSED. Root cause was NOT the model: extracting an actual captured frame (see the
  technique below) showed the phone was **rotated 90° AND framing the Mac keyboard in ~40% of the frame**,
  with window glare — so complex scenes land small/sideways/partial and fall below threshold. The model is
  verified correct on the SAME media via clean injection (street→`car×82,truck×27`; room→
  `chair/couch/tv/potted plant/handbag` — all COCO-91-correct). **Lesson: a Tier-2 MISS is inconclusive
  until you've looked at what the phone captured.** To do that: `psql … "SELECT blob_uri FROM segments
  WHERE device_id LIKE 'android%' AND media_type IN (2,3) ORDER BY received_at DESC LIMIT 1"` → the uri is a
  `file://` blob path → `ffmpeg -i <blob> -frames:v 1 frame.jpg` → look at it before concluding a live
  regression. **UPDATE (2026-07-01): the 90° ROTATION is now fixed in-app** — `OrientationTracker`
  (accelerometer, works screen-off) + `SegmentMuxer.setOrientationHint()` stamp each segment's MP4
  rotation matrix so capture is UPRIGHT at any phone orientation; the worker's ffmpeg autorotates on it
  (verified: a captured frame now reads upright, 1280×720→720×1280). The remaining rig concern is AIM/
  framing (don't let the keyboard/desk fill the frame), not rotation.
- ✅ **Object NMS + recall (2026-06-30).** Added class-aware `geom::nms_by` to the object lane
  (`OBJECT_NMS_IOU=0.5`) — RF-DETR's 300-query head had zero dedup, the lone detector lane missing it.
  Raised `FRAMES_PER_SEGMENT` 2→3 (recall). Both verified: `car_object` F1 stays 1.000, audio unchanged.
- ✅ **ALPR WORKING END-TO-END (2026-07-01).** Detector = open-image-models `yolo-v9-t-640-license-plate`
  (MIT, plain ONNX; `fetch_plate_detector.sh` default URL). Verified on a real plate: detect conf ~0.90 →
  OCR "EMD774" (exact) → minted to `license_plates`. Fixes that got it working: (1) end2end `[N,7]` xyxy
  decode + centered 114 letterbox (`PLATE_DETECT_END2END=true`); (2) OCR input is **UINT8** not f32
  (`ocr.rs` auto-detects dtype — feeding f32 made ORT reject the run); (3) fixed-length softmax head, conf
  = max prob (`PLATE_OCR_CTC=false`); (4) `PLATE_DETECT_WHOLE_FRAME` now scans the whole frame too, not
  only when no vehicle. Cross-checked my Rust decode vs open-image-models' reference (byte-identical box).
- ◐ **(historical) Plates — OCR DONE, detector remaining (2026-06-30).** fast-plate-ocr's API had moved
  (`ONNXPlateRecognizer`→`LicensePlateRecognizer`); `export_plate_ocr.py` adapted, OCR provisioned
  (`models/lp_ocr_cct.onnx` NHWC `[1,64,128,3]`→`[1,9,37]` + `lp_ocr_charset.json`). The CCT head is
  FIXED-LENGTH not CTC → added `PLATE_OCR_CTC` (default false) so double letters survive; `greedy_decode`
  unit-tested. Added the `inspect_plate_model_io_shapes` validation test. **Blocker:** the plate DETECTOR
  needs an operator-authorized model (auto-loading a 3rd-party `.pt` is pickle-RCE-blocked + a licensing
  call) — set `PLATE_DETECTOR_ONNX_URL` / `yolo export` a YOLOv8/11 LP model → validate with the inspect test.

### RAG-CHAT ANSWER scoring — the PI-workflow layer (2026-07-01)

The harness now scores the **RAG chat answer itself** (not just perception). A fixture with the `chat`
(alias `rag`) modality carries an `Expected.chat.questions[]`; per question the harness POSTs to the live
`/v1/rag/chat`, parses the SSE, and scores **deterministic-first** assertions that survive LLM wording:
`must_contain` / `must_not_contain` / `expect_number` (digit OR number-word) / `expect_routed_agent`
(vs the SSE `routed_agent_id`) / `min_citations` / `citation_must_attribute`; plus Info-only
`reference_answer` cosine + optional LLM-judge (never gate). Multi-clip **scenarios** (`Meta.injections[]`)
stage the same subject across days/cameras. Determinism: `RAG_LLM_TEMPERATURE=0`+`RAG_LLM_SEED` (in
eval.env + config-hash). Phone: `physical_loopback.py --scenario <case>` scores the same questions
tolerantly over real capture. Files: `hushai-eval/src/{fixtures,query_rag,score,lib}.rs`,
`hushai-rag/src/chat.rs` (SSE `routed_agent_id`). Fixtures: `repeat_visitor`, `money_talk`,
`clip_speaker_roster`. A question may also carry `playback` (`{device_id, playhead_offset_ns}`,
offset from base like `ChatFilters`) — the simulated viewer playback context for deictic questions.

**Harness-driven RAG wins (the loop working):**
- ✅ **F1 — deterministic presence/count aggregation.** The People/Objects/Plates agents listed a
  top-k-CAPPED sighting set and let the small LLM COUNT it → undercount (live: 60 sightings → "48 times";
  the list caps at 50). Added `hushai-rag/src/presence.rs` (`person_/plate_/object_presence` → uncapped
  `COUNT(DISTINCT segment)` + first/last + hour/day rhythm, mirrors `analytics.rs`) + `is_count_intent`
  routing in `chat.rs`; the model now NARRATES a computed figure. Verified live incl. real phone capture
  ("how many times did I see a tv?" → 3, DB truth 3) and L1 (`tests/presence.rs`: 55 vs capped-50).
- ✅ **Co-occurrence "who was I with" dropped a person.** Retrieval returned Bob+Carol but the LLM
  narrated only Carol. Fixed with a DETERMINISTIC `presence::render_people_list` when
  `routes::is_co_occurrence_query` → "You were with Bob and Carol."
- ✅ **F4 multi-turn condensation.** A follow-up ("count them again" / "what days does it appear")
  re-embedded bare, lost the entity, and mis-routed to `recordings` → "I don't have information". Added
  `RAG_QUERY_CONDENSE` + `llm::condense` (rewrite follow-up → standalone, temp 0). SUB-BUG: after
  condensing, feeding the router the prior context dragged it back to `recordings`; fixed by classifying
  the STANDALONE query with EMPTY context. Both follow-ups now route `objects` + answer.
- ✅ **`physical_loopback.py` 401 on RAG chat** — it authed with `DEVICE_TOKEN`; the RAG needs `RAG_TOKEN`
  (run_stack mints a random one). Fixed to use `RAG_TOKEN` (pinned to `dev-rag-token` in eval.env).
- ◐ **Objects SEMANTIC recall false-negative (OPEN).** Non-count "when did I see a laptop" via CLIP-NN
  returned "did not see" though laptop=3 in the DB (full-sentence CLIP text embeds poorly). The COUNT path
  (exact label) is unaffected. Candidate: use exact-class `list_by_object_class` for "when did I see a
  <COCO class>" too — but that drops modifiers ("red car"), so test carefully; do NOT fix blind.
- ✅ **Named-person "was I with X?" — FIXED.** `resolve_people_sources` now routes a co-occurrence
  query that NAMES someone to `retrieve::list_co_presence_pair` (X's sightings only in segments where the
  owner was ALSO present), not `list_by_person` (solos). Verified: "was I with Bob?"→"You were with Bob";
  "was I with the Stranger?"→"I didn't see you with anyone" (Stranger was alone); no-name "who was I with"
  still enumerates all.
- ✅ **F7 events/alerts agent — BUILT.** New `AgentKind::Events` + `retrieve::list_events` (lane +
  alerts-only severity filters) + router "events" category + persona; `llm::answer_events` narrates the
  timeline, count-intent → deterministic count. Verified live (5 seeded events → "what happened" lists all
  5; "any alerts?" → only the 2 warnings; "how many?" → 5) with router regression clean. Not wired into
  the single-shot `/v1/rag/query` (chat auto-routes it). No eval fixture yet (events come from the worker
  producer; add a scenario that injects event-producing media, or L1-seed the `events` table).
- ✅ **Deictic "who was speaking in this video clip" — FIXED (2026-07-01).** With a clip clearly
  playing and the owner's voice enrolled ("Mendel"), chat answered "I don't have information…": the
  viewer sent only `filters.device_id` (no time anchor) and the Grounded agent ran an UNANCHORED
  semantic NN on "who was speaking" → nothing relevant. Fix: (a) viewer sends a `playback` object
  (`{device_id, playhead_unix_nanos}` — on-screen camera + wall-clock playhead; `store.js`
  pull-provider); (b) `chat.rs` anchors deictic questions to playback (fill unset device, ±2 min
  window via `DEICTIC_CLIP_WINDOW_NANOS`), which also suppresses CAMERA_CLARIFY while playing;
  (c) `is_speaker_roster_query` pre-routes "who was speaking/talking" to `recordings`
  deterministically (the LLM router's "who" drifts to `people`/faces) and, with a bounded window,
  answers from `retrieve::list_speakers_in_window` (deterministic distinct-speaker roster; empty →
  `window_has_footage` distinguishes silence from worker lag). Fixture: `clip_speaker_roster`
  (JFK window enrolled as "Mendel" — the first `enroll:` use in a train fixture; Q1 =
  playback-anchored roster → "mendel" + attributed citation, Q2 = same ask, no playback →
  CAMERA_CLARIFY guard).
- ◐ **Objects SEMANTIC recall false-negative (still OPEN)** — see above; the F1 count path is unaffected.

---

## 7. Troubleshooting (gotchas hit while building this)

- **exit 2 / `inconclusive` / poll timeout** → worker not running, vision lane waited-for but disabled,
  or a model missing. Check `pgrep -f target/debug/hushai-worker` and the worker log. The poller waits
  for the vision lane only if a vision modality is scored.
- **Worker exits immediately** → missing `DYLD_FALLBACK_LIBRARY_PATH` (sherpa) or `ORT_DYLIB_PATH` (vision).
- **`feed_segments.py: video not found`** → pass an ABSOLUTE media path; the script's CWD is `local_dev`.
  (The harness canonicalizes paths in `inject.rs`; the gotcha bites manual invocations.)
- **`speaker_id` decode error (UUID vs TEXT)** → `transcript_sentences.speaker_id` is **TEXT** (stringified
  UUID), not a `uuid` column; read as String and parse (see `query.rs`).
- **Phone captures audio only** → the app's persisted mode is voice-assistant/audio-only. Force video with
  `am start … --ez audio_only false`. Verify mode in logcat: `capture started … audioOnly=…`.
- **Phone gotchas** (memory): Galaxy is Android 9 (no `adb pair`); the camera SurfaceView renders BLACK
  under `adb screencap` (overlay) — verify liveness via `dumpsys media.camera`, not screenshots.
- **`refusing to run against … not a *_test database`** → set `DATABASE_URL` to `hushai_test` (the guard
  is intentional; the harness TRUNCATEs).
- **All metrics show `new`** → the config-hash changed (model/knob/provisioning); `--update-baseline`.

---

## 8. Extending the harness

- **New scorer / modality:** add the ground-truth struct in `src/fixtures.rs`, the scorer in
  `src/score.rs` (return `Metric`s with a `Direction` + `floor_ok`), gate it on the modality in
  `score_all`, and query the data in `src/query.rs`. Keep metrics assignment-invariant + tolerance-banded.
- **New physical expectation:** extend the `--expect-*` flags in `local_dev/physical_loopback.py`
  (tolerant presence/recall only — physical capture is non-deterministic).
- **New tolerance band:** `src/baseline.rs::unchanged_band` (counts strict; sentiment widest; ASR tight).

---

## File map

| Path | Role |
|------|------|
| `hushai-eval/src/` | the deterministic harness (ctx, manifest, reset, inject, poll, enroll, query, score, baseline, report, probe) |
| `hushai-eval/fixtures/{train,holdout}/` | fixtures (ground truth committed; media gitignored) |
| `hushai-eval/baselines/<config-hash>/` | committed reference metric vectors |
| `local_dev/feed_segments.py` | injector (extended: `--capture-start-ns`, `--segment-id-seed`, `--emit-ids`) |
| `local_dev/physical_loopback.py` | Tier-2 scored physical camera-at-screen test |
| `local_dev/build_fixtures.sh` / `fetch_eval_clips.sh` | regenerate synthetic / real fixture media |
| `local_dev/provision_vision.sh` | export RF-DETR + CLIP object-lane weights |
| `local_dev/eval.env` | determinism profile sourced by `run_stack.sh --test-db` |
| `hushai-worker/src/{speaker,vad,process}.rs` | speaker lane (the VAD fix + the quality instrumentation live here) |
