# Perception Hardening — Session Summary (2026‑07)

A consolidated record of the work done to bring HushAI's four perception lanes — **object detection,
face detection, audio/ASR, and license‑plate recognition** — plus the Android capture path, to a
working + regression‑guarded state. Everything below was verified against the **live pipeline**
(deterministic injection via `hushai-eval`, physical phone‑at‑screen capture via
`local_dev/physical_loopback.py`, or both) — not just unit tests.

**Status of the code:** the perception changes are **committed** (present in `HEAD`; they rode along in
the vision/RAG commits). The one exception is the **backend `db.rs` fix** (below), which is an
uncommitted working‑tree change at the time of writing.

---

## TL;DR — what changed, by lane

| Lane | Headline change | Verified | Regression guard |
|------|-----------------|----------|------------------|
| **Objects** | Fixed the RF‑DETR class decode (was mislabeling *everything*) + added class‑aware NMS + more frames/segment | injection + live phone | `car_object` Tier‑1 fixture + `coco91_column_map_is_correct` unit test |
| **Faces** | Provisioned **SCRFD** (fixed live small/through‑screen misses) | injection + live phone | `face_id` Tier‑1 fixture |
| **Audio** | Tunable whisper anti‑hallucination knobs (defaults unchanged, WER‑validated) | full eval (WER unchanged) | 6 audio fixtures |
| **Plates** | **New end‑to‑end ALPR** — chose+provisioned a free MIT model, fixed 4 decode issues | injection + live phone (`EMD774`) | `plate_ocr` Tier‑1 fixture |
| **Android capture** | Upright rotation at any phone orientation | on‑device frame extraction | — |
| **Backend** | Fixed a pool‑connect regression that crashed the backend on startup | `/readyz` = 200 | — |

---

## 1. Object detection — `hushai-worker/src/vision/objects.rs`

- **Headline bug (fixed): the class map was wrong.** RF‑DETR‑Nano's ONNX output is `labels[1,300,91]`
  (C = **91**): the class‑logit **column index is the COCO category id** (col 1 = person, 2 = bicycle,
  37 = sports ball, 82 = refrigerator; col 0 + the historical gaps are background). The old decoder mapped
  that column through a **dense COCO‑80** table, so *every* detection was mislabeled — a person came back as
  `bicycle`, confirmed both by direct injection and live capture. Fixed to the canonical **COCO‑91** layout
  (`coco91_class_names`), which skips background/gap columns (never emits `class_<i>`) and can be overridden
  by the authoritative `models/rf-detr-classes.json` (`OBJECT_CLASSES_PATH`).
- **Class‑aware NMS.** RF‑DETR's 300‑query head emits several near‑duplicate boxes per object and the lane
  had *no* dedup (the only detector lane missing it). Added `geom::nms_by` per label inside `detect()`,
  tunable via `OBJECT_NMS_IOU` (default 0.5) — suppresses same‑label overlap so an overlapping
  person+bicycle both survive.
- **Recall.** `FRAMES_PER_SEGMENT` default raised **2 → 3** (more chances to catch a transient object; the
  motion‑gate still skips static segments in production).
- **Guards:** unit test `coco91_column_map_is_correct` + the `car_object` Tier‑1 fixture (below).
- **Verified live:** a dense traffic scene → `car, truck`; a living room → `chair, couch, potted plant`.

## 2. Face detection — SCRFD + ArcFace

- **Provisioned SCRFD** (`models/scrfd_10g_bnkps.onnx`, via `local_dev/fetch_scrfd.sh`), the worker's
  *intended* default detector (`detect_scrfd.rs`, `FACE_DETECTOR_KIND=scrfd`, YuNet as fallback). It had
  been silently falling back to the weaker YuNet because the file wasn't present.
- **This fixed the live small/through‑screen face miss:** a screen‑displayed face that YuNet captured as
  **0** faces through the phone, SCRFD reads as **3** (live loopback PASS), while still detecting a large
  face on direct injection.
- **Characterized (not a bug):** a group of 3 near‑identical faces (Apollo 11 crew) mints only 2 distinct
  persons — the identity matcher false‑merges lookalikes. But 2 **clearly‑distinct** people (verified with a
  woman + a man) are correctly kept separate → the threshold is **well‑calibrated for the common case**.
  Tightening it to catch lookalikes would cause the *worse* failure (duplicate identities for the same
  person across captures), so this is left as a documented hard‑case, **not** tuned.
- **Guard:** the `face_id` Tier‑1 fixture (below).

## 3. Audio / ASR — `hushai-worker/src/asr.rs`

