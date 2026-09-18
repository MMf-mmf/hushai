# Vision: image cleanup + license-plate recognition (ALPR)

**Status:** built 2026-06-28. All Rust compiles; 138 unit tests pass; Android built + run on a real
device. Runtime with real ML weights is **operator-gated** (weights are gitignored — see
[Model provisioning](#model-provisioning)).

This document is the technical reference for the work that added (a) a shared **image-cleanup stage**
between detection and recognition for both faces and vehicles, and (b) a greenfield **license-plate
recognition (ALPR) lane**. It assumes familiarity with the existing vision pipeline (AGENTS.md
"Vision pipeline — Phase A" / `hushai-worker/src/vision/`).

---

## 1. Why / what changed

Before this change the crop fed to recognition was a raw warp/clamp — **no context margin, no
zoom/upscale, no restoration, no deskew**. Small/blurry/distant/off-angle faces were *rejected* by
the quality gate, and vehicles were detected but their plates were never read.

The mandate was: *"whenever we identify a person or a car, clean up the image — zoom and crop — to get
the clearest image, then process and categorize it correctly. Follow the most sophisticated industry
standards. Compute is not a factor. We need the best results."*

Three product decisions shaped the implementation:

- **Both tracks, full depth** — worker + DB + backend + RAG + viewer + Android, for faces *and* plates.
- **Best accuracy, any license** — peak-accuracy ONNX weights regardless of license, kept gitignored +
  operator-fetched (the same posture already used for ArcFace `w600k_r50`). Each provisioning script
  prints the weight's license so it can be swapped later.
- **Dev-stage** — minimal real enrolled data, so SCRFD is the *default* detector and restored
  embeddings may mint/fold into centroids, gated only by a fixture-separation validation test (no long
  dual-embed transition window). On a populated catalog, flip `FACE_RESTORED_MAY_MINT=false` first.

---

## 2. Architecture — one shared cleanup core, two lanes

```
                         ┌──────────────────────── vision/enhance.rs (shared) ────────────────────────┐
frame ─ detect ─ bbox ─► │ crop_with_margin → (upscale if tiny) → restore/rectify → deskew → sharpen   │ ─► recognize
                         └────────────────────────────────────────────────────────────────────────────┘
FACE lane:  SCRFD detect → margin-crop → Real-ESRGAN (if tiny) → GFPGAN restore → align_crop(112²) → ArcFace + flip-TTA → match/mint
PLATE lane: RF-DETR car  → vehicle ROI → plate detect (bbox/4-corner) → homography rectify → Real-ESRGAN → CLAHE+unsharp → OCR → vote → string match
```

Everything new is **optional + non-fatal**: a missing model self-disables only its step; the required
face lane (detector + ArcFace) always runs, and audio is never affected. This mirrors the existing
object lane's resilience contract.

All new ONNX models load through the existing `vision/model.rs::load_session` → the **same single ORT
1.20 dylib** via `ORT_DYLIB_PATH`. No new ORT linkage; the sherpa-1.17.1 coexistence story is
unchanged (see AGENTS.md "vision ONNX runtime"). Launch the worker with
`DYLD_FALLBACK_LIBRARY_PATH=target/debug/deps` as before.

---

## 3. Module / file map

### New worker modules (`hushai-worker/src/vision/`)
| File | Purpose |
|---|---|
| `enhance.rs` | The shared cleanup core: `crop_with_margin`, `resize_rgb` (Lanczos3), `unsharp_mask`, `clahe_gray` (tiled CLAHE), `deskew`, `homography_warp` (4-point DLT perspective rectify), `bilinear_sample`; ONNX wrappers `Upscaler` (Real-ESRGAN) + `FaceRestorer` (GFPGAN/CodeFormer); enums `DetectorKind`, `RestorerKind`. |
| `geom.rs` | Shared `iou` + generic `nms_by<T>` (lifted out of `detect.rs`). |
| `detect_scrfd.rs` | `ScrfdDetector` (SCRFD-10GF), the default face detector; implements `detect::FaceDetect`. |
| `plates/mod.rs` | ALPR lane module root + `is_vehicle(label)`. |
| `plates/detect.rs` | `PlateDetector` — defensive YOLO bbox/4-corner decode inside a vehicle ROI. |
| `plates/rectify.rs` | `rectify` (homography deskew to 256×64) + `enhance_plate` (SR + CLAHE + unsharp). |
| `plates/ocr.rs` | `PlateOcr` — defensive ONNX recognizer (NCHW/NHWC + CTC greedy decode) → `PlateRead`. |
| `plates/normalize.rs` | Pure: `normalize`, `fold_confusables`, `edit_distance`, per-position `vote`, `assess_quality`. |
| `plates/plate_match.rs` | Advisory-locked, idempotent **match-or-mint by normalized string** into `license_plates`/`plate_detections`. |

### Changed worker files
| File | Change |
|---|---|
| `vision/detect.rs` | Added `pub trait FaceDetect`; `FaceDetector` (YuNet) now `impl FaceDetect`; NMS/IoU moved to `geom.rs`. |
| `vision/face_embed.rs` | `FaceEmbedder` gained `flip_tta` + `embed_aligned`; added `Pose`, `pose_from_landmarks`, `is_frontal`, `assess_quality_parts`; bilinear now `enhance::bilinear_sample`. |
| `vision/face_match.rs` | `FaceWrite` gained `restored/yaw/pitch/quality_score/is_best_shot/crop_uri`; `FaceMatchConfig.restored_may_mint`; INSERT writes the new columns; mint-guard honors `may_fold`. |
| `vision/write.rs` | The whole cleanup cascade (`enhance_and_embed`, `try_restore`), face best-shot + `persist_face_crops`, RF-DETR fan-out, the plate lane (`process_plate_lane`, `cluster_and_vote_plates`, `persist_plate_crops`), and the unified write tx (faces + objects + plates). `VisionModels` gained `restorer/upscaler/plate_detector/plate_ocr` and `detector` is now `Arc<dyn FaceDetect>`. |
| `lib.rs` | `build_face_detector` (kind selection + fallback), restorer/upscaler/plate-model loading in `build_vision_models`, `load_plate_charset`, `ensure_plate_detection_partitions(3)` on startup. |
| `config.rs` | All new `FACE_*` + `PLATE_*` knobs + `plate_gates()`/`plate_match_cfg()` bundles. |
| `tests/vision_pipeline.rs` | `inspect_enhance_model_io_shapes`, `inspect_plate_model_io_shapes`, `restored_low_quality_recovers_identity`; `FaceDetect` import. |

### DB, backend, RAG, viewer, Android, provisioning
| Area | Files |
|---|---|
| Migrations | `hushai-backend/migrations/0012_face_crops.sql`, `0013_license_plates.sql` |
| Backend API | `hushai-backend/src/plates.rs` (new), `routes.rs`, `lib.rs`; `persons.rs::sample_face` (prefers cleaned crop; `extract_jpeg`/`parse_bbox` made `pub(crate)`) |
| RAG | `hushai-rag/src/plates.rs` (new), `retrieve.rs` (`list_by_plate`), `agents.rs` (`AgentKind::Plates`), `routes.rs` (`plates_query`), `llm.rs` (`answer_plates`), `chat.rs`, `config.rs`, `lib.rs` |
| Viewer | `src/proxy.rs`, `src/detections.rs` (`fetch_plates`), `ui/js/api.js`, `ui/js/detections.js`, `ui/js/settings/plates.js` (new), `ui/index.html` (🚗 button + modal) |
| Android | `net/PlatesClient.kt` (new), `ui/PlatesScreen.kt` (new), `ui/CaptureScreen.kt` (nav card), `MainActivity.kt` (`Screen.Plates`), `test/.../PlatesClientTest.kt` (new) |
| Provisioning | `local_dev/fetch_scrfd.sh`, `export_gfpgan.py`, `fetch_realesrgan.sh`, `fetch_plate_detector.sh`, `export_plate_ocr.py` |

---

## 4. Face track

### 4.1 Detector selection (`FaceDetect` trait)
`VisionModels.detector` is `Arc<dyn FaceDetect>`. `build_face_detector` reads `FACE_DETECTOR_KIND`
(default `scrfd`) and **falls back** to whichever model is provisioned: if SCRFD weights are missing
it logs a warning and loads YuNet, and vice-versa. So a box without SCRFD weights still gets identity.

**SCRFD decode** (`detect_scrfd.rs`): input 640² RGB, `(x−127.5)/128`, black-padded letterbox. Outputs
are grouped **by trailing dim** (1=score, 4=bbox, 10=kps) and each group ordered by anchor count
(stride 8 has the most → `[s8,s16,s32]`), so the decode is robust to export tensor names/order.
Anchors-per-cell is inferred from the score length. bbox is distance-decoded from the anchor center;
landmark order matches the ArcFace template convention (index 0 = image-left eye), so `align_crop`
works for both SCRFD and YuNet with no landmark swap.

### 4.2 The cleanup cascade (`write.rs::enhance_and_embed`)
Per detected face, in `spawn_blocking`:

1. **Hard reject** if `det_score < FACE_HARD_MIN_DET_SCORE` or `min_side < FACE_HARD_MIN_PX` (no row).
2. Compute pose (landmark proxy), raw aligned crop, raw sharpness, raw quality.
3. **Already-clean (raw quality == Mint)** → embed the raw aligned crop, `restored=false`. **This path
   is byte-for-byte the legacy behavior, so clean faces never move in ArcFace space.**
4. **Recoverable** (not clean, and `sharpness < FACE_RESTORE_MAX_SHARPNESS` or
   `min_side < FACE_RESTORE_MIN_PX`) **and a restorer is loaded** → `try_restore`:
   - `crop_with_margin(FACE_CROP_MARGIN_FRAC)` carrying landmarks into crop-local coords;
   - if `min_side < FACE_UPSCALE_MIN_PX` and an upscaler is loaded, Real-ESRGAN upscale (landmarks
     scaled by the actual output/input ratio);
   - `FaceRestorer.restore` → 512² restored image; landmarks scaled to 512²;
   - `align_crop` on the **restored** pixels → 112² → `embed_aligned`, `restored=true`.
   - **Usability** is judged on the ORIGINAL size (restoration can't invent true resolution): usable iff
     `det_score ≥ FACE_MIN_DET_SCORE && original min_side ≥ FACE_HARD_MIN_PX && restored_sharpness ≥
     FACE_MIN_SHARPNESS`. Mint only when the face was genuinely large + confident + sharp-after-restore.
5. **Fallback** (no restorer, or restoration didn't lift above the gate) → embed the raw crop if it at
   least `AttachOnly`s, else drop.

Alignment always runs **last, on restored pixels**, so ArcFace input geometry is unchanged — only pixel
quality improves. Restoration failures are caught and fall back to the raw crop (non-fatal).

### 4.3 Pose / frontality, flip-TTA
- `pose_from_landmarks` is a closed-form proxy (roll from the eye line; yaw from nose horizontal offset
  between the eyes; pitch from nose vertical position between eye and mouth lines). Coarse but enough to
  **gate minting**: `downgrade_for_pose` turns a `Mint` into `AttachOnly` when `|yaw| > FACE_MINT_MAX_YAW_DEG`
  or `|pitch| > FACE_MINT_MAX_PITCH_DEG` (a profile face minting a "new person" is a classic over-split bug).
- **Flip-TTA** (`embed_aligned`, default on): embed the aligned 112² crop and its horizontal mirror,
  sum, L2-renormalize — the InsightFace-standard accuracy win.

### 4.4 Best-shot + persisted cleaned crop
- `quality_score = det_score × frontality × size_term × sharp_term` per face. The highest per segment is
  tagged `is_best_shot`.
- If `FACE_PERSIST_CROP`, the cleaned best-shot/each crop is written to
  `<BLOB_DIR>/face_crops/<segment_id>_<i>.jpg` (deterministic name → reprocess-idempotent), and
  `crop_uri` is stored on the row.
- Backend `sample_face` now selects the best row (`is_best_shot DESC, quality_score DESC, det_score DESC`)
  and **prefers `crop_uri`** (canonicalized + required under `blob_root`), falling back to the old ffmpeg
  seek+crop when absent. The viewer People modal and Android People screen therefore show the *restored*
  thumbnail with no client change.

### 4.5 Embedding-space safety (the one hard invariant)
Generative restoration + flip-TTA can move ArcFace embeddings. Protections, in order of strength:
1. Clean faces skip restoration entirely (legacy space preserved).
2. `FACE_RESTORED_MAY_MINT=false` makes a restored face match/attach but never mint or fold into a
   centroid (treated as `marginal` for centroid math). Dev-stage default is `true`; **set it false on a
   populated catalog** until calibration proves space-compatibility.
3. The gate test `restored_low_quality_recovers_identity` (below) must pass before trusting the path.

---

## 5. ALPR track

### 5.1 RF-DETR fan-out + per-frame flow (`write.rs`)
RF-DETR runs **once** per frame and fans out to (a) the CLIP object lane and (b) the plate lane. The
plate lane depends on the RF-DETR **detector** being loaded, *not* on CLIP — so plates can run without
provisioning the object/CLIP lane.

`process_plate_lane` (per frame): for each vehicle ROI (car/truck/bus/motorcycle; or the whole frame if
`PLATE_DETECT_WHOLE_FRAME` and no vehicle) → `crop_with_margin(PLATE_VEHICLE_ROI_MARGIN)` → `PlateDetector.detect`
→ map ROI-local coords back to original-frame pixels → `rectify::rectify` (homography to 256×64 from 4
corners, else bbox crop+resize) → `rectify::enhance_plate` (SR if small + CLAHE + unsharp) → `PlateOcr.read`
→ a `PlateCand` (read + bboxes + corners + crop + timestamps).

After the frame loop, `cluster_and_vote_plates` groups candidates by plate-bbox IoU (>0.3) **across
frames** and `normalize::vote`s each cluster into one confident read (per-position confidence-weighted
majority, dominant-length first). Each surviving cluster → a `PlateWrite` with the best member's
bboxes/corners/crop. Best-shot tagging + `persist_plate_crops` (`<BLOB_DIR>/plate_crops/`) mirror faces.

### 5.2 Matching is by string, not embedding (`plate_match.rs`)
A plate's identity **is** its text, so the catalog is matched by the confusable-folded normalized
string — **exact** (`plate_text_norm` unique index) then **fuzzy** (pg_trgm `%` candidates filtered by
Rust `edit_distance ≤ PLATE_MAX_EDIT_DISTANCE` and trigram `similarity ≥ PLATE_FUZZY_MIN_SIMILARITY`).
Only a `Mint`-quality read (high OCR confidence, length ≥ `PLATE_MIN_LEN`) may mint a new plate
(`ON CONFLICT (plate_text_norm)` makes minting race-safe). The canonical `plate_text` self-heals to the
best clean read (the string analogue of the self-healing centroid). Same advisory-lock + idempotent
delete-by-segment + intra-tx-visibility machinery as `face_match.rs`, with a distinct lock key
(`0x6873_706c_6174` = "hsplat").

### 5.3 Defensive decoders (validate at provisioning)
- **`plates/detect.rs`**: takes the largest 2-D output `[C,N]`/`[N,C]`, channels 0..4 = `cx,cy,w,h`
  (model-input px), channel 4 = confidence; with ≥8 trailing channels, the next four `(x,y[,vis])`
  groups are plate corners. Bbox-only models still work (axis-aligned crop, no deskew).
- **`plates/ocr.rs`**: inspects the declared input dims to pick layout (NCHW vs NHWC, gray vs RGB),
  greedy per-timestep argmax with CTC blank/duplicate collapse, maps indices through the charset sidecar.

Both are validated by the model-gated `inspect_plate_model_io_shapes` test against the real export.

---

## 6. Database schema

### `0012_face_crops.sql` (additive, nullable → partition-safe)
Adds to `person_segments`: `crop_uri text`, `is_best_shot boolean default false`,
`restored boolean default false`, `yaw real`, `pitch real`, `quality_score real`, plus a partial index
`(person_id, quality_score DESC) WHERE person_id IS NOT NULL AND crop_uri IS NOT NULL`.

### `0013_license_plates.sql`
Requires `pg_trgm` + `fuzzystrmatch` (created by the migration). Two tables:

- **`license_plates`** (catalog): `plate_id uuid PK, plate_text, plate_text_norm, region_hint, n_samples,
  n_sightings, display_name, first_seen_unix_nanos, last_seen_unix_nanos, first_seen_device_id, created_at,
  updated_at`. Indexes: **UNIQUE** `plate_text_norm` (race-safe mint key + exact match), `lower(display_name)`,
  GIN trigram on `plate_text_norm` (fuzzy).
- **`plate_detections`** (monthly RANGE-partitioned by `created_at`, mirrors `person_segments`):
  `id bigserial, segment_id uuid, device_id, plate_id uuid (nullable), start/end/frame_offset_nanos,
  vehicle_bbox jsonb, vehicle_label, plate_bbox jsonb, plate_corners jsonb, ocr_text, ocr_text_norm,
  ocr_confidence real, char_confidences jsonb, det_score real, quality text, embedding vector(512)
  (nullable, **no HNSW** — reserved for a future visual-similarity path), crop_uri, is_best_shot,
  created_at`. Indexes: `segment_id`, `(plate_id,start_unix_nanos)`, `(ocr_text_norm,start_unix_nanos)`.
  Partition helpers `ensure_plate_detection_partitions(n)` / `drop_plate_detection_partitions_before(cutoff)`
  + a DEFAULT partition (clones of the 0009 helpers). The worker calls `ensure_plate_detection_partitions(3)`
  on startup.

---

## 7. Configuration reference (new env vars)

All in `hushai-worker/.env.example` + `config.rs`. Defaults keep every new lane inert until provisioned.

### Image cleanup / face restoration
| Var | Default | Meaning |
|---|---|---|
| `FACE_DETECTOR_KIND` | `scrfd` | `scrfd` (default) or `yunet`; falls back to whichever is provisioned |
| `FACE_SCRFD_MODEL_PATH` | `./models/scrfd_10g_bnkps.onnx` | SCRFD weights |
| `FACE_EMBED_FLIP_TTA` | `true` | average a crop's embedding with its mirror |
| `FACE_CROP_MARGIN_FRAC` | `0.35` | context margin before the restorer |
| `FACE_RESTORE_MODEL_PATH` | `./models/gfpgan_v1.4.onnx` | restorer weights (empty/missing ⇒ sub-lane off) |
| `FACE_RESTORE_KIND` | `gfpgan` | `gfpgan` or `codeformer` |
| `FACE_RESTORE_CODEFORMER_W` | `0.6` | CodeFormer fidelity weight (ignored by GFPGAN) |
| `FACE_RESTORE_MAX_SHARPNESS` | `45.0` | restore only crops blurrier than this |
| `FACE_RESTORE_MIN_PX` | `80` | …or smaller (shorter side px) than this |
| `FACE_HARD_MIN_PX` | `16` | below this, even restoration can't help → drop |
| `FACE_HARD_MIN_DET_SCORE` | `0.3` | absolute detector-confidence floor |
| `FACE_UPSCALE_MODEL_PATH` | `./models/realesrgan_x4plus.onnx` | optional super-res (shared with plates) |
| `FACE_UPSCALE_MIN_PX` | `48` | super-resolve a crop smaller than this |
| `FACE_MINT_MAX_YAW_DEG` | `35.0` | beyond this yaw, a face may match but not mint |
| `FACE_MINT_MAX_PITCH_DEG` | `30.0` | as above, pitch |
| `FACE_RESTORED_MAY_MINT` | `true` | restored faces may mint/fold (set `false` on a populated catalog) |
| `FACE_PERSIST_CROP` | `true` | persist the cleaned crop for the UI thumbnail |
| `FACE_BEST_SHOT_ENABLED` | `true` | tag the best face per segment |
| `BLOB_DIR` | `./blobs` | shared with the backend; face/plate crops live under it |

### License plates (ALPR)
| Var | Default | Meaning |
|---|---|---|
| `PLATE_ENABLED` | `true` | master switch (still self-disables if models missing) |
| `PLATE_DETECT_MODEL_PATH` | `./models/lp_detector.onnx` | plate detector |
| `PLATE_OCR_MODEL_PATH` | `./models/lp_ocr_cct.onnx` | plate OCR |
| `PLATE_OCR_CHARSET_PATH` | `./models/lp_ocr_charset.json` | ordered class→char map (array of 1-char strings) |
| `PLATE_DETECT_INPUT_SIZE` | `640` | detector square input |
| `PLATE_MIN_DET_SCORE` | `0.35` | detector confidence floor |
| `PLATE_MIN_PX` | `16.0` | min plate bbox side (orig-frame px) |
| `PLATE_MIN_OCR_CONF` | `0.55` | min mean per-char OCR confidence to keep a read |
| `PLATE_MINT_MIN_OCR_CONF` | `0.80` | higher bar to mint a new catalog plate |
| `PLATE_MIN_LEN` | `4` | reject reads shorter than this |
| `PLATE_MAX_EDIT_DISTANCE` | `1` | fuzzy-match budget to an existing plate |
| `PLATE_FUZZY_MIN_SIMILARITY` | `0.7` | trigram similarity floor for a fuzzy candidate |
| `PLATE_VEHICLE_ROI_MARGIN` | `0.10` | expand the vehicle bbox before cropping the ROI |
| `PLATE_SR_MIN_SIDE_PX` | `64.0` | super-resolve a rectified plate smaller than this |
| `PLATE_DETECT_WHOLE_FRAME` | `false` | also scan the whole frame when no vehicle is present |
| `PLATE_REQUIRED` | `false` | `true` ⇒ a missing plate model disables the WHOLE vision subsystem |

`PERSON_SIGHTING_GAP_SECONDS` / `PLATE_SIGHTING_GAP_SECONDS` (default 60) sessionize detections into the
"N sightings" counts the backend list endpoints return.

---

## 8. Model provisioning

All weights are **gitignored** under `models/` and operator-fetched. Each script prints a sha256 (pin
once stable) and the weight's license. After provisioning, run the matching decode-validation test.

| Script | Output | Model / license | Validate with |
|---|---|---|---|
| `fetch_scrfd.sh` | `scrfd_10g_bnkps.onnx` | SCRFD-10GF (InsightFace buffalo_l `det_10g`); research/non-commercial | `inspect_enhance_model_io_shapes` |
| `export_gfpgan.py` | `gfpgan_v1.4.onnx` (`--model codeformer` → `codeformer.onnx`) | GFPGANv1.4 / CodeFormer; research | `inspect_enhance_model_io_shapes`, `restored_low_quality_recovers_identity` |
| `fetch_realesrgan.sh` | `realesrgan_x4plus.onnx` | Real-ESRGAN x4plus; BSD-3 (set `REALESRGAN_ONNX_URL`) | `inspect_enhance_model_io_shapes` |
| `fetch_plate_detector.sh` | `lp_detector.onnx` | a YOLO LP detector (bbox or 4-keypoint pose); prefer Apache/MIT — Ultralytics weights are AGPL (set `PLATE_DETECTOR_ONNX_URL` or `yolo export`) | `inspect_plate_model_io_shapes` |
| `export_plate_ocr.py` | `lp_ocr_cct.onnx` + `lp_ocr_charset.json` | fast-plate-ocr CCT; MIT | `inspect_plate_model_io_shapes` |

The ONNX I/O contracts the decoders expect (validate against the real export):
- **SCRFD**: 640² RGB NCHW `(x−127.5)/128`; 9 outputs (score/bbox/kps × strides 8/16/32).
- **Real-ESRGAN** (`Upscaler`): RGB NCHW `[0,1]` in, RGB NCHW `[0,1]` out (scale inferred).
- **GFPGAN/CodeFormer** (`FaceRestorer`): 512² RGB NCHW `[-1,1]` in/out; CodeFormer may take a 2nd scalar
  fidelity input (passed when the graph has 2 inputs).
- **Plate detector**: YOLO `[C,N]`/`[N,C]` — box(4)+conf(1)[+corners].
- **Plate OCR**: NCHW/NHWC image in; `[T,C]`/`[C,T]` per-timestep class scores out; charset indexed by class.

---

## 9. API / RAG / UI surfaces

### Backend (`hushai-backend/src/plates.rs`, routes in `routes.rs`)
- `GET  /v1/plates` — catalog with sessionized `n_sightings` + up to 3 recent sighting times (`PlateSummary`).
- `GET  /v1/plates/search?q=` — normalize + exact/trigram search (the "find plate X" lookup).
- `PATCH /v1/plates/{id}` — set `display_name`.
- `POST /v1/plates/{id}/merge` — fold an OCR-split duplicate into another (`{"into": "<uuid>"}`).
- `GET  /v1/plates/{id}/sample-crop` — best rectified-plate JPEG (prefers stored `crop_uri`, else ffmpeg crop).

### RAG (`hushai-rag/`)
- `AgentKind::Plates`, agent id **`plates`** (`agents.rs`), preamble `PREAMBLE_PLATES` (grounded-only,
  time-first, refers to a plate by its string/label, never leaks ids).
- `routes.rs::plates_query` — explicit `plate_id`/`plate_text` filter → `retrieve::list_by_plate`; else
  resolve plate-shaped tokens in the query (`plates::resolve_plates_in_text`, len ≥ 4 with ≥ 1 digit) →
  `list_by_plate`; answer via `llm::answer_plates`. This is the path for *"when did I see plate ABC123"*.
- The `plates` path matches by **normalized string** (no owner co-occurrence fallback, unlike People).

### Viewer
- `proxy.rs` dispatches `/v1/plates*` to the backend.
- `detections.rs::fetch_plates` + the overlay (`ui/js/detections.js`) draw plate boxes in **amber**
  (`#ffd24a`) with the plate string as the label.
- `ui/js/settings/plates.js` — the **🚗 Plates** modal (search box + named/unidentified sections, crop
  thumbnail, name + merge); wired via `ui/index.html` (`btnPlates`, `platesModal`).

### Android
- `net/PlatesClient.kt` (list/search/rename/merge/sample-crop), `ui/PlatesScreen.kt`, a nav card in
  `CaptureScreen.kt`, `Screen.Plates` in `MainActivity.kt`, `PlatesClientTest.kt`.

---

## 10. Verification

- **Compiles:** `cargo check --workspace --tests` — clean.
- **Unit tests:** `cargo test --workspace --lib` — 138 pass (worker 55, RAG 52, backend 17, viewer 14),
  including the new `enhance`/`geom`/`plates::normalize`/`plates::rectify` tests and the bumped RAG agent
  registry test.
- **Android:** built with Homebrew `openjdk@17` + SDK `adb` + the Gradle 8.9 wrapper; `PlatesClientTest`
  passes; `:app:installDebug` onto a real device; the Plates screen renders and calls the backend.
  ```
  cd hushai-android
  export JAVA_HOME=/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home
  export ANDROID_HOME=$HOME/Library/Android/sdk
  export PATH="$JAVA_HOME/bin:$ANDROID_HOME/platform-tools:$PATH"
  ./gradlew :app:installDebug
  ```
- **Model-gated (SKIP until weights provisioned):**
  ```
  # decode-validation:
  cargo test -p hushai-worker --test vision_pipeline inspect_enhance_model_io_shapes -- --nocapture
  cargo test -p hushai-worker --test vision_pipeline inspect_plate_model_io_shapes -- --nocapture
  # recover-then-embed correctness gate (needs FACE_TEST_DIR with obama.jpg + arcface + gfpgan):
  FACE_TEST_DIR=/path cargo test -p hushai-worker --test vision_pipeline restored_low_quality_recovers_identity -- --nocapture
  ```

### Full end-to-end (operator)
1. Provision the ORT dylib + all vision models (the `local_dev/` scripts above + the existing
   `fetch_onnxruntime.sh` / face / RF-DETR / CLIP scripts).
2. Run the decode-validation tests; fix any decode mismatch they surface (that's their job).
3. Apply migrations (start `hushai-backend`), run the worker with `DYLD_FALLBACK_LIBRARY_PATH=target/debug/deps`,
   feed a clip with faces and a vehicle+plate — `./local_dev/build_demo.sh` builds both from
   public-domain stills, or replay your own file with `local_dev/feed_segments.py --video`.
4. Confirm: faces that previously produced zero `person_segments` now attribute + `GET /v1/persons/{id}/sample-face`
   returns a cleaned crop; `GET /v1/plates` lists the plate; the viewer overlay draws faces+plates; the
   RAG query *"when did I see plate <X>"* returns the sighting time.

---

## 11. Known limitations / future work

- **Decoders are defensive, not export-pinned.** SCRFD, the plate detector, and the plate OCR decode by
  shape/order heuristics; the `inspect_*` tests exist to catch a mismatch at provisioning. If you swap a
  model, re-run them.
- **Thresholds are uncalibrated guesses.** Restoration triggers, mint gates, plate OCR/fuzzy gates — tune
  on real footage. The recover-then-embed gate test is the safety net for embedding drift.
- **Plate clustering is bbox-IoU across frames (>0.3).** Fine for ~2s segments / slow scenes; fast-moving
  plates may not cluster — a tracker would improve temporal voting.
- **Multi-line plates** are handled generically (aspect-ratio split is a TODO in `rectify`); region/country
  formats are intentionally not special-cased (`fold_confusables` default map is region-agnostic).
- **Plate visual-similarity** (`plate_detections.embedding`) column exists but is unused (no HNSW) — a hook
  for "find visually-similar unreadable plates" later.
- **Crop sharing** assumes the worker and backend resolve `BLOB_DIR` to the same filesystem location
  (local-first; both run from the repo root). If they don't, the backend's path guard fails closed and
  `sample_face`/`sample-crop` fall back to ffmpeg re-cropping.
- **Licensing:** several best-accuracy weights are research/non-commercial or AGPL. Fine for personal/research;
  for a shipped product swap to permissive weights (fast-plate-ocr MIT, Real-ESRGAN BSD, a permissive plate
  detector) — the provisioning scripts print each license to make this auditable.
