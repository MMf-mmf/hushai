# Osprey — custom detection, trained here, proven in the field, run on the edge

**Our own object detector · our own labeled footage · a bbox-level eval gate · quantized deployment to a phone and a dedicated edge box.**

This spec is written BEFORE implementation. Part 1 (data flywheel), Part 2 (training package), Part 3 (worker custom lane) and Part 5 (eval `detections` modality) are build contracts for Waves 1–3; Part 4 (edge) is a build contract for Waves 4–5. The phases in Part 6 are executed after implementation and become the acceptance gate. Every `file:line` reference in this document was validated against the working tree on **2026-09-16** (branch `feat/hushai-voice-assistant`, migration head `0031_gotham.sql`, HEAD `85c70da`). This file's pillar checkboxes and result matrix are updated as work lands (the `docs/feature-parity-roadmap.md` convention that `gotham.md` also follows).

Osprey is the complement of [`gotham.md`](gotham.md), which explicitly disclaims this territory (`gotham.md:18`, non-goal 6 at `gotham.md:756`: "**New perception lanes or models** — the graph derives exclusively from existing catalogs + events"). Gotham adds reasoning on top of perception. Osprey changes perception itself.

---

# The game plan in plain language

*This section is the whole story with no code. Read it alone and you know what is being built, in what order, and why.*

## What we have today

Cameras (an Android phone, and the browser) record two-second video clips and upload them. A Rust worker pulls three still frames out of every clip and runs them through pre-trained models somebody else made: a general object detector that knows the 80 COCO categories (person, car, dog, chair…), a face detector plus a face-recognition model, and a licence-plate reader. Whatever those models find is written to the database, drawn as boxes in the browser timeline, and made answerable in chat ("when did I last see Bob?").

Every one of those models is **downloaded, not trained**. Nothing in this repository has ever trained a model, and nothing has ever measured *how good* a detector's boxes are — the current test only checks that the word "car" appears somewhere in a six-second clip, not that the box sits on the car.

## What Osprey adds, in one sentence each

1. **A way to say "wrong".** A Label mode in the browser where you step through the exact frames the AI looked at and grade its boxes: accept, reject, fix the class, redraw, or add one it missed. Your corrections become a private training set.
2. **A training workshop.** A new Python package where those corrections plus public datasets turn into trained detectors — first by fine-tuning an existing architecture, later by building one from parts, and finally by comparing whole model families.
3. **A ruler that can't be fooled.** A new test type that scores boxes properly (the industry-standard mAP metric, written from its definition), including the failure modes mAP hides: false alarms on classes nobody asked about, and recall at the confidence threshold the product actually uses.
4. **A second detector slot in the worker.** The trained model runs alongside the COCO one, off by default, so the existing system is byte-for-byte unchanged until you switch it on.
5. **Three new things the cameras can see.** Backyard wildlife (raccoon, deer, fox, coyote…), small and far-away objects that the current detector misses, and smoke/fire — each wired into the existing alerts and chat.
6. **The model, squeezed onto small hardware.** First the old Galaxy S8 that already runs as a camera; then a dedicated edge computer (an NVIDIA Jetson) with a camera, running the detector in real time on battery-class power, measured properly: latency, memory, watts, heat.
7. **A rig that fakes bad conditions.** Harsh light, water spray on the lens, a shaking mount, darkness. Whatever the detector misses there gets harvested back into step 1, and the loop closes.

## Why each piece exists (the job it maps to)

The target role wants someone who has trained detectors on messy real data, can modify a YOLO or DETR architecture rather than configure one, builds pipelines from open-source parts, has opinions about training infrastructure, and has deployed vision models to edge devices with real profiling and quantization. Osprey is that list, turned into working software you own:

| What the job asks for | Where you earn it | What proves it |
|---|---|---|
| Trained detection models on custom datasets in PyTorch | Block 3 (fine-tune), Block 4 (own detector) | A model in `models/custom/` that passes a gate on your own footage |
| Real data that fought you — bad annotations | Block 2 (label schema records *what the model got wrong*) + Block 7 (noisy-label audit) | A documented relabel that moved field accuracy |
| Small objects | Block 4 (stride-4 head, tiling, a dedicated small-object fixture) | AP on small objects, before and after, at a stated latency cost |
| Domain shift · "passes the test set, fails in the field" | Blocks 2, 6, 7 (sealed field split, the field-gap log, the condition rig) | `field_gap.jsonl`: clip-set recall next to own-footage recall |
| YOLO and DETR families, deep enough to modify | Blocks 3, 4, 7 (three model tracks + ablations) | An ablation table and a written "which family, when" recommendation |
| Build a pipeline from open-source components, not an API call | The whole of Part 2 | Backbone → neck → head → assigner → loss, all yours |
| Opinions about training infrastructure, said out loud | Blocks 0, 1, 8 (dataset hashing, group-aware splits, run manifests, gate discipline) | Every experiment reproducible from a hash |
| Edge deployment: profiling, quantization, accuracy/latency/memory/power | Blocks 5, 6 | Two Pareto tables (phone, Jetson) and a chosen operating point with the reasoning |
| Wildlife / environmental detection | Blocks 3, 7 | Wildlife and smoke lanes live in the app, firing real alerts |
| Aerial / remote sensing (preferred) | Block 4 | A public drone-imagery benchmark run with the same tiling code |

## The order of work

Nine blocks. Week numbers assume about ten hours a week and are guidance, not a contract — the horizon is open-ended, and blocks overlap where they do not depend on each other.

### Block 0 — Foundations (weeks 0–2)
**Build.** Fix a suspected half-frame timing bug in how the worker records *when* each analysed frame happened (harmless today, fatal the moment you draw boxes on frames). Stand up the training package skeleton with a `doctor` command that checks the machine. Stand up the edge-client skeleton that can pretend a video file is a camera. Sort out disk space. Order the Jetson, because it has a lead time.
**Learn.** How data actually flows in this system; what a dataset audit turns up before you train anything.
**Done when.** The audit report for a public wildlife dataset exists, a dataset fingerprint reproduces twice in a row, and the fake camera posts ten out of ten clips into the test stack.
**Interview story.** "I built the data layer first, and found N broken labels before training anything."

### Block 1 — Measure before you train (weeks 2–5)
**Build.** The bbox-level eval: a new `detections` test type with mAP, AP on small objects, recall at the product's threshold, and false alarms per frame. Ground truth is generated *analytically* — the test clips are slow pans over a still photo, so where the box lands on each sampled frame is arithmetic, not opinion. Then measure the detector already shipping.
**Learn.** mAP from its definition, and the three ways it lies.
**Done when.** The full gate passes twice with the new tests, and a hand-computed AP case (a worked example you can do on paper) is a green unit test.
**Interview story.** "I wrote the mAP scorer from the COCO definition and pinned its tolerance to 0.02, because our pipeline is deterministic and the default band would hide a nine-point regression."

### Block 2 — The flywheel (weeks 4–8)
**Build.** The labels table, the API, and the browser Label mode. A ranked queue that puts the *useful* frames in front of you first: low-confidence detections, frames where the phone and the server disagreed, unusual-looking things. Export to the standard COCO format.
**Learn.** What makes an annotation good; why random sampling wastes your time; the difference between the test set and the field.
**Done when.** Two hundred labeled frames of your own footage exist, and the first field-gap number is recorded.
**Interview story.** "Random sampling gave me 95% trivial accepts; the ranked queue tripled my error yield per minute."

### Block 3 — The first custom model (weeks 6–10)
**Build.** Fine-tune RF-DETR (the transformer detector the app already runs) on backyard wildlife: a public camera-trap dataset plus your own captures. Export it through a strict contract so the existing Rust decoder reads it unchanged. Turn on the second detector slot. Raccoon boxes appear in the browser; "what visited the yard last night?" answers in chat.
**Learn.** How DETR fine-tuning works, what the bipartite matcher does, how to operate on a model's classification head.
**Done when.** The model clears its accuracy floor on held-out cameras *and* on your own footage, and a test proves that with the lane off, every existing baseline is unchanged.
**Interview story.** "First custom model shipped end to end, through a contract test that pins its input and output shapes."

### Block 4 — Build the detector yourself (weeks 10–16)
**Build.** `hushdet`: an anchor-free detector assembled from parts — a pretrained backbone, a feature pyramid neck, decoupled heads, the task-aligned assigner from the TOOD paper, and the CIoU + distribution-focal loss pair. Then modify it: add a stride-4 head for small objects, swap backbones four ways, add attention, add re-parameterizable convolutions, distill from a bigger teacher. Small/far mode lands here too: tiling at inference, plus a drone-imagery benchmark run off-app.
**Learn.** Every loss term and every assignment rule, because you wrote them.
**Done when.** Your detector is within 10% of the fine-tuned RF-DETR's accuracy at half the CPU latency, with at least a four-row ablation table.
**Interview story.** "I can modify a YOLO because I wrote one."

### Block 5 — The edge, part one: the phone (weeks 14–20)
**Build.** Golden test vectors so the image pre-processing and box decoding are provably identical in Kotlin, Rust and Python. Then on-device detection on the Galaxy S8, feeding its findings to the server as *hints* the server grades against its own results — a drift monitor. Then the sweep: execution providers × precision × thread counts, with latency, memory, battery and temperature.
**Learn.** What quantization buys on a 2017 chip that lacks the dot-product instructions (spoiler: much less than the brochure says), and how to prove it.
**Done when.** A Pareto table exists, the video recording is provably untouched, and the server-side agreement score is populated.
**Interview story.** "Why int8 barely helped on this SoC, with the per-op profile to show it."

### Block 6 — The edge, part two: the dedicated device (weeks 16–24)
**Build.** A Jetson Orin Nano with a camera, running the same client code, uploading to the same backend, detecting with TensorRT. FP16 first, then INT8 two different ways, then the power modes. A thirty-minute soak to catch thermal throttling. Then the harsh-conditions rig: bright light, a sprayed shield in front of the lens, a shaking mount, infrared at night. What it misses gets harvested back into Block 2.
**Learn.** TensorRT, calibration-set design (this is where "it worked until night-time" gets diagnosed), power/thermal trade-offs.
**Done when.** A table of three precisions × three power modes × two models, and a one-page memo choosing the operating point.
**Interview story.** "Here is my Pareto front and why I picked 15 W INT8."

### Block 7 — Model-family depth and the environmental lane (weeks 22–28)
**Build.** Fine-tune the RT-DETR family for a three-way comparison. Run the robustness suite (synthetic harsh conditions) across every model. Audit a public smoke/fire dataset for bad labels, fix them, train the smoke lane, and wire it to an alert at a precision-leaning threshold. Write the post-mortem on one case where a model passed the test set and failed in the field.
**Learn.** YOLO versus DETR trade-offs with your own numbers; label noise as a measurable ceiling.
**Done when.** A written recommendation backed by data, and at least one relabel that moved field accuracy.
**Interview story.** "Test-set pass, field fail — here's the diagnosis."

### Block 8 — Ongoing
Distillation, class-incremental training with replay (teaching new classes without forgetting old ones), quantization-aware training if post-training quantization costs too much accuracy, the Mac-side capacity study (how many cameras can one machine feed?), optionally a second edge device, and the write-ups that turn all of this into a portfolio.

## What this deliberately does not do

No cloud anything. No new service or port. No general-purpose annotation tool (no polygons, no object tracks, no multiple users). No AGPL-licensed code or weights shipped, ever — this repo has a public MIT mirror. No re-distribution of any dataset. No language model anywhere in a test gate. No rewrite of the Rust worker into a training framework; training is Python, inference is Rust.

---

# The build contract

*Everything below is precise. `file:line` references were validated on 2026-09-16 against the tree state named in the preamble.*

## Scope

What "own the detection system" means **here** — a single-owner, fully local, zero-egress home system:

1. **Train detectors on our own data.** Today the repo has zero training code: the only Python is export tooling (`local_dev/export_rf_detr.py`, `export_clip.py`, `export_gfpgan.py`, `export_plate_ocr.py`), there is no `pyproject.toml`/`requirements.txt` anywhere, and runtime inference is pure ONNX through `ort = "=2.0.0-rc.9"` (`hushai-worker/Cargo.toml:31`). Osprey adds `hushai-train/` and keeps Rust inference-only.
2. **Grade the model's own output as the labeling primitive.** Today there is no annotation or correction surface for boxes anywhere — a repo-wide grep for `annotat|correction|ground_truth|relabel` in the backend, viewer and worker returns one unrelated hit. The closest precedents are catalog-level (person merge/rename `hushai-backend/src/persons.rs`, the Gotham binding review queue `hushai-backend/src/graph_api.rs:355-396`).
3. **Score boxes, not label sets.** Today `score_objects` (`hushai-eval/src/score.rs:554-578`) computes one metric, `objects.label_f1`, from the set of labels seen in a time window; the observed struct `ObjectDet` (`hushai-eval/src/query.rs:21-27`) carries only `{label, start_ns, end_ns}` — **no bbox and no score ever reach the scorer**, even though the worker's `DetectedObject` has them (`hushai-worker/src/vision/objects.rs:33-40`). Osprey adds a `detections` modality with mAP, AP-small, recall at the operating point, and false-positives per frame.
4. **Run a second, custom detector without disturbing the first.** The object lane today is a single RF-DETR-Nano session built in `hushai-worker/src/lib.rs:417-470`, configured by `OBJECT_*` knobs at `hushai-worker/src/config.rs:747-771`. Osprey adds a parallel `CUSTOM_DET_*` session, off by default.
5. **Put the model on edge hardware and measure it honestly.** Today there is zero on-device inference on Android (`com.alphacephei:vosk-android:0.3.47` is the only ML dependency, `hushai-android/app/build.gradle.kts:78`) and zero quantization anywhere in the tree (a grep for `quantiz|int8|fp16` returns only the plate-OCR input dtype). Osprey adds ONNX Runtime on the phone, a dedicated `hushai-edge/` client for a Jetson-class board, and a shared profiling protocol.
6. **Turn all of it into new capability lanes** — wildlife, small/far objects, smoke/fire — wired into the existing event/alert stack (`hushai-worker/src/events_producer.rs`, migration `0014_events_and_alerts.sql`) and the RAG object agent (`hushai-rag/src/routes.rs:352-434`).