- **Tunable anti‑hallucination knobs.** `transcribe_blocking` previously set no whisper decode‑quality
  params (C defaults applied). Added a `DecodeQuality` struct + `WHISPER_NO_SPEECH_THOLD`,
  `WHISPER_LOGPROB_THOLD`, `WHISPER_ENTROPY_THOLD`, `WHISPER_SUPPRESS_NST` env knobs, applied via the
  whisper‑rs setters.
- **Defaults deliberately unchanged.** The eval harness caught that flipping `suppress_nst=true` regressed
  `fdr_infamy` WER 0.562 → 0.625 (it shifts the greedy path on noisy audio), so the shipped defaults equal
  whisper's — **zero behavior change**. The knobs are a *calibration surface*: tune per‑deployment to fight
  hallucinated tails on noisy/looped audio, then re‑run the eval to confirm WER doesn't regress.

## 4. License plates (ALPR) — now working end‑to‑end

Previously fully disabled (no detector). Now provisioned + working, verified by both direct injection and a
live phone capture reading a real plate (`EMD774`, exact) into the catalog.

- **Model choice (free, agent‑selected):** the detector is **`open-image-models` YOLOv9‑t‑640**, which is
  **MIT‑licensed** and pairs natively with the existing **`fast-plate-ocr` CCT** recognizer (same author).
  It's a plain pre‑exported **ONNX** — no pickle, so no arbitrary‑code‑execution risk from loading it.
  Provisioned reproducibly via `local_dev/fetch_plate_detector.sh` (default URL baked in;
  byte‑identical on re‑fetch) + `local_dev/export_plate_ocr.py`.
- **Four decode fixes that got it working** (`vision/plates/detect.rs`, `ocr.rs`, `write.rs`):
  1. **End2end output layout.** The detector emits `[N,7] = [batch, x1,y1,x2,y2, class, score]` (NMS baked
     in), not raw `cxcywh`. Added an end2end decode branch (`PLATE_DETECT_END2END`, default true) + the
     matching **centered 114‑gray letterbox** (Ultralytics/YOLOv9 preprocessing).
  2. **OCR input dtype.** The CCT model wants **UINT8** (raw 0‑255; it normalizes internally). Feeding f32
     made ORT reject every run. `ocr.rs` now auto‑detects the ONNX input element type and feeds u8 vs f32.
  3. **Confidence.** The CCT head outputs *softmax probabilities*; the decoder was re‑softmaxing them
     (confidence collapsed to ~0.07 and would trip the mint gate). Fixed to use the max probability
     directly (`PLATE_OCR_CTC` selects fixed‑length vs CTC decode).
  4. **Coverage.** `PLATE_DETECT_WHOLE_FRAME` now scans the whole frame *in addition to* vehicle ROIs, so a
     plate outside an imperfect RF‑DETR car box isn't missed.
- **Cross‑checked** the Rust decode against the reference (`open-image-models`) — byte‑identical box.
- **Guards:** `inspect_plate_model_io_shapes` + `detect_plates_on_test_image` tests + the `plate_ocr`
  Tier‑1 fixture (below).

## 5. Android capture — upright rotation (`hushai-android/.../capture/`)

- The phone captured **sideways** (no rotation compensation), which was silently defeating the vision lanes
  on complex scenes. Fixed so capture is **upright at any phone orientation**:
  - `OrientationTracker.kt` (new) reads the physical orientation from the **accelerometer**
    (`OrientationEventListener` — works screen‑off, unlike display rotation) and computes the upright
    rotation from it + the camera sensor orientation.
  - `VideoEncoder` samples it **per ~2s segment** (mid‑capture rotation self‑corrects) →
    `SegmentMuxer.setOrientationHint()` stamps the MP4 rotation matrix.
  - The worker's ffmpeg **autorotates** on that metadata (no `-noautorotate`), and browsers honor it, so
    both detection frames and NVR playback come out upright.
- **Verified on‑device:** an extracted captured frame reads upright (rotation matrix `-90`, 1280×720 →
  720×1280), and the previously‑failing live scenes then passed.

## 6. Backend regression fix — `hushai-backend/src/db.rs` (uncommitted)

- **Symptom:** the backend exited on startup with `pool timed out`; this was the root cause of the
  intermittent "stack keeps dying" behavior during the session.
- **Cause:** the pool's `after_connect` hook ran two `;`‑separated `SET` commands
  (`statement_timeout` + `idle_in_transaction_session_timeout`) through `sqlx::query().execute()`, which
  uses the prepared/extended protocol — Postgres rejects a multi‑command prepared statement
  (`cannot insert multiple commands into a prepared statement`), so every pooled connection failed.
- **Fix:** `sqlx::raw_sql(AssertSqlSafe(init))` (the **simple** query protocol allows multiple commands),
  preserving both timeout SETs. **Verified:** backend now logs `listening` and `/readyz` → 200 (which runs
  `SELECT 1` through the pool, proving `after_connect` succeeds).
- This was uncommitted DB‑hardening WIP; the fix keeps its intent and just makes the connection actually
  open. It unblocks the whole test stack (perception + RAG‑chat).

## 7. Test coverage & determinism

- **Three new deterministic Tier‑1 vision fixtures** (`hushai-eval/fixtures/train/`), all with committable
  ground truth (`meta.json` + `expected.json`); media is regenerated from **public‑domain Wikimedia**
  sources by `local_dev/fetch_eval_clips.sh`:
  - `car_object` — PD Peugeot iOn photo → asserts `objects.label_f1` on `car` (locks the COCO‑91 decode; a
    regression to COCO‑80 would label it `motorcycle` → F1 = 0).
  - `plate_ocr` — PD Auckland street photo cropped to a full car + plate → asserts the plate reads `EMD774`
    (locks the whole ALPR chain).
  - `face_id` — PD NASA official portrait (Judith Resnik) → asserts `persons.distinct_count = 1` (locks
    SCRFD detect + ArcFace identity).
  - All three vision lanes now have a Tier‑1 guard, alongside the 6 existing audio fixtures.
- **Determinism fix:** added `VISION_MOTION_SKIP_ENABLED=false` to the determinism profile
  (`local_dev/eval.env`). The skip‑static motion gate is **stateful per camera** — a re‑injected *static*
  clip is skipped on the 2nd+ run (its frames match the last analyzed frame), which made vision
  non‑deterministic across reruns (`plate_ocr` flaked 1.0/0.0/0.0 until this was set). Ken‑burns fixtures
  dodged it by having motion.

---

## How to reproduce / verify

```bash
# One-time provisioning (models are gitignored; scripts fetch/export them)
local_dev/fetch_scrfd.sh                                  # SCRFD face detector
local_dev/fetch_plate_detector.sh                         # plate detector (MIT YOLOv9-t, default URL)
SSL_CERT_FILE=$(python -c 'import certifi;print(certifi.where())') \
  python local_dev/export_plate_ocr.py                    # plate OCR (fast-plate-ocr CCT)
local_dev/fetch_eval_clips.sh                             # regenerate all fixture media (incl. the 3 vision ones)

# Deterministic Tier-1 gate (worker must run with the eval.env determinism profile)
cargo run -p hushai-eval -- run --tier full --fixtures all          # exit 0 = all lanes pass
cargo run -p hushai-eval -- run --tier full --case plate_ocr        # ALPR only
cargo test -p hushai-worker --lib                                   # unit tests (COCO-91 map, OCR decode, …)
cargo test -p hushai-worker --test vision_pipeline inspect_plate_model_io_shapes -- --nocapture

# Live (phone-at-screen) realism gate
python3 local_dev/physical_loopback.py --media <img|clip> --case X --expect-objects car --expect-face
```

Model licenses (weights gitignored; code paths MIT): RF‑DETR Apache‑2.0 · OpenCLIP · YuNet Apache‑2.0 ·
SCRFD/ArcFace InsightFace **non‑commercial research** · fast‑plate‑ocr **MIT** · open‑image‑models plate
detector **MIT**.

---

## Open items (honest)

- **Rebaseline the vision fixtures** — deferred while a concurrent workstream is still changing the eval
  config‑hash (RAG‑chat determinism knobs). A single `cargo run -p hushai-eval -- run --tier full
  --fixtures all --update-baseline` anchors `car_object`/`plate_ocr`/`face_id` once that settles. Until
  then they pass on their floors.
- **Face lookalike merge** — 3 near‑identical faces → 2 identities. Characterized as an acceptable
  hard‑case; a real fix needs a labeled multi‑person set to find a separating threshold (do **not**
  blind‑tune).
- **ASR hallucinated tail on looped/low‑SNR audio** — the whisper knobs are the lever; needs a stable repro
  fixture + per‑deployment WER validation before changing defaults.
- **Plate rectification** — the current detector is bbox‑only (axis‑aligned crop); a 4‑corner *pose* plate
  model would enable true deskew before OCR (the CCT OCR handles mild skew fine today).
- **Commit `db.rs`** — the backend pool fix is uncommitted working‑tree at time of writing.

---

## Where else this is documented

- `AGENTS.md` — the "Vision pipeline" + "ALPR" sections carry the decode contracts + provisioning steps.
- `hushai-eval/RECURSIVE_TESTING.md` — the testing playbook: fixtures, the determinism profile, and the
  per‑finding history (§6).
- `.env.example` (worker) — every knob above with its default + one‑line rationale.