What it is **not**: no cloud, no new service or port, no general annotation tool, no LLM in any gate, no shipped AGPL, no dataset redistribution, no Rust training. See [Non-goals](#non-goals--referenced-not-duplicated).

## Locked decisions

- **Naming.** "Osprey" is the spec codename. Env prefixes are functional, per house convention: **`CUSTOM_DET_*`** (worker second detector), **`LABELS_*`** (backend label surface), **`EDGE_*`** (worker edge-grading), **`INGEST_HINT_DET_*`** (backend hint-gate policy), **`HUSHAI_TRAIN_*`** (Python side only, never in the Rust config).
- **Rust stays inference-only.** Training and dataset tooling is Python + PyTorch in a new top-level `hushai-train/` (uv-managed, **not** a Cargo workspace member, own isolated venv). `hushai-edge/` (the device client) is likewise Python, for the same reason: every vendor runtime (TensorRT, HailoRT, picamera2, Sony MCT) is Python-first.
- **One artifact format: ONNX, plus a model card that drives preprocessing.** The card (`model_card.json`) states input dtype/layout, letterbox position and pad value, whether normalization is folded into the graph, and the output contract. Runtimes read the card; they never hardcode. Today `objects.rs:109-124` hardcodes f32/NCHW/ImageNet-norm/pad-bottom-right-with-0 while `plates/detect.rs:56-60` uses centered 114-gray padding — that divergence is exactly what a card prevents repeating.
- **The custom detector is additive and off by default.** With `CUSTOM_DET_ENABLED=false` (the default), the object lane produces byte-identical rows and all 28 baselines under config_hash `d4acc862fd0ba577` are unchanged. A fixture proves it (`det_custom_off_identical`).
- **Device sends measurements, server owns thresholds.** New `hint.det.*` attrs ride inside the existing `hint.v="1"` family (`hushai-backend/src/hints.rs:98-130` ignores unknown keys and drops *all* hints on an unknown version — so **never bump `hint.v`**). The phone and the Jetson emit identical keys. The only gate use is a *veto on static skips*: a confident device detection forces processing. There is no rule that lets a device's silence skip work.
- **Labels are graded model output, and they are durable.** Frame identity is `(segment_id, frame_offset_nanos)`. Label rows carry a soft pointer to the model row plus a denormalized copy of what was graded, and **no foreign key** to `segments` or `scene_objects` — those are deleted by retention (`hushai-backend/src/devices.rs`), by partition drops (`drop_scene_object_partitions_before`, `0009_person_vision.sql`) and by the eval's `TRUNCATE … CASCADE` (`hushai-eval/src/reset.rs:10-19`). Labels outlive all three.
- **Labels never leave the machine.** Frames and exports live under `BLOB_DIR/label_frames/` and `BLOB_DIR/label_exports/`; the export endpoint returns a filesystem path, not bytes; own-footage fixtures live in a gitignored `fixtures/local/` split that is never in `--fixtures all`.
- **Licensing is a hard gate.** Only Apache-2.0 / MIT / BSD for anything committed or shipped — this repo has a public MIT mirror (`MMf-mmf/hushai`). Ultralytics (AGPL-3.0, already installed globally on this machine and importable from `local_dev/.venv-vision` because it was created with `--system-site-packages`) is a **local study reference only**; `hushai-train/` gets an isolated venv plus a test that asserts `ultralytics`, `yolov5`, `mmyolo` and `cleanlab` are not importable. Datasets are recipes, never redistributed; research-only licences are flagged in the dataset manifest and require an explicit `--personal-learning` flag to enter a train split.
- **Eval discipline is inherited, not re-litigated.** Detection metrics are Tier-1 deterministic; latency is `Info` only; tolerance bands are tight (0.02 for AP/recall, versus the 0.10 default at `hushai-eval/src/baseline.rs:95`); calibration freezes observed values and widens, never narrows; **never loosen a gate to go green** (`hushai-eval/RECURSIVE_TESTING.md:203-207`).
- **Migration numbers.** `0032_detection_labels.sql` (labels + `scene_objects.detector/frame_w/frame_h`) and `0033_edge_detection_grading.sql` (edge-vs-server grade columns). They are separate files because one creates tables and one alters a table. Whichever merges first keeps its number; the other renumbers **before** merge. A shipped migration file is never renumbered.
- **Edge hardware.** Primary: **NVIDIA Jetson Orin Nano Super dev kit** (or a used original Orin Nano dev kit, which the JetPack 6.2 firmware turns into a Super — same silicon), with **JetPack 6.2.x pinned for the whole curriculum**. Optional dashcam-sized second device: **Pi Zero 2 W + Raspberry Pi AI Camera (IMX500)**. Rationale and the full comparison table are in [Part 4b](#4b-dedicated-edge-device-o5).
- **PR0 is a precondition, not a feature.** The suspected frame-offset bias (below) is verified and fixed before any labeling code is written, because every box-level label depends on the frame index being right.

## §1 Pillars

Dependency order: **O6-COCO → O1 → O2 → O3 → O6-custom → O7**; **O4** and **O5** run in parallel from Wave 4. Waves: Wave 1 = PR0 + O6-COCO + O1. Wave 2 = O2 + O3 + O6-custom (staging) + the wildlife lane. Wave 3 = O2's own-detector track + small/far mode + Mac quantization. Wave 4 = O4 (phone). Wave 5 = O5 (device) + O7's environmental lane + the depth work.

### O1 — Data flywheel (label · queue · export)
- [ ] PR0: frame-offset verification + fix (`hushai-worker/src/vision/frames.rs:140`)
- [ ] Migration `0032_detection_labels.sql` + `scene_objects.detector/frame_w/frame_h` writes
- [ ] Backend `/v1/labels*` (frames, upsert, done, queue, stats, export, purge) + audit arm + proxy allowlist
- [ ] Viewer Label mode (`Shift+D`) + 5 e2e checks
- [ ] Hard-example queue (confidence band · hint-audit disagreement · edge disagreement · CLIP novelty · class rarity)

### O2 — `hushai-train/` (the training package + the curriculum)
- [ ] Package skeleton, `doctor`, dataset schema/manifest/splits/audit, converters
- [ ] Track 1: RF-DETR fine-tune (+ class-incremental with COCO replay)
- [ ] Track 2: `hushdet`, an anchor-free detector built from components, with the ablation series
- [ ] Track 3: RT-DETRv2 / D-FINE for the family comparison
- [ ] Export contract + contract test + model card; quantization (QDQ INT8, FP16); offline eval + robustness suite

### O3 — Custom lane in the worker
- [ ] Card-driven preprocessing for both detector sessions (COCO card = byte-identical behavior)
- [ ] Second ONNX session, `CUSTOM_DET_*`, `detector` tag on rows, `object_detect_custom` stage metric
- [ ] Events + RAG label-set awareness for custom classes

### O4 — Edge: the phone
- [ ] Shared golden vectors (Kotlin ↔ Rust ↔ Python) — closes the gap recorded at `AGENTS.md:655`
- [ ] `FrameAnalyzer` fan-out + `EdgeDetector` + ONNX Runtime for Android + settings/intents
- [ ] `hint.det.*` emission + backend parse + veto + `0033` grading + metrics + dashboard warning
- [ ] Profiling protocol + `local_dev/edge_profile.sh` Pareto table + power/thermal soak

### O5 — Edge: the dedicated device (`hushai-edge/`)
- [ ] Client: GStreamer capture → 2 s segments → durable buffer → uploader (contract-conforming camera)
- [ ] TensorRT FP16 + INT8 (calibration cache **and** explicit Q/DQ), engine cache, model-card compat check
- [ ] Profiling: power modes, `tegrastats` rails vs wall meter, per-layer profile, 30-minute soak
- [ ] Field-condition rig + hard-example harvest back into O1

### O6 — Eval: the `detections` modality
- [ ] `DetectionsGt` + `score_detections` (greedy IoU matching, 101-point AP) + bands + unit tests
- [ ] `local_dev/gen_det_gt.py` analytic ground-truth generator with a self-verification mode
- [ ] Fixture bank D1–D9 incl. a sealed negatives counter-fixture and the `local` field fixture
- [ ] Tier-2 `--expect-detections` + the field-gap log

### O7 — New capability lanes
- [ ] Wildlife (backyard animals) — label set, model, events, alert rule, RAG phrasing
- [ ] Small/far objects — P2 head variant, tiling at inference, the small-object fixture
- [ ] Environmental smoke/fire — noisy-label audit first, precision-leaning operating point, alert rule

---

# Part 1 — Data-flywheel build contract (O1)

## 1.0 PR0 — frame identity (a precondition for everything else)

`hushai-worker/src/vision/frames.rs:140` records each sampled frame's position as:

```rust
let offset_nanos = ((i as f64 + 0.5) / fps * 1e9) as i64;
```

while ffmpeg's `fps=` filter (`frames.rs:86`) emits output frame *i* on the grid `i/fps`, starting from the first input frame. If that reading is right, every `frame_offset_nanos` in `scene_objects`, `person_segments` and `plate_detections` is **half a sample period late** — 333 ms with the default `FRAMES_PER_SEGMENT=3` over a 2 s segment (`hushai-worker/src/config.rs:751`, `frames.rs:76-77`).

Today the consequence is cosmetic: the viewer snaps detections to the nearest group within 400 ms (`hushai-viewer/ui/js/detections.js:16`), and `GET /v1/persons/{id}/sample-face` seeks with `-ss offset` (`hushai-backend/src/persons.rs:507`) so it returns a frame about eight source frames after the one the face was found in. For **box-level labeling it is fatal**: prefill boxes would be drawn on the wrong picture and every label inherits the error.

**PR0 procedure (verify first, then fix).**

1. Reproduce: take a 2 s clip, run the worker's exact filter chain, and separately extract at `-ss 0.0/0.667/1.333` and `-ss 0.333/1.0/1.667`; compare PNG bytes to the filter output. A fiducial clip (a moving white rectangle on black, generated by `local_dev/gen_det_gt.py --verify`) makes the answer visual and numeric.
2. If confirmed, change the line to `let offset_nanos = (i as f64 / fps * 1e9) as i64;` and add a unit test `sampled_offsets_sit_on_the_fps_grid`.
3. If **not** confirmed (ffmpeg's `fps` filter selects mid-interval on this build), keep the line, record the finding here, and derive the label-time seek from the same expression instead.

Gate safety either way: eval windows are scored from `start_unix_nanos`/`end_unix_nanos` (`hushai-eval/src/query.rs:217-232`), not from `frame_offset_nanos`, so **no metric, no config hash and no baseline changes**. Historical rows keep whatever bias they had; the 400 ms snap tolerance hides it; do not backfill.

## 1.1 Design stance

1. **A label is a grade on model output, not free-form drawing.** Every row either grades one model box (`accept` | `reject` | `relabel` | `redraw`) or adds one the model missed (`add`). The training signal and the error record are the same artifact.
2. **Frame identity is `(segment_id, frame_offset_nanos)`** — deterministic after PR0, shared by the worker, the viewer's detection grouping (`hushai-viewer/src/detections.rs:9-10`) and the eval's sampled grid.
3. **Pixels are snapshotted lazily**, the first time a human touches a frame. Persisting every sampled frame is a non-starter; see 1.3.
4. **Explicit over implicit.** Marking a frame `done` requires a verdict on every model box at or above the viewer's display threshold (0.3, `detections.js:17`). Silence is not consent; implicit accepts are label noise.

## 1.2 Migration `0032_detection_labels.sql`

Header comment states the inverse of the derived-data headers used by 0024/0028: **DURABLE HUMAN DATA — never truncated by retention, never rebuilt, deleted only by a deliberate privacy action.**

```sql
-- (a) scene_objects: detector tag + frame dimensions.
--     Additive, defaulted/nullable → partition-safe, no backfill (the 0012 precedent).
ALTER TABLE scene_objects ADD COLUMN IF NOT EXISTS detector text NOT NULL DEFAULT 'rfdetr-coco';
ALTER TABLE scene_objects ADD COLUMN IF NOT EXISTS frame_w  integer;
ALTER TABLE scene_objects ADD COLUMN IF NOT EXISTS frame_h  integer;
CREATE INDEX IF NOT EXISTS scene_objects_custom_det_idx
    ON scene_objects (device_id, start_unix_nanos) WHERE detector <> 'rfdetr-coco';

-- (b) label sets: append-only ordered class lists. COCO category_id = array index + 1.
CREATE TABLE label_sets (
    label_set   text PRIMARY KEY,                 -- 'home-v1'
    classes     jsonb NOT NULL,                   -- ["person","car","cat","dog","raccoon", ...]
    version     integer NOT NULL DEFAULT 1,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

-- (c) one row per human-touched sampled frame. NO FK to segments — labels outlive footage.
CREATE TABLE label_frames (
    frame_id            uuid PRIMARY KEY,
    device_id           text   NOT NULL,
    segment_id          uuid   NOT NULL,
    frame_offset_nanos  bigint NOT NULL,
    t_unix_nanos        bigint NOT NULL,          -- capture_start + offset (the viewer group key)
    frame_w             integer NOT NULL,
    frame_h             integer NOT NULL,
    frame_uri           text,                     -- <BLOB_DIR>/label_frames/<segment_id>/<offset_ns>.png
    pixel_sha256        bytea,                    -- sha of the decoded rgb24 buffer (provenance only)
    status              text NOT NULL DEFAULT 'open'
                        CHECK (status IN ('open','done','skipped')),
    dataset_tag         text,                     -- 'train-2026-10' | 'eval-night' | NULL
    queue_reasons       jsonb,                    -- {"low_conf":2,"hint_disagree":true,"novelty":0.41}
    labeler             text NOT NULL DEFAULT 'admin',
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    UNIQUE (segment_id, frame_offset_nanos)       -- idempotency key
);
CREATE INDEX label_frames_device_time_idx ON label_frames (device_id, t_unix_nanos);
CREATE INDEX label_frames_status_idx      ON label_frames (status, dataset_tag);

-- (d) one row per graded or added box.
CREATE TABLE detection_labels (
    label_id              uuid PRIMARY KEY,
    frame_id              uuid NOT NULL REFERENCES label_frames (frame_id) ON DELETE CASCADE,
    label_set             text NOT NULL REFERENCES label_sets (label_set),
    class_name            text NOT NULL,          -- the HUMAN's final class
    bbox                  jsonb NOT NULL,         -- [x,y,w,h] original-frame px, clamped
    verdict               text NOT NULL CHECK (verdict IN ('accept','reject','relabel','redraw','add')),
    source                text NOT NULL CHECK (source IN ('human','model_prefill','imported')),
    -- Soft pointer: scene_objects' PK is (id, created_at) and partitions get DROPPED, so no FK.
    -- The denormalized copy IS the record of what was graded (the entity_edges evidence posture).
    model_row_id          bigint,
    model_row_created_at  timestamptz,
    model_detector        text,
    model_class           text,
    model_bbox            jsonb,
    model_score           real,
    labeler               text NOT NULL DEFAULT 'admin',
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    CHECK ((verdict = 'add') = (model_row_id IS NULL))
);
CREATE UNIQUE INDEX detection_labels_one_verdict_per_model_box
    ON detection_labels (frame_id, model_row_id) WHERE model_row_id IS NOT NULL;
CREATE INDEX detection_labels_frame_idx ON detection_labels (frame_id);
CREATE INDEX detection_labels_class_idx ON detection_labels (label_set, class_name, verdict);

INSERT INTO label_sets (label_set, classes) VALUES
 ('home-v1', '["person","bicycle","car","motorcycle","bus","truck","cat","dog","bird","package"]');
```

**Why a new `detector` column and not `embedding_model`.** `embedding_model` describes the CLIP tower that embeds *every* region row regardless of which detector proposed it (`hushai-worker/src/vision/write.rs:82`, column written at `write.rs:990-1015`); custom-lane rows still carry CLIP embeddings so semantic search keeps working on them. Overloading it would break the one column RAG filters on and leave no way to express "custom box, CLIP embedded". The `__frame__` whole-frame row keeps the default tag.

**Frame dimensions.** Nothing in the system stores them today — the viewer scales boxes against `video.videoWidth/videoHeight` at render time (`hushai-viewer/src/detections.rs:4-10`). Going forward the worker has `img.width()/height()` in hand inside the per-frame loop (`hushai-worker/src/vision/write.rs:302-431`) and writes them on every row (`ObjectWrite` at `write.rs:133-141` gains two fields, bound in `insert_scene_objects` at `write.rs:978-1015`). For history and for the snapshot, the backend reads dimensions from the PNG IHDR header (bytes 16–23) of the frame it just extracted — no image crate needed.

## 1.3 Frame persistence: re-extract, do not store

| Option | Cost at 3 frames / 2 s / camera | Verdict |
|---|---|---|
| Worker writes every sampled frame | 129,600 frames/day → ≈19 GB/day/camera as JPEG q85 at 1280×720, ≈155 GB/day as PNG; needs its own retention sweep | **Rejected** — dwarfs the footage it came from |
| Worker writes only "hard" frames (a 0.3–0.5 box, or a hint-audit disagreement) | 1–3% of frames → 0.2–0.6 GB/day/camera; needs a sweep hook in the retention path | **Deferred** (Wave 5 escape hatch, PR24) — only if the queue proves starved |
| **Re-extract from the stored segment at label time** | one ffmpeg call per *touched* frame (~60 ms); 2,000 labeled frames ≈ 2.4 GB total | **Chosen** |

Re-extraction is deterministic enough because the backend already does exactly this for `sample-face` and `sample-crop` (`hushai-backend/src/persons.rs:507` `extract_jpeg`, reused by `plates.rs`): H.264 decoding is normative, offsets are deterministic after PR0, and `-ss <offset>` before `-i` on an MP4 is an accurate (decode-and-discard) seek. The one version-sensitive step is the YUV→RGB conversion, pinned with `-sws_flags accurate_rnd+full_chroma_int` (`LABELS_SWS_FLAGS`). PNG bytes are *not* stable across libpng/zlib versions, which is why provenance is a sha over **pixels**, and why a sha mismatch is a warning, never a gate.

Storage: `BLOB_DIR/label_frames/<segment_id>/<offset_ns>.png` — outside `blobs/`, so the content-addressed reclaim path (`hushai-backend/src/storage.rs`) can never touch it, and device deletion leaves it. Purging is explicit (`DELETE /v1/labels/frames?device_id=`). Retention race: if the segment was already reclaimed, the snapshot returns `410 footage_gone`; the queue only offers frames whose segments are still present.

## 1.4 Backend API (`hushai-backend/src/labels.rs`, mounted behind `auth::require_bearer`)

| Method + path | Purpose | Audit action |
|---|---|---|
| `GET /v1/labels/sets` · `PATCH /v1/labels/sets/{name}` `{append:[…]}` | class lists, append-only (never renumber) | `label_set.append` |
| `GET /v1/labels/frames?device_id&from&to&include=prefill` | enumerate sampled frames in a window, each with `model_boxes[]`, existing `labels[]`, `status` | — |
| `GET /v1/labels/frames/{id}` · `…/image.png` | one frame + its snapshot (path-traversal guard as `persons.rs:459-465`) | — |
| `POST /v1/labels/frames` `{device_id, segment_id, frame_offset_nanos}` | ensure row + snapshot; idempotent on the unique key; `410` if footage gone | `label_frame.create` |
| `POST /v1/labels/frames/{id}/done` `{status, force?}` | frame done marker; `409` while unreviewed boxes ≥ `LABELS_MIN_REVIEW_SCORE` remain | `label_frame.done` |
| `DELETE /v1/labels/frames?device_id=` | privacy purge: rows + PNGs | `label_frame.purge` |
| `POST /v1/labels` `{frame_id, label_set, verdict, class_name, bbox, model_row_id?, model_row_created_at?, label_id?}` | upsert a grade or an add; implicitly creates the frame row if absent | `label.upsert` |
| `PATCH /v1/labels/{id}` · `DELETE /v1/labels/{id}` | edit / retract | `label.update` / `label.delete` |
| `GET /v1/labels/queue?device_id&from&to&limit=50` | ranked hard-example frames (1.5) | — |
| `GET /v1/labels/stats?dataset_tag` | frames open/done, labels per class × verdict, per-detector agreement rate | — |
| `POST /v1/labels/export` `{format:'coco', dataset_tag?, label_set, include_negatives}` | writes `BLOB_DIR/label_exports/<tag>/<export_id>/`, returns `{export_id, dir, n_images, n_annotations}` | `label.export` |

**Prefill is virtual.** `model_boxes` are read live from `scene_objects` (both detectors); no rows are written until a human acts. `source='model_prefill'` is reserved for a future bulk pseudo-label materialization; `imported` for external COCO imports.

**Wiring, three one-line additions.** (a) `hushai-viewer/src/proxy.rs:142-162` `is_backend_path` gains `/v1/labels` — without it the viewer routes to hushai-rag and every call 404s. (b) `hushai-backend/src/audit.rs:87-127` `classify` gains a `"labels"` arm (its `match collection` currently returns the raw `"POST /v1/labels"` with no target for unknown collections); the literals `sets|frames|queue|export|stats` register as collection-level route names so they are not mistaken for ids. (c) `routes.rs` mounts the router beside the graph surface (`routes.rs:200-226` is the shape to copy).

**Auth.** `require_bearer` (`hushai-backend/src/auth.rs:98-110`) accepts any token in `DEVICE_TOKENS` — the known gap that a camera token is an admin token (`AGENTS.md:647-648`). Labels are mutating admin calls through the viewer proxy, so they are audited at the gateway (`proxy.rs:33-70`) *and* in-handler with the verdict detail (the `graph_api.rs:373-381` precedent). Optional hardening in PR23: a `require_bearer_label("admin")` variant for labels and export.

## 1.5 The hard-example queue

Deterministic ranking, ties broken by `(t_unix_nanos, segment_id)`, `status <> 'open'` excluded, candidates capped at `LABELS_QUEUE_MAX_CANDIDATES` before the novelty pass:

```sql
WITH cand AS (
  SELECT s.segment_id, s.frame_offset_nanos, s.device_id,
         s.start_unix_nanos + s.frame_offset_nanos AS t_ns,
         count(*) FILTER (WHERE s.det_score >= $lo AND s.det_score < $hi) AS low_conf,
         count(*) FILTER (WHERE s.detector <> 'rfdetr-coco')              AS custom_n,
         bool_or(v.hint_audit AND v.audit_verdict = 'disagree')           AS hint_disagree,
         bool_or(COALESCE(v.edge_agreement, 1.0) < 0.3)                   AS edge_disagree
  FROM scene_objects s
  JOIN segment_vision_status v ON v.segment_id = s.segment_id
  LEFT JOIN label_frames lf
         ON lf.segment_id = s.segment_id AND lf.frame_offset_nanos = s.frame_offset_nanos
  WHERE s.device_id = $1 AND s.start_unix_nanos >= $2 AND s.start_unix_nanos < $3
    AND s.object_label <> '__frame__' AND (lf.status IS NULL OR lf.status = 'open')
  GROUP BY 1,2,3,4)
SELECT *, low_conf*1.0 + (hint_disagree::int)*0.5 + (edge_disagree::int)*1.0 + custom_n*0.25
         AS base_score
FROM cand ORDER BY base_score DESC, t_ns, segment_id LIMIT $cap;
```

`edge_agreement` comes from `0033` (Part 4); guard the reference with `to_regclass`/a column check so the query works before that migration lands. Then, in Rust, per candidate's low-confidence regions: a CLIP k-NN distance over `scene_objects` (HNSW index `scene_objects_embedding_hnsw`, `0009_person_vision.sql:159-163`) gives `novelty`; `score = base_score + 2·novelty + class_rarity_bonus` where the bonus is `1/(1 + accepted_labels_of_that_class)`. The reasons are written to `label_frames.queue_reasons` the first time the frame is touched, so the sampler is auditable after the fact.

**Known blind spot.** Frames the motion gate skipped write no rows at all (`VISION_MOTION_SKIP_ENABLED`, `hushai-worker/src/vision/write.rs:199-261`), so a missed *static* object never reaches the queue. Accepted; run a deliberate `VISION_MOTION_SKIP_ENABLED=false` session for a static-object labeling pass.

## 1.6 COCO export

Deterministic ordering (images by `t_unix_nanos`, annotations by `label_id`). `accept|relabel|redraw|add` become annotations; `reject` is omitted from `annotations.json` and listed in `negatives.json` (hard negatives, with the model box that was wrong — the false-positive mining signal); a frame marked `done` with no positives becomes an image with zero annotations (a true negative, which is training signal too); `open` frames are never exported.

```json
{"info":{"dataset_tag":"train-2026-10","label_set":"home-v1","label_set_version":3,"exported_at":"…","git_sha":"…"},
 "categories":[{"id":1,"name":"person"}],
 "images":[{"id":1,"file_name":"images/<segment_id>_<offset_ns>.png","width":1280,"height":720,
            "hushai":{"device_id":"cam-front","segment_id":"…","frame_offset_nanos":666666666,
                      "t_unix_nanos":1781784000666666666,"pixel_sha256":"…"}}],
 "annotations":[{"id":1,"image_id":1,"category_id":3,"bbox":[412.0,180.5,300.0,140.0],
                 "area":42000.0,"iscrowd":0,
                 "hushai":{"verdict":"redraw","source":"human","labeler":"admin",
                           "model_detector":"rfdetr-coco","model_class":"car","model_score":0.41}}]}
```

No zip (the backend has no zip dependency); `hushai-train/` consumes the directory.

## 1.7 Viewer Label mode

**Files.** New `hushai-viewer/ui/js/labeling.js` (class `Labeling`, which *wraps* the existing `Detections` canvas rather than forking it, reusing `_videoRect()` at `detections.js:123-129` and the `groupTimes` index); `ui/js/api.js` gains the label calls; `ui/index.html:66-69` gains `<button id="modeLabel">` plus an `<aside id="labelPanel">` (queue list, class picker, counters, Done); `ui/styles.css:282-293` gains `.det-overlay.label { pointer-events: auto; cursor: crosshair; }` so **hit-testing exists only in Label mode** and the 23 existing e2e checks are untouched; `ui/js/app.js` refactors `setMode(on)` into `setViewMode("video"|"det"|"label")` keeping `setMode` as a boolean shim (the e2e harness calls `viewerDebug.setMode(true)`).

**Keys.** The keydown switch at `app.js:967-1062` already owns `d` (detections), `a` (AI ribbons), `l`/`L` (seek), `[`/`]` (prev/next recording), `i`/`o`/`e` (export), `,`/`.`/`<`/`>` (frame step and speed) and the digits. Label mode therefore enters on **`Shift+D`** (the free key in the Detections family) and, once active, consumes its keys before the player sees them (`labeling.handleKey(e)` first; return if consumed — the modal/export-mode precedence pattern at `app.js:956-964`):

| Key | Action |
|---|---|
| `Shift+D` or `#modeLabel` | enter/exit Label mode (pauses playback, snaps to the nearest sampled frame) |
| `[` / `]` | previous / next sampled frame (`groupTimes` neighbours → `seekTo(t, {play:false})`) |
| click | select the smallest-area box containing the point (inner box wins) |
| `a` / `x` / `r` | accept / reject / relabel the selection (relabel opens the picker; digits 1–9 pick) |
| drag on empty area (≥ 4 px) | draw a box → `add`; drag a selected box's edge → `redraw` |
| `Backspace` | retract my label on the selection |
| `Enter` | mark the frame `done` (toast + refusal while unreviewed boxes remain) |
| `Esc` | deselect; second `Esc` exits Label mode |

**Hit-testing on letterboxed video** is the inverse of `_videoRect()`: `fx = (clientX − rect.left − offX) / scale`, same for y, clamped to the intrinsic size; device-pixel-ratio is already absorbed by the canvas transform (`detections.js:183-192`). Boxes are stored in original-frame pixels. When `frame_w` is known the viewer asserts `video.videoWidth === frame.frame_w` and toasts a warning on mismatch — the remux is `-c copy`, so a mismatch means the invariant broke.

**Debug hooks** (`app.js:148-165`, localhost-gated): add `setViewMode` and a `labeling` getter exposing `{frames(), index(), current(), select(i), verdict(v), draft(), step(±1), queue(), counts()}`.

**Five e2e checks**, appended before the native-dialog guard that must stay last (`hushai-viewer/e2e/run.mjs:463`), each SKIPping with a reason when the stack has no vision rows:
1. `Shift+D` enables canvas hit-testing and pauses — computed `pointer-events` is `auto`, tab is selected, `video.paused`.
2. `]` steps to the next sampled frame — `currentMs()` within 1 ms of the next group time.
3. A mouse drag draws a box in original-frame pixels — `labeling.draft().bbox` equals the in-page inverse-mapped expectation.
4. Accept persists through the proxy and is audited — poll the frame endpoint, then `GET /v1/audit?action=label.upsert`; clean up with `DELETE`.
5. Label keys never leak to the player — `a` leaves the AI-ribbon state unchanged; `]` leaves the selected device unchanged.

## 1.8 Part-1 touch points

| # | File | Change |
|---|---|---|
| 1 | `hushai-worker/src/vision/frames.rs:140` | PR0: verify, then `i/fps` + unit test |
| 2 | `hushai-backend/migrations/0032_detection_labels.sql` (+ `migrations/README.md` row) | §1.2 |
| 3 | `hushai-backend/src/labels.rs` (new) + `routes.rs` | API, queue, snapshot (`extract_png`, generalizing `persons.rs:507`), export |
| 4 | `hushai-backend/src/audit.rs:87-127` | `"labels"` collection arm + collection-literal route names |
| 5 | `hushai-backend/src/config.rs` | `LABELS_*` knobs (§5) |
| 6 | `hushai-viewer/src/proxy.rs:142-162` | `/v1/labels` in `is_backend_path` |
| 7 | `hushai-viewer/ui/js/labeling.js` (new), `api.js`, `app.js`, `index.html`, `styles.css` | §1.7 |
| 8 | `hushai-viewer/e2e/run.mjs` | 5 checks, before `:463` |
| 9 | `hushai-worker/src/vision/write.rs:133-141, 978-1015` | `ObjectWrite.{detector, frame_w, frame_h}` + INSERT binds |

---

# Part 2 — Training-package build contract (O2)

## 2.0 Environment facts this design is built on (verified 2026-09-16, this machine)

| # | Fact | Consequence |
|---|---|---|
| 1 | torch 2.9.1 on MPS: `aten::grid_sampler_2d_backward` and `torchvision::_deform_conv2d_backward` are **not implemented**; forward works. `nms`, `roi_align`, `generalized_box_iou`, bf16 autocast, SDPA, `topk`, `scatter_add` all work | Deformable attention needs a gather-based sampler or `PYTORCH_ENABLE_MPS_FALLBACK=1` |
| 2 | `rfdetr 1.8.3` (Apache-2.0, in `local_dev/.venv-vision`) ships `rfdetr/utilities/tensors.py::_bilinear_grid_sample`, a gather sampler used on MPS; its trainer imports `pytorch_lightning`, which is **not installed** there. Custom datasets use contiguous remapped labels; logits width is `num_classes + 1` | RF-DETR fine-tuning is native on MPS; `hushai-train` must pin `pytorch-lightning`; the canonical dataset keeps a category-0 placeholder so column 0 stays background, matching `objects.rs:322-346` |
| 3 | `transformers 5.12.1` has RT-DETR, RT-DETRv2, D-FINE and Deformable-DETR; each calls `F.grid_sample` once, no custom kernel | Track 3 trains on MPS with the fallback env var or a patched sampler (itself an exercise) |
| 4 | `torch.onnx.export` in 2.9 defaults to `dynamo=True`, `external_data=True` | A default export writes weights to a sidecar `.onnx.data` that `fingerprint_models` (`hushai-eval/src/manifest.rs:243-273`) never sees → swapped weights, unchanged config hash, silently stale baselines. The export contract **forces a single-file graph** |
| 5 | The worker dlopens ONNX Runtime **1.20.0** (`hushai-worker/Cargo.toml:31`, default `ORT_DYLIB_PATH` in `hushai-worker/src/config.rs:737-740`); `.venv-vision` has onnxruntime **1.27** | The Python contract test pins `onnxruntime==1.20.1`; quantizer output stays ≤ opset 17 / IR ≤ 9 (no opset-21 Q/DQ) |
| 6 | `/System/Volumes/Data` is ~97% full, ≈17 GiB free | Dataset budget is explicit; `HUSHAI_TRAIN_DATA` must support an external volume; `doctor` fails below 10 GiB |
| 7 | `local_dev/.venv-vision` was created `--system-site-packages`, so the global `ultralytics 8.3.240` (AGPL-3.0) is importable from it | `hushai-train/.venv` is isolated, plus a license-hygiene test |
| 8 | `cleanlab` is AGPL-3.0; MMYOLO is GPL-3.0; running MegaDetector v5 pulls AGPL `yolov5` | The confident-learning audit is written in-house; TAL is read from the TOOD paper with mmdetection/PP-YOLOE (both Apache-2.0) as references; MegaDetector is a local labeling tool only |

## 2.1 Package layout and tooling

```
hushai-train/
  pyproject.toml  uv.lock  .python-version(3.12)  README.md
  hushai_train/
    cli.py            # typer: doctor | data | train | eval | export | quant | bench | runs
    paths.py          # HUSHAI_TRAIN_DATA (default hushai-train/data), runs/, ../models/custom
    data/
      schema.py       # pydantic COCO models + strict validation
      manifest.py     # content-based dataset_hash, sources + licences, split policy, stats
      splits.py       # group-aware splits (location/camera/session), the sealed `field` split
      pool.py         # content-addressed image pool, symlinked per split
      quality.py      # audit: degenerate/dup boxes, class histogram, size stats, leakage, empties
      noisy_labels.py # in-house confident-learning box audit + review gallery
      tiling.py       # SAHI-style slicing for training tiles + slice-merge NMS for eval
      augment.py      # albumentations + own mosaic / copy-paste / small-object oversampling
      dataset.py      # torch Dataset over COCO JSON; letterbox identical to objects.rs:109-124
      convert/{lila,visdrone,yolo,hushai,coco_subset}.py
    models/
      hushdet/{backbone,neck,head,blocks,assigner,loss,decode,ema,distill,export_head}.py
      rfdetr_track.py   # fine-tune, head surgery, COCO replay, query/decoder modifications
      rtdetr_track.py   # HF RT-DETRv2 / D-FINE + grid_sample patch
    train/{loop,schedules,replay}.py
    export/{onnx_export,contract_test,model_card,quantize,bench}.py
    eval/{coco_eval,slices,robustness,threshold,report}.py
    track/mlflow_local.py
  configs/{datasets,models,experiments}/*.yaml
  tests/            # pytest: converters, assigner, loss, decode, contract, golden, license hygiene
  runs/ mlruns/ data/ .venv/   # gitignored
```

Dependency pins, all Apache-2.0 / MIT / BSD: `torch==2.9.1`, `torchvision==0.24.1` (match the host), `timm`, `rfdetr==1.8.3` + `pytorch-lightning`, `transformers>=5.12`, `pycocotools` (reference mAP numbers), `albumentations`, `onnx<1.18`, **`onnxruntime==1.20.1`** (match the worker dylib), `onnxslim` (onnxsim wheels lag on 3.12+), `onnxconverter-common` (fp16), `mlflow` (local file store only), `pg8000` (pure-Python Postgres for `pull-captures`), plus opencv-headless, pillow, numpy, scipy, pandas, matplotlib, pydantic, typer, rich, pyyaml, psutil, imagehash. Extras: `cuda` (cloud burst, torch from a cu12x index), `dev`.

Banned, enforced by `tests/test_license_hygiene.py` (`importlib.util.find_spec(...) is None` plus a source grep): ultralytics, yolov5, mmyolo, cleanlab. `sahi` is MIT but we write our own tiling for the learning value. `backbone.py` asserts the chosen timm weights carry a permissive `pretrained_cfg["license"]` (some in22k weights do not).

Coexistence: `local_dev/.venv-vision` and `provision_vision.sh` stay byte-identical — they provision the shipped COCO/CLIP/OCR exports and must keep working. `hushai-train` gets `local_dev/provision_train.sh` (`uv sync --frozen && uv run hushai-train doctor`). `.gitignore` gains `hushai-train/{.venv,runs,mlruns,data}/`. `doctor` reports: torch + MPS, ORT version versus the `models/onnxruntime/…1.20.0` dylib, ffmpeg, `DATABASE_URL` reachability, free disk, `PYTORCH_ENABLE_MPS_FALLBACK`, and that `ultralytics` is not importable. Python 3.12, not 3.13, for wheel coverage.

## 2.2 Canonical dataset, converters, versioning, splits, label quality

**Format.** COCO JSON in the layout `rfdetr` expects, so all three model tracks read one thing:
`$HUSHAI_TRAIN_DATA/datasets/<lane>/<version>/{train,valid,test,field}/_annotations.coco.json`, images symlinked from a content-addressed pool. Conventions: `categories[0] = {"id":0,"name":"__background__"}` (keeps "column 0 = background", the convention `objects.rs:322-346` already decodes); lane class ids fixed in `configs/datasets/<lane>_classes.yaml` and never renumbered across versions; `bbox` `[x,y,w,h]` in pixels (same as `scene_objects.bbox`); every image carries `extra{camera_id, location_id, captured_at, daynight, source, license, orig_dataset, orig_id, annot_source}`.

**Converters** (`hushai-train data convert <kind> --src … --classes … --out …`):
- `lila` — LILA camera-trap COCO → lane classes via a species map; `location` → `location_id`; images without boxes route to a `needs_boxes` list for MegaDetector pseudo-labeling.
- `visdrone` — per-image txt → COCO; drop the ignored-region and "others" categories; keep occlusion in `extra`; `--tile 640 --overlap 0.2` writes a tiled variant.
- `yolo` — `data.yaml` + normalized txt ↔ COCO, both directions (D-Fire and FASDD ship YOLO format).
- `hushai` — `data pull-captures --device-id cam0 --since … --min-score 0.3`: SQL over `segments ⋈ scene_objects`, reconstructing the blob exactly as `hushai-worker/src/vision/frames.rs:38-64` does (prepend `codec_init_data` when `container='fmp4'`), ffmpeg-extracting at `frame_offset_nanos`, writing COCO with `annot_source: "pipeline:<detector>"`. Once Part 1 lands, `/v1/labels/export` output supersedes this for anything human-graded.
- `coco_subset` — COCO 2017 val + a stratified train subset with the 91-id layout intact (replay, and the "existing classes" baseline).
- Human labels from Part 1 are read by the canonical loader directly (the export already *is* COCO).

**Manifest and versioning.** `manifest.json = {dataset_hash, lane, version, sources[{name,url,license,license_flag,fetched_sha}], classes, split_policy, stats}`; `dataset_hash = sha256(sorted (file_name, image_sha256, canonical annotation JSON))[:16]` — content-based, not `len:mtime`, because it must reproduce on a cloud box. Every run and every `model_card.json` records it.

**Splits.** Group-aware only, seeded by `dataset_hash`: by `location_id` for camera traps, by clip for VisDrone, by `device_id+session_id` for app footage. Report an in-distribution val (random split) *next to* an out-of-distribution val (held-out locations) — the gap between them is the domain-shift number. `test` = held-out locations **and** a held-out time window. The **`field`** split holds only own captures and Tier-2 loopback frames, hand-labeled, never trained on, and is itself split into `field-dev` (threshold selection) and `field-sealed` (scored only in the full gate — the `fixtures/holdout/` posture).

**Label quality.** `data audit` writes `audit.md`/`audit.json`: degenerate and out-of-bounds boxes, same-class duplicates at IoU > 0.9, near-duplicate images by perceptual hash, class histogram and imbalance ratio, per-class size distribution (COCO small < 32², plus a tiny < 16² bucket for the aerial benchmark), aspect outliers, per-camera counts (split leakage), empty-image ratio. `noisy_labels.py` implements confident learning (Northcutt et al. 2021) in-house: K-fold cross-validated predictions, IoU-matched to ground truth, a per-box issue score, plus "missing box" (a confident unmatched prediction) and "spurious box" (ground truth unmatched at any threshold) candidates, ranked into an HTML gallery, applied with `data fix --apply review.json`.

## 2.3 Datasets (recipes only; nothing is redistributed)

| Lane | Dataset | Size | Licence | Use on the M3 Pro |
|---|---|---|---|---|
| Wildlife | **ENA24-detection** (LILA) | ~9k images, 23 eastern-NA species, all boxed | CDLA-Permissive-1.0 | Primary; whole set, location split |
| Wildlife | **Caltech Camera Traps** boxed subset | ~57k boxed (small-res pack ≈6 GB) | CDLA-Permissive-1.0 | ≤15k images; the cis/trans-location split *is* the domain-shift lesson |
| Wildlife | NACTI, Snapshot Serengeti | large; boxes on subsets | CDLA-Permissive-1.0 | Optional 5k for OOD robustness only |
| Wildlife | MegaDetector v5a / PytorchWildlife | weights | MIT (but the yolov5 runtime is AGPL) | Pseudo-label teacher, run locally, never shipped |
| Small/far | **VisDrone2019-DET** | 6,471 / 548 / 1,610, 10 classes | research-only — **flag** | Whole set, tiled to 640; off-app benchmark |
| Small/far | DOTA, xView | 20+ GB each | research / non-commercial — **flag** | Only with an external volume |
| Smoke/fire | **D-Fire** | 21.5k images, YOLO format | verify at fetch | 10k subsample + hard negatives |
| Smoke/fire | **FASDD** | ~100k incl. UAV/RS subsets | CC BY 4.0 (verify) | 10k subsample |
| Low-light | **ExDark** | 7,363 images, 12 classes | verify at fetch | The night slice |
| Replay | COCO 2017 val + train subset | 5k + 10–20k | CC BY 4.0 annotations; images are Flickr-licensed | Replay and the existing-class baseline |

Internal-disk budget ≤ 12 GB (ENA24 + Caltech-small + VisDrone + a D-Fire subset + a COCO subset); anything more needs `HUSHAI_TRAIN_DATA` on external storage. `data fetch` verifies the source hash and writes the licence flag into the manifest; `data audit` refuses to build a **train** split from a research-only or non-commercial source unless `--personal-learning` is passed, so the flag is explicit in the run record.

## 2.4 Three model tracks — modify, do not configure

All three share one dataset, one letterbox, one `coco_eval`, one export contract and one `bench`, producing a single comparison table (mAP50 / mAP / AP-small / CPU ms / CoreML ms / params / MB, on `valid`, `test` and `field`).

**Track 1 — RF-DETR fine-tune (`rfdetr_track.py`).**
`hushai-train train rfdetr --dataset wildlife/v1 --variant nano --resolution 384 --epochs 30 --batch 4 --grad-accum 4 --device mps [--replay coco_replay/v1:0.3] [--freeze-encoder-epochs 10]`. Two variants: (a) a K-class model — the default, and what the additive second lane runs; (b) class-incremental — keep the 91-column COCO head, append K columns by copying the existing rows and initializing the new ones, replay 30% COCO images per batch, and report forgetting as the COCO-val mAP delta on the replay subset.
Modification exercises, each a flag, an MLflow run and a written result: queries 300 → 100 (latency versus small-object recall); projector scale P4 → P3 (finer features, more memory); deformable-attention sampling points 4 → 8; matcher cost weights and their effect on convergence; write your own gather-based `grid_sample` and swap it for the shipped one (understanding the op MPS lacks).
Read: DETR (Carion 2020), Deformable DETR (Zhu 2020), DINO (Zhang 2022), LW-DETR (Chen 2024), the RF-DETR report, and `rfdetr/models/{lwdetr,transformer,matcher,criterion}.py`.

**Track 2 — `hushdet`, an anchor-free detector from components.**
`backbone.py`: `timm.create_model(name, features_only=True, out_indices=(2,3,4))` (verified available locally: `mobilenetv4_conv_small` as the default, `efficientnet_b0`, `convnext_nano`, `fastvit_t8`, `repvgg_a0`, `resnet18`). `neck.py`: FPN top-down + PAN bottom-up with CSP blocks, optional P2 (stride 4), width/depth multipliers. `head.py`: decoupled per-level classification (K logits) and regression (4 × 16 DFL bins). `decode.py`: anchor points, DFL integral, distance-to-box. `assigner.py`: the Task-Aligned Assigner from TOOD (alignment `s^α·u^β`, α=0.5, β=6, top-10 in-box candidates, conflicts resolved by max metric), written clean-room from the paper with Apache-2.0 references. `loss.py`: BCE with TAL-normalized targets, CIoU, DFL. `augment.py`: mosaic-4, copy-paste with small-object oversampling, HSV, random affine, mosaic disabled for the final epochs. `train/loop.py`: bf16 autocast on MPS, gradient accumulation, EMA with ramp, cosine schedule with warmup, AdamW, per-epoch `coco_eval`, MLflow. `export_head.py`: an in-graph sigmoid-max → TopK-300 tail so the exported model emits `[1,300,4]` + `[1,300,K+1]` and the existing Rust decoder is a drop-in.
Architecture exercises: (1) a P2 head versus higher input resolution at equal FLOPs (AP-small against ms and MB); (2) four backbone swaps → an accuracy/latency Pareto plot; (3) SE or CBAM in the neck, or one transformer block on P5 (the bridge to Track 3); (4) RepConv training-time branches plus a `reparameterize()` with an export test asserting max |Δ| < 1e-4; (5) distillation from a bigger teacher (logit KD on matched anchors plus mask-weighted feature imitation); (6) stretch: a YOLOv3-style anchor head with k-means anchors, to feel what TAL replaced; (7) SAHI-style tiling at inference (Python eval; a Rust port is explicitly out of scope).
Read: YOLOv3, YOLOX (Ge 2021), TOOD (Feng 2021), GFL (Li 2020), CIoU (Zheng 2020), RepVGG (Ding 2021), PP-YOLOE (Xu 2022), Kisantal 2019 (small-object augmentation), Ghiasi 2021 (copy-paste), SAHI (Akyon 2022), FGD (Yang 2022).

**Track 3 — RT-DETR family via HF (`rtdetr_track.py`).**
`RTDetrV2ForObjectDetection` and `DFineForObjectDetection` (both Apache-2.0), with `PYTORCH_ENABLE_MPS_FALLBACK=1` or `--patch-grid-sample` (reusing Track 1's gather sampler; benchmark both). Exercises: decoder layers 6 → 3 (the LW-DETR lesson), queries 300 → 100, backbone r18vd ↔ r34vd, and comparing D-FINE's distribution refinement against Track 2's DFL — the same idea in a different family. HF heads have no background column, so the exporter prepends one and the contract stays uniform.
Read: RT-DETR (Zhao 2024), RT-DETRv2 (Lv 2024), D-FINE (Peng 2024), and `modeling_rt_detr_v2.py`.

## 2.5 The export contract

Artifacts live at `models/custom/<model_id>/` (two levels deep, safely inside the eval's depth-3 walk), with `model_id = <arch>-<size>-<dataset_hash8>-<run8>`:
`model.onnx` (fp32, single file), `model.fp16.onnx`, `model.int8.onnx`, `classes.json`, `model_card.json`, `golden_detections.json`, `SHA256SUMS`.

**Contract (drop-in for the decoder at `objects.rs:131-202`).** Input `[1,3,S,S]` f32 (or `[1,S,S,3]` u8 with normalization folded into the graph — the card says which), RGB, letterbox anchored top-left with pad 0 *after* normalization (mean colour), exactly the rule at `objects.rs:109-124`, and `dataset.py` trains with the same rule. Output[0] `[1,N,4]` cxcywh normalized to the input square (so the normalized-versus-pixel heuristic at `objects.rs:168-202` reads "normalized"); output[1] `[1,N,C]` raw logits (the decoder applies the sigmoid); N = 300. **Order is the contract; names are not.** Exporter: `torch.onnx.export(dynamo=False, opset_version=17, do_constant_folding=True)` → `onnxslim` → checker and shape inference → assertions: single file (no external data), IR ≤ 9, opset ≤ 17, one input, two outputs, `C == max(class id) + 1`.

`classes.json` is the `{"id":"name"}` shape `load_class_map` already parses (`objects.rs:350-368`), so the worker needs no new parser.

`hushai-train export contract-test models/custom/<id>` mirrors the Rust test `inspect_object_model_io_shapes` (`hushai-worker/tests/vision_pipeline.rs:253`) — same `IN/OUT name : dims` print format — then: loads under onnxruntime 1.20.1 CPU; checks PyTorch-versus-ORT parity on eight fixed frames from `field-dev` (boxes < 1e-3, logits < 1e-2); decodes with a Python re-implementation of the Rust decoder (column argmax, threshold, class-aware greedy NMS matching `vision/geom.rs` semantics, letterbox un-mapping) and writes `golden_detections.json` so a Rust test can decode the same frames and compare; smoke-tests the CoreML EP and reports the node-partition count (which reveals fallbacks).

`model_card.json` carries: `{model_id, arch, lane, input{size,layout,dtype,norm,letterbox{position,fill}}, outputs{order,box_format,activation}, N, C, opset, ir_version, sha256, dataset_hash, run_id, git_sha, license, metrics{valid,test,field}, operating_points, latency_ms{…}, quant{method,calib_hash,n}, variants[…]}`. **The card is the single source of truth for preprocessing** across the Rust worker, the Kotlin phone client and the Python edge client.

## 2.6 Quantization

- `quant calib-set --from hushai-captures --n 300 --stratify daynight,device_id --exclude field` → a calibration directory keyed by content hash. Calibration data comes from **our own frames**, which is exactly where the "it worked until night-time" failure gets created or prevented.
- `quant static --method percentile --per-channel --format QDQ`: ORT `quant_pre_process` then `quantize_static` (QDQ, u8 activations, int8 per-channel weights), excluding the first convolution, the head output convolutions, and Sigmoid/TopK; assert no opset-21 operators so ORT 1.20 can load it. The same QDQ artifact also builds under TensorRT on the Jetson — one INT8 file for two targets.
- `quant fp16` via `onnxconverter_common.float16` with `keep_io_types=True` — useful for CoreML and NNAPI; note in the report that ORT CPU fp16 is slow.
- Quantization-aware training only if post-training quantization costs more than 1.0 mAP50; MPS fake-quant support is partial, so QAT runs on CPU or in the cloud.
- Measurement: `eval coco --split test` for fp32/fp16/int8 → a delta table; `bench --ep cpu|coreml --threads 1,4` → p50/p95 ms, RSS, file size — the offline twin of `hushai_worker_stage_seconds{lane="vision",stage="object_detect"}` (`hushai-worker/src/vision/write.rs:42,357-366`). The real check is the loadtest profile in Part 4.
- Variant naming shared with both edge tracks: `model.<precision>[.<target>].onnx`, each listed in `model_card.variants[]` with its sha, size and per-platform latency.

## 2.7 Offline evaluation

`eval/coco_eval.py` (pycocotools, `maxDets=300`): mAP@[.5:.95], mAP50/75, AR, AP small/medium/large (plus a tiny < 16² bucket), per-class AP, per-class PR curves. `threshold.py`: per-class F1-max or precision-at-target (smoke/fire targets precision ≥ 0.9; wildlife leans to recall) → `model_card.operating_points` → the suggested `CUSTOM_DET_MIN_DET_SCORE`. `slices.py`: day/night from `captured_at`, computed proxies (mean luminance; variance-of-Laplacian as a blur proxy — the same idea as `FACE_MIN_SHARPNESS` at `hushai-worker/src/config.rs:766`; grayscale/IR detection), dataset tags → mAP per slice with support counts. `robustness.py`: deterministic corruptions at three severities on `test` — brightness and contrast extremes, sun flare, rain and spatter (the spray proxy), motion blur, downscale, sensor noise, JPEG — reported against clean (ImageNet-C's idea applied to detection). This is the offline stand-in for "salt spray, harsh lighting, constant motion"; the physical rig in Part 4b is the real thing.

Discipline: always report `valid` / `test` (held-out cameras) / `field` together. Promotion of a model to the default in `models/custom/` requires `field-sealed` mAP50 at least matching the incumbent with no per-class collapse. Never lower a gate to go green.

## 2.8 Experiment tracking and reproducibility

MLflow with a local file store (`hushai-train/mlruns/`, gitignored): Apache-2.0, fully offline (this project is zero-egress by design), already the logger `rfdetr`'s trainer imports, has an HF `Trainer` callback, stores artifacts per run, and `mlflow ui` compares runs. Weights & Biases is rejected (account and cloud by default); TensorBoard + JSONL is rejected (no parameter table, no run comparison — we would rebuild MLflow).

Run directory `runs/<exp>/<run_id>/` with `run_id = <YYYYMMDD-HHMM>-<config_hash8>`: `config.resolved.yaml`, `manifest.json` (dataset hash, config hash, repo git sha, torch/ORT versions, device, seed, fallback flag), `checkpoints/`, `export/`, `eval/{valid,test,field}.json`, `logs/`. Seeds are set everywhere and `use_deterministic_algorithms(warn_only=True)`; MPS caveats are documented: index reductions and CPU-fallback ops are non-deterministic and the macOS DataLoader spawns, so expect roughly ±0.3 mAP jitter — any claim under 1 mAP needs at least two seeds. CUDA and MPS numerics differ, so the recorded device makes them separate lineages.

## 2.9 What fits on this machine, and the cloud burst

Calibrate in Block 0 with `hushai-train bench-train`; these are the planning estimates (M3 Pro, 18-core GPU, 36 GB unified):

| Configuration | Per epoch | Practical run |
|---|---|---|
| `hushdet`-mobilenetv4-small @384, batch 32, 10k images | 3–5 min | 50 epochs ≈ 3–4 h |
| Same @640 with a P2 head | ≈4× | Tiles only (≤5k) or cloud |
| RF-DETR-Nano @384, batch 4 × 4 accumulation, 8k images | 15–25 min | 20 epochs ≈ 5–8 h (freeze the encoder for the first half: ~1.5× faster), 8–12 GB unified |
| RT-DETRv2-R18 @512 (HF, CPU-fallback grid_sample backward), 5k images | 30–60 min | 8–12 epochs locally, otherwise cloud |

Keep 5–20k images per lane. Set `PYTORCH_MPS_HIGH_WATERMARK_RATIO` so MPS leaves room for Postgres and ffmpeg. Burst to the cloud when a run exceeds ~8 h or needs CUDA-only operators or QAT: roughly $0.35–0.70/h for an RTX 4090 on RunPod/Vast, $0.75–1.30/h for an A10/A100 on Lambda (list prices move — verify). Reproducibility across the boundary: the same `uv.lock` with `--extra cuda`, `data fetch --manifest` (content-addressed, hashes verified), the same config and seed, and `rsync` of `mlruns/` and `runs/` back (the file store merges trivially). Budget target for the whole curriculum: under $50.

## 2.10 Part-2 touch points

| # | File | Change |
|---|---|---|
| 1 | `hushai-train/` (new tree) | §2.1 |
| 2 | `local_dev/provision_train.sh` (new) | uv sync + doctor |
| 3 | `.gitignore` | `hushai-train/{.venv,runs,mlruns,data}/` |
| 4 | `models/custom/` (gitignored, created on first promotion) | export artifacts |
| 5 | `AGENTS.md` component map | a row for `hushai-train/` |

---

# Part 3 — Worker custom-lane build contract (O3)

## 3.1 Card-driven preprocessing (both sessions)

`ObjectDetector::new` today hardcodes f32 NCHW with ImageNet normalization and top-left zero padding (`hushai-worker/src/vision/objects.rs:77,109-124,253-254`). Add `ObjectDetector::with_card(session, card)` where the card supplies input size, dtype (`f32` | `u8`), layout (`nchw` | `nhwc`), normalization (in-graph or mean/std), letterbox position and fill. Generate a card for today's RF-DETR export that reproduces the current behavior exactly, and route the existing session through it: **the COCO lane's output must be byte-identical**, proven by `car_object` and `det_car_bbox` staying at their frozen values. New knob `OBJECT_DET_CARD_PATH` (optional; absent = today's hardcoded defaults, so an un-provisioned tree is unaffected).

## 3.2 The second session

Built exactly like the existing lane (`hushai-worker/src/lib.rs:417-470` is the template, including the both-models-or-neither rule and the `OBJECT_REQUIRED` bail at `:453-461`):

- Load only when `CUSTOM_DET_ENABLED=true`. Failure → warn and disable, unless `CUSTOM_DET_REQUIRED=true`, which bails loudly (audio keeps running either way).
- Runs inside the same `spawn_blocking` per frame (`hushai-worker/src/vision/write.rs:303-431`), immediately after `object_detector.detect(&img)` at `write.rs:349-369`.
- Its detections are appended to the object list **before** CLIP embedding (`write.rs:371-417`), so custom boxes get a 512-d CLIP vector and open-vocabulary search keeps working on them, and **excluded** from the vehicle ROIs handed to the plate lane (`write.rs:419-430`) — a raccoon is not a car.
- Rows are written with `detector = 'custom:<CUSTOM_DET_NAME>'`; the delete-by-segment-then-insert idempotency (`write.rs:978-1015`) already covers two producers in one transaction.
- New stage histogram label `object_detect_custom` beside the existing `object_detect` (`write.rs:42,357-366`), so the loadtest harness can attribute the cost (`hushai-loadtest/src/controller.rs:18-34` gains the row).
- Per-class score thresholds come from `model_card.operating_points` when present, falling back to the single `CUSTOM_DET_MIN_DET_SCORE`.

## 3.3 Downstream awareness

- **Events.** `events_producer::derive_vision_events` (`write.rs:530-551`) emits `object_seen` events for custom labels under the existing `EVENTS_OBJECT_MIN_SCORE`, so an alert rule can target `raccoon` or `smoke` with no new event machinery (migration `0014_events_and_alerts.sql`).
- **RAG.** The exhaustive-query path resolves a phrase to a COCO label through a hard-coded 80-name list (`hushai-rag/src/routes.rs:436-464`). Make it label-set aware: read `RAG_EXTRA_OBJECT_CLASSES_PATH` (the same `classes.json` the worker loads) or, simpler, fall back to a `SELECT DISTINCT object_label` probe. Open-vocabulary CLIP search already works on custom rows without any change, because they carry embeddings.
- **Viewer.** No change needed — `hushai-viewer/src/detections.rs:90-120` selects all non-`__frame__` rows with a bbox, so custom boxes appear in the overlay the moment they are written.

## 3.4 Config-hash posture (the decision, with the reasoning)

Two failure modes must both be impossible:

1. *Off must be free.* Dropping a trained model into `models/` today re-mints the lineage for **every** fixture, because `fingerprint_models` hashes `len:mtime` of every `*.onnx|*.bin|*.gguf|*.json` under `models/` to depth 3 (`hushai-eval/src/manifest.rs:243-273`) — even with the lane disabled.
2. *Retrained must not be free.* A plain string knob `CUSTOM_DET_MODEL_PATH` would let a **retrained file at the same path** reuse a stale baseline — the exact false-pass class the prefix-fold exists to kill.

**Chosen mechanism.** `fingerprint_models` skips `models/custom/`, **and** `EnvManifest::collect` folds `custom_det::model = sha256(CUSTOM_DET_MODEL_PATH)` plus `custom_det::classes = sha256(CUSTOM_DET_CLASSES_PATH)` into `knobs` **only when `CUSTOM_DET_ENABLED` is truthy**. Also add `"CUSTOM_DET_"` to `KNOB_PREFIXES` (`manifest.rs:129-153`), which folds only knobs actually set — so a plain run keeps `d4acc862fd0ba577`. That makes "off ⇒ identical lineage" and "retrained ⇒ new lineage" properties of the hash rather than of operator discipline about where files were copied. The operating convention still applies on top: training artifacts live in `hushai-train/runs/<run>/`, and promotion copies an immutable `models/custom/<model_id>/model.onnx` and is a deliberate re-baseline.

Related, out of scope, listed under risks: the `len:mtime` fingerprint is why `d4acc862` drifted to `7963897c` on this machine after a re-provision; a cached content sha would fix it for every model, not just ours.

A staging env layer `local_dev/eval.custom.env.example` (the `eval.agent.env` precedent, `run_stack.sh:371-379`) carries the custom pins so the shared `local_dev/eval.env` stays untouched and `d4acc862` survives.

## 3.5 Part-3 touch points

| # | File | Change |
|---|---|---|
| 1 | `hushai-worker/src/vision/objects.rs` | `with_card` preprocessing; `DetectedObject` unchanged |
| 2 | `hushai-worker/src/lib.rs:417-470` | second session build + degradation ladder |
| 3 | `hushai-worker/src/vision/write.rs:349-431, 978-1015` | custom detect call, ordering, `detector` tag, stage metric |
| 4 | `hushai-worker/src/config.rs:747-771` | `CUSTOM_DET_*` + `OBJECT_DET_CARD_PATH` |
| 5 | `hushai-worker/.env.example` | the new knobs (and the four object knobs currently missing from it) |
| 6 | `hushai-rag/src/routes.rs:436-464` | label-set-aware exhaustive resolution |
| 7 | `hushai-eval/src/manifest.rs:129-153, 243-273` | prefix + skip + conditional content sha |
| 8 | `hushai-loadtest/src/controller.rs:18-34` | `object_detect_custom` in the stage scan |
| 9 | `local_dev/eval.custom.env.example` (new) | staging pins |

---

# Part 4 — Edge build contracts (O4 phone, O5 device)

## 4.0 Shared across both targets

**Golden vectors** live at `contracts/golden/` and are consumed by Kotlin JUnit, Rust `#[test]`s and Python pytest — closing the gap recorded at `AGENTS.md:655` ("shared-proto golden-vector CI"). Four files: letterbox geometry (scale, scaled size, pad, for a set of input sizes), box decode + class-aware NMS (boxes and scores in, kept indices out), `hint.det.*` encoding, and manifest wire bytes (generated by a Rust test behind `UPDATE_GOLDEN=1`, decoded by a Kotlin Wire test and vice versa). Letterbox vectors pin **geometry, not pixels** — resamplers legitimately differ; decode and NMS vectors are exact.

**The `hint.det.*` contract**, documented in `contracts/cameraToBackendContract.md` §8 beside the existing hint family (`:195-204`). It rides *inside* `hint.v="1"`: the parser accepts only that version and drops all hints on anything else (`hushai-backend/src/hints.rs:104-110`), while ignoring unknown keys (`:111-129`). **Never bump `hint.v`.**

| Key | Example | Meaning |
|---|---|---|
| `hint.det.v` | `1` | det-family version |
| `hint.det.model` | `hushdet-n-320-int8@ab12cd34` | model id @ sha8 |
| `hint.det.ep` | `xnnpack` | which runtime actually ran (splits the drift monitor) |
| `hint.det.frames` | `3` | frames inferred in this segment; `0` ⇒ treat as absent |
| `hint.det.n` | `4` | post-NMS detections across those frames |
| `hint.det.max_score` | `0.874` | |
| `hint.det.counts` | `person=2,car=1` | per label, max simultaneous count in any one frame |
| `hint.det.boxes` | `t250:0,87,412,233,118,402;1,63,700,540,210,130\|t760:…` | frames separated by `\|`, each `t<ms>` from segment start; per detection `class_idx,score_pct,x,y,w,h` in per-mille of the upright frame |
| `hint.det.rot` | `90` | rotation applied to reach upright coordinates |
| `hint.det.ms_p50` | `96` | device-side inference p50 (telemetry without adb) |
| `hint.det.trunc` | `1` | present only if boxes were truncated |

Caps: 24 boxes per segment (highest score first) and 2048 bytes hard, truncating whole frames from the end. Locale-invariant decimal strings, as the existing hints already do (`VideoEncoder.kt:186-190`).

**Backend parse** — `hints::parse_det(attrs) -> Option<DetHints>`, gated on `hint.det.v == "1"`; any malformed det field drops the **det family only**, never the motion/audio hints, counted by `hushai_ingest_hint_det_total{result}`. New knobs join `HintGateCfg` (`hints.rs:31-46,64-85`): `INGEST_HINT_DET_ENABLED=true`, `INGEST_HINT_DET_MAX_BYTES=2048`, `INGEST_HINT_DET_VETO_MIN_SCORE=0.5`.

**The one gate use: a veto, never a skip.** In `vision_decision` (`hints.rs:164-178`), if `det.frames > 0 && det.n > 0 && det.max_score >= veto_min` then `Enqueue`, even when the motion score says static. The device reports a measurement; the server owns the threshold; the outcome is only ever *more* processing. Counter `hushai_ingest_hint_det_veto_total`. Explicitly forbidden: any rule where a device seeing nothing causes a skip — that would make the edge detector authoritative, which it is not. A later, optional priority ordering (a `priority` column consulted by `claim_one_vision`, `hushai-worker/src/claim.rs:218-228`) is reordering only, never skipping, and needs a starvation guard.

**Migration `0033_edge_detection_grading.sql`** follows the 0022 principle — device values stay in `segments.attrs`, the status row holds the worker's **grade**:

```sql
ALTER TABLE segment_vision_status
  ADD COLUMN IF NOT EXISTS edge_model         text,
  ADD COLUMN IF NOT EXISTS edge_ep            text,
  ADD COLUMN IF NOT EXISTS edge_det_frames    smallint,
  ADD COLUMN IF NOT EXISTS edge_det_n         smallint,
  ADD COLUMN IF NOT EXISTS edge_server_n      smallint,
  ADD COLUMN IF NOT EXISTS edge_label_jaccard real,
  ADD COLUMN IF NOT EXISTS edge_box_f1        real,
  ADD COLUMN IF NOT EXISTS edge_agreement     real;
CREATE INDEX IF NOT EXISTS segment_vision_status_edge_graded_idx
  ON segment_vision_status (updated_at) WHERE edge_agreement IS NOT NULL;
```

**Worker grading** (`hushai-worker/src/vision/edge_grade.rs`). The device samples different instants than the worker's three frames, so grading is at the **segment set level**: build the device instance set and the server instance set (server boxes with `score >= EDGE_GRADE_MIN_SCORE` whose label is in the device model's vocabulary, converted to per-mille, then deduplicated across frames at IoU ≥ 0.5); `edge_label_jaccard` over label sets; greedy same-label matching at a tolerant `EDGE_GRADE_IOU=0.3` gives `edge_box_f1`; `edge_agreement = ½·jaccard + ½·box_f1`; both-empty scores 1.0. Outcomes recorded as a metric label: `agree` (≥0.7) · `partial` · `disagree` (<0.3) · `edge_only` · `server_only` · `both_empty` · `phone_saw_server_skipped` — that last one, recorded on the motion-skip path (`write.rs:199-261`), is the most interesting drift signal there is. Metrics: `hushai_worker_edge_graded_total{lane,model,outcome}` plus a ratio histogram, which needs a small generalization of `observe_duration` into `observe_with_buckets` (`hushai-backend/src/observe.rs:83-101` hardcodes latency buckets today).

**Dashboard.** `/api/dashboard` `queues.vision` (`hushai-viewer/src/dashboard.rs:81-106`) gains `edge_graded_recent`, `edge_agreement_mean_recent`, `edge_disagree_recent` and a per-model breakdown, in the same 24-hour shape as the existing `audit_agree_recent`/`audit_disagree_recent` fields; the UI adds an `edgeWarning` mirroring the existing audit warning, firing when at least 20 segments were graded and the mean agreement is below 0.5.

**Eval hashing.** Do **not** add `"EDGE_"` to `KNOB_PREFIXES` initially: grading writes status columns, not scored rows. `EDGE_*` and `INGEST_HINT_DET_*` are never pinned in `local_dev/eval.env` (the `GOTHAM_*` discipline), so `d4acc862` is untouched. Add the prefix the day a fixture asserts on `edge_agreement`.

## 4a — The phone (O4)

**Runtime: ONNX Runtime for Android, `com.microsoft.onnxruntime:onnxruntime-android:1.20.0`** — pinned to match the worker's dlopened 1.20.0 so one artifact is validated once. The AAR carries NNAPI and XNNPACK execution providers plus native libraries; **no NDK is needed**, which matters because `hushai-android/app/build.gradle.kts` has no NDK, no `abiFilters`, no `externalNativeBuild` and no `jniLibs` today. Add `ndk { abiFilters += listOf("arm64-v8a") }` to keep the APK from growing by tens of megabytes. Alternatives considered: NCNN (Vulkan on the Adreno 540 is the classic Snapdragon 835 winner) needs NDK and C++ and a second artifact format — rejected on both counts; LiteRT's GPU delegate is the strongest Android-9 accelerator story and needs no NDK but requires `.tflite` conversion, breaking "one artifact" — kept as a documented escape hatch and a good comparison to run once if XNNPACK cannot hit the latency budget.

**Components** (`com.hushai.android.capture.edge`). The single CPU-accessible pixel seam today is the motion analyzer's `ImageReader` (`hushai-android/app/src/main/kotlin/com/hushai/android/capture/MotionHintAnalyzer.kt:33-34`, added as an optional third Camera2 output at `CameraController.kt:122`). Split it:

| Class | Responsibility |
|---|---|
| `FrameAnalyzer` | Owns the one `ImageReader` and its `HandlerThread`; `acquireLatestImage` → dispatch to each registered consumer synchronously → always close the image. Exposes `surface`. The Camera2 stream count stays at three. |
| `MotionHintSampler` | The existing 32×32 luma-tile mean-subtracted MSE, byte-for-byte (`MotionHintAnalyzer.kt:72-128`), including `take()` |
| `EdgeDetector` | Fused YUV→RGB letterbox into a reusable buffer on the reader thread (≤ 4 ms), then hands off to its own thread for inference; drops the frame if the previous one is still running |
| `YuvToRgb` | Fused downscale + BT.601 limited-range conversion, plane/row/pixel-stride aware; unit-testable on synthetic planes |
| `DetPostprocess` | Sigmoid, threshold, cxcywh→xywh, class-aware greedy NMS (matching `hushai-worker/src/vision/geom.rs`), un-letterbox, rotate to upright |
| `DetHints` | `DetWindow` → the `hint.det.*` map, with the caps |
| `EdgeModelSpec` | Parses `model_card.json` (§2.5) |
| `EdgeSession` | ORT lifecycle: EP chain, warm-up, optional profiling, teardown |
| `EdgeStats` | Ring buffers → p50/p95 logged every 60 s under `HUSHAI_TX` |

Wiring in `CaptureService.startCapture` (`CaptureService.kt:433-475`, analysis surface passed at `:499-502`): build `FrameAnalyzer`, always register the motion sampler, register the detector only when the setting is on. `VideoEncoder` gains a second attr provider merged beside `motionHintAttrs()` (`VideoEncoder.kt:168-193`); `GlVideoPipeline` forwards it. `Segment.attrs` is untouched (`Segment.kt:34-39`), so offline replays carry identical det hints.

**Frame path.** Detector off: the reader stays exactly `320x240 YUV_420_888, maxImages=2` — zero behavior change on the default path. Detector on: pick the largest `YUV_420_888` output ≤ 640 wide whose aspect matches the encoder size within 1% (on the S8, 640×360 against a 1280×720 encoder). Same-aspect is load-bearing: Camera2 crops different aspect ratios to different fields of view, and the boxes are normalized to the frame. Fuse the downscale into the conversion and write directly into the letterboxed model buffer with the card's padding rule (1.5–4 ms; a naive per-byte full-resolution loop is 8–15 ms — measure both, it is a good lesson). Prefer a u8 NHWC model input with normalization folded into the graph: no float pass on the phone, friendlier to NNAPI, and the same artifact still works in Rust and Python because the card says so. Model input 320 primary, 416 as the accuracy/latency row. Boxes are rotated to upright with the same value the encoder stamps (`OrientationTracker.orientationHint()`, used at `VideoEncoder.kt:157`) and `hint.det.rot` records it.

**Cadence, threading, backpressure.** One inference per `EDGE_SAMPLE_INTERVAL_MS=500` (≈2 fps, 3–4 frames per segment, matching the worker's three). ORT with `setIntraOpNumThreads(2)` (sweep 1/2/4), sequential execution, all optimizations, and `session.intra_op.allow_spinning=0` — spinning is the classic mobile battery killer. One reused direct buffer and tensor; never allocate per frame. An `AtomicBoolean` drops frames rather than queueing; the reader never blocks; the encoder surface is a separate output of the same request and is never touched.

**Lifecycle.** The existing `onConfigureFailed` retry drops the analysis surface on limited hardware (`CameraController.kt:154-166`) — unchanged semantics, since `FrameAnalyzer` replaces the same single surface. EP chain falls back nnapi → xnnpack → cpu, logging which one actually ran and how many nodes it took; a missing asset or a total failure logs and disables, and capture continues (fail-open end to end). Teardown joins the inference thread with a bound before closing the analyzer.

**Settings and assets.** `Settings.edgeDetect()` plus developer keys for EP, threads, model and profiling (the `uprightBake` precedent at `hushai-android/.../config/Settings.kt:40,92`), intent extras beside `EXTRA_UPRIGHT_BAKE` (`CaptureService.kt:889-901`), one switch in the capture screen with a status subtitle. Models ship as gitignored assets fetched by `local_dev/fetch_edge_models.sh` (the `fetch_vosk_models.sh` precedent).

**Expected budgets on the Snapdragon 835** (Kryo 280 is ARMv8.0 — **no dot-product instructions**, which is the whole int8 story here):

| Item | Expectation |
|---|---|
| Inference @320, XNNPACK fp32, 2 threads | 90–160 ms |
| Inference @320, CPU int8 QDQ | 70–140 ms (1.3–1.8× gain, not 3×) |
| Inference @320, NNAPI | 40 ms to 400+ ms — driver-dependent; verify a vendor driver exists at all with `adb shell lshal \| grep neuralnetworks` |
| @416 | ×1.6–1.8 |
| Pre / post | 1.5–4 ms / 1–3 ms |
| Incremental power | 150–400 mW over a ~1.2–1.8 W capture baseline |
| RSS delta | +25–60 MB |

**Profiling protocol** (a first-class deliverable, not a side effect). ORT session profiling writes a JSON that groups by operator and provider — that table *is* the evidence for where time goes and how much fell back to CPU. End-to-end latency percentiles are logged under `HUSHAI_TX`. CPU from `top`/`dumpsys cpuinfo`; memory from `dumpsys meminfo`; **thermal from `/sys/class/thermal/thermal_zone*` and CPU frequency scaling, because `dumpsys thermalservice` is API 29+ and this phone is API 28**; battery from `dumpsys batterystats --reset` → soak over wireless adb → `--charged`, cross-checked against `current_now` × voltage sampling; frame drops from the app's own counters plus `ffprobe -count_frames` on pulled segments. `local_dev/edge_profile.sh` runs the grid (model × EP × threads), takes the phone lock (`local_dev/phonelock.py`), uses `logcat -d` rather than a streaming logcat (the recorded wedge), and emits a Pareto table.

**Encoder-untouched proof** (the exit criterion for the integration phase): two-minute static-scene runs with the detector off and on; compare per-segment frame counts and byte sizes; `dumpsys media.camera` shows three streams in both.

**Tests.** Golden vectors in JUnit; a JVM fake-frame harness (desktop ORT swapped in for the Android AAR) so the whole pre/infer/post path runs on a PNG off-device; an on-device self-test intent that runs a pushed PNG and logs the result for comparison against the Python reference; and `local_dev/physical_loopback.py --edge --expect-edge-hints`, which asserts that segments carrying `hint.det.v` arrived and that `edge_agreement` was populated.

## 4b Dedicated edge device (O5)

**The device is just another conforming camera.** It POSTs `SegmentManifest` + body per `contracts/cameraToBackendContract.md`, buffers offline, and attaches `attrs`. Nothing server-side branches on it — §7 of that contract is explicit that `source_kind` is descriptive and "adding a new camera type requires no backend change" (`:176-183`). Zero backend changes are required by this track beyond the shared 4.0 work.

### Hardware decision (as of 2026-09; re-verify price and stock at purchase)

| Candidate | Price | Realistic INT8 detector speed | Toolchain and host requirement | Power | Camera + encode | Maturity | Industry relevance | Learning value |
|---|---|---|---|---|---|---|---|---|
| **Jetson Orin Nano Super dev kit (8 GB)** | ≈$399 since mid-2026 (was $249); a used *original* Orin Nano kit ($200–300) becomes "Super" via the JetPack 6.2 firmware — same silicon | 67 TOPS. Nano-class detector @640 FP16 ≈ 4–6 ms model-only, INT8 ≈ 3–4.5 ms; Python end-to-end 40–80 fps. Our 1–5 fps loop is trivial | ONNX → TensorRT (`trtexec`, Polygraphy) or ORT with the TensorRT EP. **All on-device**; an x86 host is needed only for SDK-Manager flashing (an SD-card image works from the Mac) | 7 W / 15 W / 25 W / uncapped modes via `nvpmodel`; `tegrastats` exposes per-rail milliwatts | 2× CSI (IMX219/IMX477 with in-tree overlays), USB 3. **No hardware encoder, no DLA, no PVA** — H.264 is software (≈0.5–1 core at 720p15) | Excellent | **Highest** — TensorRT is the hiring keyword in robotics, drones and maritime autonomy | **Highest**: quantization plus power modes is literally the job's trade-off sentence |
| Jetson Orin NX / AGX / Thor | $700–3,500 | 100–2,000 TOPS | Same JetPack/TensorRT; adds NVENC and DLA cores | 10–60 W | Hardware encode present | Same | Same | Overkill — identical skills, only DLA is new |
| Raspberry Pi 5 + AI HAT+ (Hailo-8L/8) or AI HAT+ 2 (Hailo-10H, ≈$130) | Pi ≈$80–90 + HAT $70–130 | YOLOv8n-class @640 ≥40 fps on-chip; 25–40 fps end-to-end in Python; the Pi's single PCIe lane halves the vendor numbers | ONNX → HAR → HEF via the **Hailo Dataflow Compiler, x86_64 Linux only** — Apple-Silicon Docker emulation is not viable; budget a cloud VM or a used mini-PC | 5–12 W; no formal power modes | 2× CSI; **no hardware H.264 encoder** | Very good | High in automotive/dashcam and smart cameras | High: a real quantizing compiler, op coverage, on-chip versus host NMS |
| Raspberry Pi AI Camera (Sony IMX500, in-sensor) | ≈$70 | Tiny models only (8 MB on-sensor); single-digit to ~30 fps by model | PyTorch → Sony MCT quantizer (runs on the Mac) → converter → packaging **on the Pi**. Needs the `.pt`, not just ONNX | 1–2 W including the host | Detections arrive as camera metadata; zero host inference | Good | Niche (Sony's smart-camera ecosystem) | Medium-high but narrow — the most constrained target you can buy |
| Rockchip RK3588 boards | $120–200 | 6 TOPS; YOLO-nano @640 ≈ 20–60 fps across 3 NPU cores | ONNX → RKNN; arm64 Linux wheels exist, so an arm64 container on the Mac works | 5–15 W | **Real hardware H.264/H.265 encode** — the only cheap board here with it | Medium (vendor BSP kernels) | High in Chinese IPC/NVR OEMs | Medium; toolchain friction |
| Qualcomm RB3 Gen 2 / Dragonwing kits | $399–599 | 12 TOPS | Qualcomm AI Hub compiles in the cloud and profiles on hosted devices — Mac-friendly | 5–15 W | Hardware encode | Medium | High in drones/robotics | Medium-high |
| Luxonis OAK 4 | $749+ | 52 TOPS, standalone Linux camera with PoE | DepthAI + Hub conversion | ~10 W | Integrated sensor + hardware encode; **closest to a product-shaped smart camera** | New | High for product work | High, but expensive and proprietary |
| Google Coral | — | 4 TOPS, TFLite only | Compiler abandoned; repo archived 2026 | — | — | **Dead** | None | Do not buy |

**Recommendation: the Jetson Orin Nano Super dev kit**, with the used-original-kit option if the price or backorder bites. TensorRT is the transferable skill; the power modes turn "balance accuracy, latency, memory and power" into an afternoon of measurements. Accept the real gotcha: software H.264 encode (mitigations below).

**The dashcam-sized wish, answered honestly.** Real dashcam SoCs (Ambarella, Novatek, Sigmastar) are NDA/OEM toolchains, not hobbyist-programmable. The closest achievable form factor is a **Pi Zero 2 W + AI Camera (IMX500)**, ≈$115: in-sensor detection, a hardware H.264 encoder on the host, Wi-Fi, about 2 W, in a dashcam-style housing, running the same `hushai-edge` client with a different detector backend. If breadth of NPU-compiler experience matters more than form factor, choose Pi 5 + AI HAT+ instead (and budget the x86 VM).

**Kit budget.** Primary ≈$640: kit $399 (or $200–300 used), NVMe $40, microSD $12, IMX477 camera $50 + lens $25 + CSI cable $5, plug wattmeter $20, case/mount $30, pan-tilt servo kit $30, acrylic shield + spray bottle $10, 850 nm IR illuminator $15. Secondary ≈$115 (Zero 2 W + AI Camera + card + PSU + case) or ≈$250 (Pi 5 + AI HAT+ 2 + cooler + PSU). Cloud x86 for Hailo compiles, if taken: $10–30 total.

### The client

Python, managed by `uv`, with `--system-site-packages` on-device because `tensorrt`, GStreamer's `gi` bindings and `picamera2` are system packages. Rust was considered and rejected: every vendor runtime is Python-first, there are no maintained crates for TensorRT or HailoRT, `picamera2` has no equivalent, the capture hot path lives inside GStreamer's C elements either way, and pre/post-processing is *shared by import* with `hushai-train`.

```
hushai-edge/hushai_edge/
  cli.py        run | doctor | status | bench   (--source file.mp4 = fake camera; --detector null|ort|trt|hailo|imx500)
  config.py  identity.py  manifest.py     # port of local_dev/feed_segments.py's manifest building
  capture/{pipeline,sources,encoders,segmenter,mp4}.py
  detect/{runtime,trt_runtime,ort_runtime,hailo_runtime,imx500_runtime,null_runtime,prepost,loop,model_card}.py
  hints.py      # port of MotionHintAnalyzer.kt + the shared hint.det.* encoder
  buffer.py     # port of DurableSegmentBuffer.kt semantics
  uploader.py delivery.py                  # port of Uploader.kt outcome classification
  health.py     # /metrics + health.jsonl + grep-stable markers
  telemetry/{jetson,rpi,hailo,generic}.py
  service/hushai-edge.service  config.example.toml
tools/  provision.sh provision_remote.sh gen_proto.sh build_engine.sh calib_set.py
        quantize_qdq.py hailo_compile.sh imx500_convert.sh deploy_model.sh profile.py edge_soak.sh
```

**Capture.** A GStreamer graph: camera source → `tee` → (a) software x264 at 720p15 → `h264parse` → `splitmuxsink` with `max-size-time=2s` writing self-contained MP4 segments, and (b) a leaky queue → `videorate` at 1–5 fps → `appsink` for the detector. Container `mp4`, codec `h264`, media type `VIDEO`, `stream_id="cam0-video"` — the same shape the Android client produces, so the worker's frame extraction (`hushai-worker/src/vision/frames.rs:38-64`) handles it with no special case. Timing: record the wall/monotonic clock pair when the pipeline starts playing, then derive each segment's `capture_start_unix_nanos` and `monotonic_start_nanos` from the fragment-opened running time; wait for the `moov` box to be present and the file size to settle before hashing, because the fragment-closed signal can precede the final close. A USB camera that emits H.264 natively bypasses software encode entirely — the cheapest fix if encode cost bites.

**Everything else mirrors Android semantics deliberately**: the durable buffer (tmp+fsync+rename sidecars, crash recovery, byte-bounded with drop-oldest setting `gap_before`, quarantine for permanent client errors), the uploader's outcome classification and backoff constants, and the motion hint computation. Where Android and the device disagree, the golden vectors decide.

**Health and operations.** A Prometheus endpoint plus one JSONL line per segment (capture end, finalized, upload 200, detection count, inference ms, power, temperature) — the rig copies that file as evidence. A systemd unit with a watchdog; `tools/provision_remote.sh` runs the whole install over ssh from the Mac and ends with `doctor --json`.

### Model deployment on the device

**TensorRT (JetPack 6.2.x pinned).** Bring-up with the ORT TensorRT EP and engine caching (one code path shared with the Mac). Production and learning with raw TensorRT: `trtexec --onnx=… --saveEngine=… --fp16` at static shapes, then INT8 **two ways on purpose**: (a) implicit calibration with a cache built from our own frames — the path every Jetson tutorial still shows, deprecated in TensorRT 10 and removed in 11, worth doing once to understand entropy versus min-max versus percentile calibrators and to use `polygraphy debug precision` to find the layer that breaks; (b) explicit Q/DQ, which is the **same artifact** `hushai-train quant static` already produces for the Mac and the phone. Expect CNNs to gain 1.2–1.8× with under a point of mAP; expect RF-DETR's LayerNorm/attention to gain little and occasionally to cliff — keep it FP16 or mixed, and say so in the report. **The Orin Nano has no DLA**, so document `--useDLACore` as the NX/AGX path only; the Nano's lesson is CPU pre/post offload. Engines are cached per `<jetpack>-<trt>-<arch>` and the loader refuses a card whose runtime does not match — engines are not portable, and a silent mismatch is worse than a loud failure.

**Hailo and IMX500** paths are documented for the optional second device: Hailo is parse → optimize with a calibration set and a model script (including on-chip NMS and 16-bit precision for sensitive layers) → compile, all on an **x86 Linux host**; IMX500 is Sony MCT quantization of the `.pt` on the Mac, then conversion and packaging (the packaging step must run on a Pi), with hard constraints — int8 only, 8 MB on-sensor, a CNN-only operator set.

### Profiling protocol

`tools/profile.py` runs a grid of model × precision × power mode: set the power mode and settle, start `tegrastats` logging, warm up, then measure p50/p95/p99 latency, throughput, RSS, and energy per inference (mean watts × latency). Per-layer timing comes from `trtexec --dumpProfile`. A `--stress` mode exists because **at 1–5 fps the detector is a few percent duty cycle and system power is dominated by idle plus software encode** — a finding to record, not hide. Power is reported **twice**: the module rails from `tegrastats` and the wall reading from the plug meter, because they measure different things. A thirty-minute soak catches thermal throttling by watching the GPU clock against the mode's maximum. End-to-end camera-to-upload latency comes from the JSONL stamps plus the server's received time, with both clocks NTP-disciplined and the offset recorded.

The output row schema is shared with the phone track (`contracts/edge-profile-schema.json`): run id, device, OS version, model id, resolution, precision, runtime, power mode, clocks pinned, latency percentiles, fps, end-to-end percentiles, RSS, GPU/CPU utilization, watts (rail and wall), millijoules per inference, temperatures, throttled flag, accuracy proxy, calibration method and size, artifact sha.

**Planning estimates, to be replaced by the first real run:** nano-class detector @640 FP16 ≈ 4–6 ms, INT8 ≈ 3–4.5 ms, @416 INT8 ≈ 2–3 ms (model only); 15 W mode ≈ 1.3–1.6× slower than uncapped, 7 W ≈ 2–3×; module power 4–5 W idle, 10–14 W loaded at 15 W mode, 18–25 W uncapped, plus 3–5 W at the wall.

### Field-condition emulation (the marine relevance)

Three layers. **Screen presets** (`local_dev/field_presets.json`) as ffplay filter chains: harsh light, flicker, fog/spray blur plus noise, night, and sea-state roll/sway via time-varying rotation and crop. **Physical**: a clear acrylic shield about 10 cm in front of the lens, sprayed with water (real defocused droplets cannot be emulated on a screen), a servo pan-tilt executing the same sinusoidal roll profiles, and a dark room with an 850 nm illuminator. **Content**: publicly available marine datasets viewed locally (never redistributed) plus own harbour footage. Output is a robustness matrix of preset × model × precision → recall proxy and device-versus-server agreement. Then `edge_loopback.py --harvest` exports every frame where the device and server disagreed, or where scores landed in the ambiguous band, into the labeling flywheel — and the loop closes: label, fine-tune, **re-calibrate including the new conditions**, re-profile, compare the Pareto rows before and after. That before/after is the interview story.

### Rig and tests

`local_dev/edge_loopback.py` is the ssh-driven analogue of `physical_loopback.py`, sharing its database guard and tolerant scoring. It takes the existing phone lock because the contended resource is the Mac's screen. Preflight (ssh reachable, service enabled, `doctor` clean, clock offset small) failing means `SKIP` and exit 0 — the Gauntlet Rule 9 posture, where a skip is recorded but is not a failure. The scored run asserts expected labels present, `hint.det.*` on at least 90% of segments, label agreement above a floor, end-to-end latency percentiles, and zero unexpected gaps; an `--offline` variant pulls the network down mid-run and checks that store-and-forward closed the gap. Unit tests (pytest, on the Mac) cover manifest bytes against the conformance corpus, buffer recovery and eviction, uploader classification against a fake server, and the segmenter producing keyframe-aligned 2 s files.

## 4.1 Part-4 touch points

| # | File | Change |
|---|---|---|
| 1 | `contracts/golden/` (new) + three consumers | letterbox · decode/NMS · hint encoding · manifest vectors |
| 2 | `contracts/cameraToBackendContract.md:187-207` | document the `hint.det.*` family |
| 3 | `hushai-android/.../capture/edge/` (new) + `MotionHintAnalyzer.kt`, `CameraController.kt`, `CaptureService.kt`, `VideoEncoder.kt`, `Settings.kt`, `build.gradle.kts` | §4a |
| 4 | `hushai-backend/src/hints.rs` | `parse_det`, `INGEST_HINT_DET_*`, the veto |
| 5 | `hushai-backend/migrations/0033_edge_detection_grading.sql` | §4.0 |
| 6 | `hushai-worker/src/vision/edge_grade.rs` (new) + `write.rs`, `claim.rs` | grading + persistence |
| 7 | `hushai-backend/src/observe.rs:83-101` | `observe_with_buckets` for the ratio histogram |
| 8 | `hushai-viewer/src/dashboard.rs:81-106` + `ui/js/dashboard/dashboard.js` | edge fields + warning |
| 9 | `hushai-edge/` (new tree) + `local_dev/{edge_profile.sh,edge_loopback.py,field_presets.json,fetch_edge_models.sh}` | §4a/§4b |
| 10 | `local_dev/physical_loopback.py` | `--edge`, `--expect-edge-hints` |

---

# Part 5 — E2E verification capability (O6)

## 5.1 Determinism class

`detections` is **Tier-1 deterministic**, database-direct like `objects`: the same clip with a pinned `base_capture_unix_nanos` (eval invariant 3), the CPU execution provider, and hashed `OBJECT_*` / `CUSTOM_DET_*` / `FRAMES_PER_SEGMENT` knobs produces byte-identical boxes. Latency is `Info` only. Eligible for `train` and `holdout`; custom-lane fixtures start in `staging` until a model exists; own footage lives in a new gitignored **`fixtures/local/`** split that runs only with `--fixtures local` and is never part of `all` (the `staging` exclusion at `hushai-eval/src/main.rs:71-73` is the precedent).

## 5.2 New assertion fields

| Field | Semantics |
|---|---|
| `frames[{offset_ns, boxes[{label, bbox, ignore?}]}]` | per-sampled-frame ground truth in original-frame pixels; `ignore` boxes make matched detections neither true nor false positives (truncated or ambiguous objects) |
| `iou_thresholds` (default `[0.50 … 0.95]`) · `frame_snap_ns` (default 400 ms, the viewer's tolerance) | matching parameters |
| `min_map50` (default 0.5) · `min_map50_95?` · `min_ap_small?` · `min_recall50?` · `max_fp_per_frame?` | floors → `floor_ok` |
| `classes[]` · `expect_no_labels[]` | which classes enter mAP; which must produce nothing |
| `detector` (`rfdetr-coco` default \| `custom` \| `any`) | which lane is scored; `custom` with the lane off degrades to `Info`, never to FAIL |
| `score_floor?` | the product's operating point (e.g. 0.4, matching `EVENTS_OBJECT_MIN_SCORE`) for `recall@op` and `fp_per_frame`; AP itself stays threshold-free |
| `small_max_px` (default 32) · `exhaustive` (default true) | the COCO small-area bound; whether observed frames absent from ground truth count as all-false-positive frames |

Metrics: `detections.map50` (floor), `detections.map50_95`, `detections.ap_small`, `detections.recall@op`, `detections.fp_per_frame` (lower is better), `detections.ap.<class>` (Info), `detections.no_labels.<class>` (Boolean), `detections.custom_rows` (Info, Boolean-zero when the fixture pins the custom lane off), `detections.latency_ms` (Info).

**Why more than mAP.** mAP is threshold-free, so it says nothing about the confidence the product actually fires alerts at; it is blind to false positives on classes with no ground truth in the frame; and macro-averaging flatters rare classes. `recall@op`, `fp_per_frame` and a sealed negatives fixture cover those three holes. Expect at least one retrain where mAP rises and `recall@op` falls — that case is the lesson, and it belongs in the write-up.

## 5.3 Fixture bank D1–D9

| # | Case | Split | Scenario | Key assertions |
|---|---|---|---|---|
| D1 | `det_car_bbox` | train | the existing `car_object` media, ground truth from the generator | `map50`, `recall@op(0.4)`, `fp_per_frame` — the COCO lane's **box-level** regression guard (`objects.label_f1` cannot see a box drifting off the car) |
| D2 | `det_two_classes` | train | two public-domain stills composited side by side | per-class AP; proves class-aware matching |
| D3 | `det_small_far` | train | D1's still pre-shrunk so the car is ~24 px | `ap_small`, frozen at first observation |
| D4 | `det_custom_off_identical` | train | D1 media with `CUSTOM_DET_*` unset | `custom_rows == 0` **and** D1's metrics unchanged under `d4acc862` |
| D5 | `det_custom_on_coco_intact` | staging → train once a model exists | D1 media with the custom lane on | COCO-filtered metrics equal D1's frozen values — the lane is additive |
| D6 | `det_wildlife_fox` | staging | a public-domain wildlife still, `detector: custom` | custom-lane `map50` (Info-skips while the lane is off) |
| D7 | `det_night_domain` | staging → train | D1's still darkened and noised deterministically | the expected accuracy drop, frozen — a domain-shift guard |
| D8 | `det_negatives_empty` | **holdout, sealed** | a landscape with no COCO object | `fp_per_frame` floor + `expect_no_labels` — the counter-fixture invariant 7 asks for |
| D9 | `det_home_field` | `local` (gitignored) | own footage labeled in Label mode → exported → converted | the field set; `recall@op` here against D1 is the field-versus-test gap |

**Ground truth: generated for D1–D8, hand-labeled for D9.** The eval clips are ffmpeg slow zoom-pans over a still (`local_dev/fetch_eval_clips.sh:81-104`), so where a box lands on each sampled frame is arithmetic. `local_dev/gen_det_gt.py` renders the clip with the *same* chain (the shell script calls it, so there is one source) and emits `expected.json` analytically from the pan/zoom parameters and the deterministic sample offsets. It **self-verifies without any model**: `--verify` renders a white rectangle on black, extracts the sampled frames, thresholds the rectangle, and asserts the analytic box within 2 px — a unit test of the ground truth, not of the detector. That same fiducial clip is what PR0 uses to settle the frame-offset question. Hand-labeling is reserved for footage where no analytic truth exists; `local_dev/labels_to_fixture.py` converts a COCO export into a fixture.

**Calibration protocol** (house doctrine, verbatim): the first live run freezes observed values and floors sit *below* them — widen, never narrow; two back-to-back runs with identical verdicts before any content assertion is frozen; staging → train only after human review; **never loosen a gate to go green** (`RECURSIVE_TESTING.md:203-207`). Tolerance bands are tight here: 0.02 for AP and recall keys, 0.05 for `fp_per_frame`, 0 for Booleans — the default 0.10 band (`hushai-eval/src/baseline.rs:95`) would hide a nine-point mAP regression on a deterministic pipeline.

## 5.4 Harness touch points

| # | File | Change |
|---|---|---|
| 1 | `hushai-eval/src/fixtures.rs:115, 262-291, 663-676` | `DetectionsGt`/`DetFrameGt`/`DetBoxGt`, `Expected.detections`, `needs_vision()` gains `"detections"`, a parse test over D1–D8 |
| 2 | `hushai-eval/src/query.rs:21-27, 217-232` | `ObjectDet` gains `bbox`, `score`, `frame_offset_ns`, `detector`; the query runs for `objects` or `detections`; read `detector` defensively so a pre-0032 database still parses |
| 3 | `hushai-eval/src/score.rs:45-89` | `score_detections` gated on the modality; pure helpers `iou`, `greedy_match_per_class`, `ap_101`, `snap_frames`; ten named unit tests including a hand-computed AP case, a no-double-match case, ignore-region handling, area filtering, snap rejection, and the custom-lane Info-skip |
| 4 | `hushai-eval/src/baseline.rs:77-95` | the tight `detections.` bands |
| 5 | `hushai-eval/src/manifest.rs:129-153, 243-273` | `"CUSTOM_DET_"` prefix, `models/custom/` skip, conditional content sha (Part 3.4) |
| 6 | `hushai-eval/src/ctx.rs` + `lib.rs` | `HUSHAI_WORKER_METRICS_URL` (default `http://127.0.0.1:9100/metrics`, per `WORKER_METRICS_ADDR` at `hushai-worker/src/config.rs:866`) for the Info latency metric — unreachable means the metric is omitted, never INCONCLUSIVE; plus `--dump-pr` writing per-class PR curves for operating-point selection |
| 7 | `hushai-eval/RECURSIVE_TESTING.md` | the modality paragraph, the `local/` split, the band row |
| 8 | `local_dev/gen_det_gt.py`, `local_dev/labels_to_fixture.py` (new); `local_dev/fetch_eval_clips.sh:81-104` | generator + shared render chain |
| 9 | `hushai-eval/fixtures/{train,holdout,staging}/det_*` | D1–D8 |

## 5.5 Tier-2 (the realism gate)

`local_dev/physical_loopback.py --expect-detections car:2,person:1 [--detector custom:<name>]` — tolerant presence counting over `scene_objects` at the product threshold, matching the existing `--expect-objects` posture. It additionally computes coverage (frames containing the label ÷ frames sampled, using the `__frame__` rows as the denominator) and prints it beside the Tier-1 `recall@op` of the fixture named by `--scenario`, appending a row to `local_dev/logs/field_gap.jsonl`. **That gap is the number the whole flywheel exists to close, and it is the number to quote in an interview.**

---

# Part 6 — Acceptance phases

Executed after each wave; fenced commands with bold PASS criteria. Run from the repository root; a failed criterion stops the run.

**Phase 0 — Preconditions.** Existing full gate exit 0 (`cargo run -p hushai-eval -- run --tier full --fixtures all`); migrations applied on dev and `hushai_test`; vision models provisioned; PR0 landed and its finding recorded. **PASS:** all green before any Osprey commit is judged.

**Phase A — Build and unit gates.** `cargo build --workspace` + `cargo clippy --workspace` clean; eval unit total stated explicitly (currently ≈35, growing by the detection-scorer and parse tests); backend label unit tests (bbox clamping, category-id stability, queue tie-breaks); `gen_det_gt.py --verify` within 2 px; `hushai-train` pytest green including the licence-hygiene test. **PASS:** counts met, zero failures.

**Phase B — Labels API at curl level.** Through the viewer proxy: create a frame (the PNG appears under `label_frames/` with correct dimensions), upsert accept/reject/add, `done` refused with 409 then accepted, export writes a directory `pycocotools` can load; `audit_log` carries `label.upsert` and `label.export` with details; no bearer returns 401. **PASS:** shapes, audit rows and auth posture all verified.

**Phase C — Detections fixtures Tier-1.** D1, D2, D4, D8 per-case → `--update-baseline` → two gating runs with identical verdicts; the 28 existing baselines still green with `CUSTOM_DET_*` unset. **PASS:** exit 0 twice, **lineage unchanged**.

**Phase D — Viewer Label mode.** Five e2e checks green with the native-dialog guard still last; manually label 20 frames of real footage in under 10 minutes, keyboard-only; observe the `done` gating refuse an unreviewed frame. **PASS:** all checks green, the manual session completed.

**Phase E — The flywheel closes.** Queue → 200 labeled frames across at least three classes → export → ingested by `hushai-train` → D9 built → `recall@op` on D9 recorded next to D1. **PASS:** one full loop, with numbers in the result matrix.

**Phase F — Custom lane.** A trained model exported, contract test green, `models/custom/` populated; `CUSTOM_DET_ENABLED=true` produces custom rows visible in the viewer overlay and answerable in chat; D5/D6 calibrated in staging with two identical runs; with the lane off, D1–D4 are bit-identical. **PASS:** additive proven both ways.

**Phase G — Phone edge.** Golden vectors green in all three languages; a 10-minute run with the detector on shows zero encoder impact (frame counts and byte sizes match a detector-off run); `hint.det.*` present in `segments.attrs`; `edge_agreement` populated by the worker; the Pareto table exists with at least model × EP × precision rows and a written EP decision. **PASS:** no recording regression, grading live, decision documented.

**Phase H — Device edge.** 24-hour soak with zero unexpected gaps and upload p95 under a second; TensorRT FP16 and INT8 engines built and validated against the FP32 reference; at least three precisions × three power modes × two models measured; a 30-minute soak reports whether and when throttling began; the field-condition rig produces a robustness matrix and at least one harvested hard-example batch that went through the flywheel. **PASS:** the table, the memo, and one closed loop.

**Phase I — Tier-2 field gap.** `physical_loopback.py --expect-detections … --scenario det_car_bbox` runs to PASS or DEGRADED and appends a `field_gap.jsonl` row. **PASS:** the gap is measured and written down.

---

## §7 Config reference

**`LABELS_*` (backend)** — not hashed; nothing here shapes pipeline output, so do **not** prefix-fold it.

| Knob | Default | Meaning |
|---|---|---|
| `LABELS_ENABLED` | `true` | mount `/v1/labels*` |
| `LABELS_FRAME_DIR` | `<BLOB_DIR>/label_frames` | snapshot root |
| `LABELS_EXPORT_DIR` | `<BLOB_DIR>/label_exports` | export root |
| `LABELS_QUEUE_LOW_CONF_LO` / `_HI` | `0.3` / `0.5` | the hard-example confidence band |
| `LABELS_QUEUE_MAX_CANDIDATES` | `200` | cap before the novelty pass |
| `LABELS_SWS_FLAGS` | `accurate_rnd+full_chroma_int` | pinned colour conversion |
| `LABELS_MIN_REVIEW_SCORE` | `0.3` | `done` requires a verdict on boxes at or above this (matches the viewer's display floor) |

**`CUSTOM_DET_*` (worker)** — prefix-folded into `KNOB_PREFIXES`, folding only when set; no secrets in the family. ★ = also contributes the conditional content sha.

| Knob | Default | Hash? | Meaning |
|---|---|---|---|
| `CUSTOM_DET_ENABLED` | `false` | ✔ | master switch; false ⇒ lineage identical |
| `CUSTOM_DET_NAME` | — | ✔ | becomes `detector='custom:<name>'` |
| `CUSTOM_DET_MODEL_PATH` | `./models/custom/<name>/model.onnx` | ✔ ★ | |
| `CUSTOM_DET_CLASSES_PATH` | `…/classes.json` | ✔ ★ | |
| `CUSTOM_DET_CARD_PATH` | `…/model_card.json` | ✔ | drives preprocessing |
| `CUSTOM_DET_INPUT_SIZE` | `384` | ✔ | overridden by the card when present |
| `CUSTOM_DET_MIN_DET_SCORE` | `0.4` | ✔ | per-class operating points from the card win |
| `CUSTOM_DET_NMS_IOU` | `0.5` | ✔ | |
| `CUSTOM_DET_MAX_PER_FRAME` | `20` | ✔ | |
| `CUSTOM_DET_MIN_BOX_PX` | `16` | ✔ | |
| `CUSTOM_DET_REQUIRED` | `false` | ✔ | true ⇒ fail the vision subsystem loudly if the model is missing |
| `OBJECT_DET_CARD_PATH` | — | ✔ (`OBJECT_` prefix) | card for the COCO session; absent = today's hardcoded behavior |

**`EDGE_*` (worker) and `INGEST_HINT_DET_*` (backend)** — **not** hashed initially and never pinned in `local_dev/eval.env`: `EDGE_GRADE_ENABLED` (`true`), `EDGE_GRADE_MIN_SCORE` (`0.4`), `EDGE_GRADE_IOU` (`0.3`), `EDGE_MODEL_SPEC_DIR` (`./models/edge`), `EDGE_GRADE_LABELS` (fallback vocabulary); `INGEST_HINT_DET_ENABLED` (`true`), `INGEST_HINT_DET_MAX_BYTES` (`2048`), `INGEST_HINT_DET_VETO_MIN_SCORE` (`0.5`).

**Eval:** `HUSHAI_WORKER_METRICS_URL` (not hashed). **Python side** (`HUSHAI_TRAIN_DATA`, `PYTORCH_ENABLE_MPS_FALLBACK`, `PYTORCH_MPS_HIGH_WATERMARK_RATIO`) never reaches the Rust config and never enters the hash.

---

## Non-goals / referenced, not duplicated

1. **The intelligence layer** — entity graph, anomalies, the Detective agent: owned by [`gotham.md`](gotham.md). Osprey feeds it better detections; it does not touch the graph.
2. **The aerial import lane** — declined during scoping. Small-object *techniques* (tiling, a stride-4 head) live in O7's small/far lane, and drone imagery is used as an **off-app benchmark** in `hushai-train`, never ingested as a camera. A geospatial extension is sketched in the appendix.
3. **A general annotation tool** — no polygons, no segmentation masks, no object tracks, no multi-user assignment or review workflow. Label mode grades boxes on sampled frames, and that is all.
4. **Cloud training as the default** — the burst path exists and is reproducible, but the curriculum is designed to complete on the M3 Pro.
5. **A real dashcam SoC** — Ambarella/Novatek-class toolchains are NDA-gated; the Pi Zero 2 W + AI Camera is the achievable form factor.
6. **Browser inference** — one paragraph in the appendix; it proves portability, it does not teach edge constraints.
7. **Audio, ASR, speaker identity, plates** — untouched. The plate lane keeps its own detector and its own decode path.
8. **Auth hardening** — the camera-token-is-admin-token gap is pre-existing (`AGENTS.md:647-648`); PR23 is an optional narrowing for label endpoints only.

---

## Risks / open questions

1. **The frame-offset claim may be wrong.** It is inferred from ffmpeg's `fps` filter semantics, not yet measured on this build. PR0 measures first and records the answer either way; nothing downstream assumes the fix landed until it is verified.
2. **MPS operator gaps.** `grid_sampler_2d_backward` and `deform_conv2d_backward` are missing; the mitigations (gather sampler, CPU fallback) change numerics, so the device and fallback flag are recorded in every run manifest and cross-device claims need two seeds.
3. **Label scarcity in our own domain.** The `field` split will be small for a long time. It is **eval-only**, split into dev and sealed halves so threshold picking cannot leak into the sealed score; pipeline pseudo-labels carry the existing detector's biases and are marked as such.
4. **Label noise caps achievable accuracy.** A loose ground-truth box turns a correct detection into a false positive at high IoU. Mitigations: prefill-then-correct so boxes start tight, `redraw` tracked separately as a quality signal, `ignore` for truncated objects, and periodic re-review of redraw-heavy classes.
5. **Licensing traps.** Ultralytics is already importable from the vision venv through system site-packages; cleanlab and MMYOLO are copyleft; several datasets are research-only. Mitigated by an isolated venv, a licence-hygiene test, manifest flags, and the `--personal-learning` gate.
6. **Baseline churn.** The `len:mtime` model fingerprint already drifted `d4acc862` → `7963897c` on this machine. Part 3.4 stops `models/custom/` from compounding it; converting the whole fingerprint to a cached content sha is a worthwhile separate change.
7. **ONNX Runtime version skew.** The worker dlopens 1.20.0 while the vision venv has 1.27. Pin 1.20.1 in `hushai-train` and forbid opset-21 quantization operators.
8. **Disk.** ~17 GiB free. Either `HUSHAI_TRAIN_DATA` on external storage or strict subsampling; `doctor` refuses to proceed quietly.
9. **Ken-burns eval clips are not the field.** Pans over stills have no motion blur, no rolling shutter, no compression noise. They stay decode/regression guards; thresholds are never tuned on them.
10. **The letterbox divergence** (top-left zero versus centered grey) is a real trap already present in the tree. The model card plus golden vectors make it explicit; the contract test on real frames catches a mismatch.
11. **NNAPI on a 2018-era device** may have no vendor driver at all. Measure with the CPU-disabled flag so an "NNAPI" row means a real accelerator or nothing; XNNPACK is the expected winner.
12. **Thermal throttling** during soaks, on both the phone and the Jetson. Detected via frequency collapse rather than a temperature threshold.
13. **Attribute bloat** on every segment manifest. Capped at 2048 bytes with an explicit truncation flag.
14. **Hardware availability.** Jetson kits are backordered after a mid-2026 price rise; the used-original-kit path is the hedge, and Block 0 has two weeks of hardware-free work by design.
15. **JetPack churn.** Pin 6.2.x for the whole curriculum and record versions in every measurement row; engines are not portable across versions.
16. **x86-only compilers.** Hailo's toolchain and the cleanest Jetson flashing both want x86 Linux; Apple-Silicon emulation is not viable. Budget a cloud VM or skip that branch.
17. **Software H.264 encode** on both candidate boards eats CPU that the detector wants. Mitigations: 720p15, a native-H.264 USB camera, or a board with hardware encode.
18. **The soft pointer dangles** after a partition drop. By design — the denormalized copy is the record.
19. **Re-extraction determinism** across ffmpeg versions: decoding is normative, scaling flags are pinned, PNG bytes are not stable so provenance hashes pixels, and a mismatch warns rather than gates.
20. **Canvas hit-testing** must survive device pixel ratio, letterboxing and resize; mitigated by reusing the existing rect computation and asserting the inverse mapping numerically in an e2e check.
21. **Scope creep** is the largest risk to the whole spec. Every wave has a gate; nothing proceeds on a red one.

---

## Result matrix

| Phase | Check | Result |
|---|---|---|
| 0 | Preconditions: full gate green, migrations applied, PR0 finding recorded | ☐ |
| A | Build + clippy + unit totals (workspace, eval, backend labels, train pytest, generator self-verify) | ☐ |
| B | Labels API curl + snapshot + COCO export loads + audit rows + 401 | ☐ |
| C | D1/D2/D4/D8 gate ×2; 28 existing baselines green; lineage unchanged | ☐ |
| D | Label mode: 5 e2e checks + a 20-frame manual session | ☐ |
| E | Flywheel: 200 frames → export → train ingest → D9; D9 vs D1 `recall@op` recorded | ☐ |
| F | Custom lane: contract test, live rows, D5/D6 staging ×2, off-state bit-identical | ☐ |
| G | Phone: golden vectors ×3 languages, zero encoder impact, `edge_agreement` live, Pareto table | ☐ |
| H | Device: 24 h soak, FP16 + INT8 validated, 3×3×2 measured, throttle point, one harvested loop | ☐ |
| I | Tier-2 `--expect-detections` + `field_gap.jsonl` | ☐ |

---

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| Prefill boxes sit about a third of a second off the picture | PR0 not deployed to the worker, or pre-fix historical rows |
| `POST /v1/labels` returns 404 through the viewer | `/v1/labels` missing from `proxy.rs::is_backend_path` — the request went to hushai-rag |
| Creating a label frame returns 410 | The segment was reclaimed by retention; label from the queue sooner, or take the "hard" persistence escape hatch |
| Every `detections.*` metric classifies as `New` after adding a custom model with the lane off | The `models/custom/` skip is not applied in `fingerprint_models` |
| `map50` regressed but `objects.label_f1` did not | Expected — label F1 cannot see box drift. Look at per-class AP and the PR dump |
| `done` refused with 409 | Unreviewed boxes above the review floor remain; grade them or pass `force` (which is logged) |
| Label e2e checks all SKIP | No vision rows in the window — seed with the `car_object` media via `feed_segments.py` |
| The custom lane loads but writes nothing | Model card input size disagrees with the exported graph; run the contract test |
| Phone logs `edge: disabled` | Asset missing or every execution provider failed; capture continues by design — check the fetch script ran |
| Device engine refuses to load | The model card's runtime does not match the live TensorRT/JetPack version; rebuild with `build_engine.sh` |
| Eval says INCONCLUSIVE after enabling the custom lane | A segment errored; the harness refuses to score a partially-processed window by design |

---

## Suggested PR slicing

**Wave 1 — measure and label (Blocks 0–2).**
1. **PR0 — frame identity.** Verify the offset semantics with the fiducial clip, fix if confirmed, unit test, note in the worker README. Gate-safe: no metric or hash change.
2. **Eval `detections` modality + `gen_det_gt.py` + D1/D2/D4/D8.** Makes Phase C executable under the unchanged lineage.
3. **`hushai-train` skeleton.** Package, `doctor`, dataset schema/manifest/splits/audit, converters, `coco_eval`, `bench`, the contract test against today's RF-DETR export, `provision_train.sh`.
4. **Migration 0032 + worker writes.** DDL, `ObjectWrite.{detector,frame_w,frame_h}`, migrations README row. No consumers yet.
5. **Backend `/v1/labels` core.** Read, upsert, snapshot, audit arm, proxy allowlist, `LABELS_*`. Phase B.
6. **Queue + export + `labels_to_fixture.py`.**
7. **Viewer Label mode + 5 e2e checks.** Phase D.

**Wave 2 — the first custom model (Block 3).**
8. **RF-DETR track + export contract + model card + quantization scaffolding.**
9. **Worker custom lane + card-driven preprocessing + manifest posture + D5/D6 staging.** Phase F.
10. **Wildlife lane wiring** (label set, RAG label awareness, an event/alert example) **+ Tier-2 `--expect-detections` + the field-gap log.** Phase I.

**Wave 3 — own detector and the Mac (Block 4).**
11. **`hushdet` + unit tests + ablation configs.**
12. **Small/far mode:** P2 variant, tiling at inference, D3 promotion, the drone benchmark report.
13. **Quantization tooling + Mac loadtest profiles + the vision lane in the saturation verdict** (`hushai-loadtest/src/saturation.rs:15-39` is audio-only today, so vision-heavy profiles report a verdict blind to the vision knee).

**Wave 4 — the phone (Block 5).**
14. **Golden vectors + three consumers.** Closes `AGENTS.md:655`.
15. **Android edge detector + hints + settings + asset fetch.**
16. **Backend `parse_det` + veto + migration 0033 + worker grading + metrics + dashboard.**
17. **`edge_profile.sh` + `physical_loopback.py --edge`.** Phase G.

**Wave 5 — the device and the depth work (Blocks 6–8).**
18. **`hushai-edge` client skeleton + fake camera + unit tests** (hardware-free; do this while the kit ships).
19. **Jetson provisioning + TensorRT tooling + `profile.py` + soak + `edge_loopback.py` + field presets.** Phase H.
20. **RT-DETR track + the three-way comparison report.**
21. **Robustness suite + noisy-label audit + the smoke/fire lane + its alert rule.**
22. **Distillation / class-incremental / replay.**
23. *(optional)* **Second edge device.**
24. *(optional)* **Labels auth hardening** and *(optional)* **"hard" frame persistence**, the latter only if the queue proves starved.

Each PR updates this spec's result matrix for the phases it makes executable.

---

## Doc-deliverables checklist (same-change rule)

- [ ] `AGENTS.md`: component-map rows for `hushai-train/` and `hushai-edge/`; the Vision subsystem paragraph gains the `detector` tag and a pointer to the label flywheel; the Testing section names the `detections` modality and the `local/` split; Known gaps notes that a camera token is a labels-admin token until PR23, and drops the golden-vector line once PR14 lands.
- [ ] `hushai-backend/migrations/README.md`: rows for 0032 and 0033.
- [ ] `hushai-backend/README.md`: the labels endpoints under the admin/catalog API.
- [ ] `hushai-viewer/README.md`: Label mode in the keyboard-shortcuts and HTTP-API sections.
- [ ] `hushai-worker/README.md`: the PR0 note, `CUSTOM_DET_*`, `models/custom/`, and the model-card contract.
- [ ] `hushai-eval/RECURSIVE_TESTING.md` + `README.md`: the modality, the bands, the `local/` split, the generator.
- [ ] `contracts/cameraToBackendContract.md` §8: the `hint.det.*` family.
- [ ] `CHANGELOG.md`: an entry per landed wave.
- [ ] `local_dev/eval.env`: **no new pins in Wave 1** (deliberately — the lineage must not move); `eval.custom.env.example` added in Wave 2.
- [ ] `docs/feature-parity-roadmap.md` Pillar C: a one-line pointer to this spec.
- [ ] `hushai-worker/.env.example`: the new knobs, plus the four object-lane knobs currently missing from it (`OBJECT_DET_INPUT_SIZE`, `OBJECT_MAX_PER_FRAME`, `OBJECT_MIN_BOX_PX`, `OBJECT_REQUIRED`).

---

## Reading list

**Detection architectures.** DETR (Carion 2020) · Deformable DETR (Zhu 2020) · DINO (Zhang 2022) · LW-DETR (Chen 2024) and the RF-DETR report · RT-DETR (Zhao 2024), RT-DETRv2 (Lv 2024), D-FINE (Peng 2024) · YOLOv3 · YOLOX (Ge 2021) · TOOD (Feng 2021, the assigner) · Generalized Focal Loss (Li 2020, the DFL) · CIoU (Zheng 2020) · RepVGG (Ding 2021) · PP-YOLOE (Xu 2022).
**Small objects and augmentation.** Kisantal 2019 (small-object augmentation) · Ghiasi 2021 (copy-paste) · SAHI (Akyon 2022) · FGD (Yang 2022, feature distillation).
**Data quality.** Northcutt et al. 2021 (confident learning) · the COCO evaluation definition itself, read from the pycocotools source.
**Quantization and edge.** Nagel et al. 2021 (Qualcomm quantization white paper) · Wu et al. 2020 (NVIDIA integer quantization) · Gholami et al. 2021 (survey) · Jacob et al. 2018 (integer-only inference) · the TensorRT developer guide chapters on quantized types and DLA · ONNX Runtime docs on the NNAPI and XNNPACK execution providers and on QDQ static quantization · MIT 6.5940 lectures on quantization and pruning.
**Platform.** Jetson Linux power management (`nvpmodel`, `jetson_clocks`, `tegrastats`) and accelerated GStreamer · Android Camera2 stream configuration and `YUV_420_888` plane semantics · `dumpsys batterystats` and Battery Historian · Netron, for reading any exported graph.

---

## Appendix — optional extensions

**Geospatial / aerial, if it ever matters.** A camera gains a fixed pose (latitude, longitude, height, heading, tilt, field of view) in the devices table; a homography maps image coordinates to a ground plane; detections export as GeoJSON with a per-detection footprint; the viewer gains a map panel. That turns "a box at pixel 412,180" into "an object at these coordinates", which is the remote-sensing framing the job's preferred list mentions. It is a self-contained afternoon once the detector exists, and it is not needed for anything else in this spec.

**Browser inference.** Vendor ONNX Runtime Web under the viewer's existing vendor directory (there is no bundler; that is the established pattern), tap frames from the web-capture controller's MediaStream, run the same letterbox/decode/NMS validated against the same golden vectors, and emit the same `hint.det.*` attrs so the server grader works unchanged on a third source. Expect 80–150 ms at 320 px int8 single-threaded on this machine. Worth two days as a portability proof — one model, three runtimes — and worth skipping entirely if the device track runs long.
