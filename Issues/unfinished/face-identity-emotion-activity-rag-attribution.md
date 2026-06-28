**Title:** `[hushai-worker + hushai-backend + hushai-rag + hushai-android] - Face/person identity (anonymous-now, named-later, cross-device) + per-face emotion + activity on the video frames, surfaced in RAG attribution`

- **Description**:

  This is the **visual sibling** of `Issues/speaker-identity-sentiment-rag-attribution.md`. That
  ticket discovers, clusters, and labels every **voice** the system hears (anonymous voice ID now,
  human-named later, cross-device) and surfaces it in RAG ("what did Bob say"). This ticket does the
  exact same thing for every **face** the cameras see: build a catalogue of people, identify who was
  picked up in frame, and let a human attach a name to an unknown person so the system thereafter
  knows who it is — plus two extra per-person signals the user asked for: **emotion** (were they
  happy / sad) and **activity** (what were they doing).

  Three derived signals, in priority order (the first is the headline):

  1. **Person identity** (`person_id`) — the most important deliverable. Anonymous now (a minted
     UUID per distinct face), human-named later. The system never has names up front: it **mints an
     anonymous face ID per distinct face it sees**, accumulates a face template (centroid) for it,
     and a human later attaches a name ("this face is Bob") via the app; from then on every new
     sighting of that face is auto-attributed to Bob, **across all cameras/devices**. This is the
     visual analogue of the voice-labeling flow — *"label them in our system once, and the system
     should know who the person is."*
  2. **Emotion / expression** per detected face (`happy | sad | neutral | …`) — coarse facial
     expression, derived per face from the frame.
  3. **Activity** per detected person (`action`, e.g. "sitting at a desk", "walking", "on the phone")
     plus a segment-level **scene** caption / vibe — "what are they doing", derived from the frame.

  **Relationship to the speaker ticket (shared lessons, no hard code overlap):** that ticket and this
  one share three hard-won patterns — (a) the *anonymous-now / named-later / cross-device global
  identity* model (online running-mean centroid match-or-mint, serialized by a global advisory lock,
  idempotent per segment); (b) the *decouple analysis granularity from the ~2s transport granularity*
  lesson (see `hushai-conversation-analysis-roadmap`); (c) the *`ort`/ONNX-Runtime native-lib
  provisioning* story. They can land **independently** in either order. If the speaker ticket lands
  first it establishes the `ort` dependency + `libonnxruntime` provisioning and the `/v1/speakers`
  endpoint shape, which this ticket then mirrors; if not, this ticket carries that provisioning
  itself. **State the dependency at PR time and reuse, don't duplicate.**

  ---

  ### The pipeline today (grounded) — there is NO frame/vision processing anywhere

  `hushai-worker` is **entirely audio-driven**. `process.rs:process_segment` is
  `media::load_segment` → `media::extract_pcm` → `transcriber.transcribe` → `chunk` → `embed` →
  `write_transcript`. Critically, `media::extract_pcm` (`media.rs:97-101`) runs ffmpeg with **`-vn`
  (drop video)** and only ever decodes 16 kHz mono PCM. **No frame is ever decoded; no pixel is ever
  looked at.** A new **vision processing path** must be added beside the ASR path — this is genuine
  greenfield, unlike sentiment (which reused an existing column).

  Usefully, the schema was **pre-designed for vision** (`0001_init.sql:74-118`, with the explicit
  comment "vector(1024) for text/summary, **vector(512) for face/object**"):

  - **`scene_objects`** (`0001_init.sql:108-118`) — empty today — already has exactly the per-person
    columns we need: `segment_id`, **`person_id text`**, `action text`, `bbox jsonb`,
    **`embedding vector(512)`**, `embedding_model`, `embedding_dim`. This is the per-face observation
    table; `person_id` is the denormalized identity stamp and `embedding vector(512)` is the
    face-template column (512-dim is the ArcFace/InsightFace face-embedding size — the schema author
    sized it for exactly this).
  - **`video_events`** (`0001_init.sql:97-106`) — empty today — has `segment_id`, `scene_label`,
    `vibe`, `disposition`, `embedding vector(1024)`: the segment-level scene caption + mood, with a
    1024-dim text-embedding slot so a caption can be embedded into the **same space as
    `transcript_sentences`** and retrieved by the existing RAG path.
  - Both already CASCADE on segment delete (`0004_segment_child_cascade.sql:21-29`).

  These tables are still in the **phase-1 single-table shape**, i.e. they have the same scalability
  gaps that `0003_scalability.sql` fixed for `transcript_sentences`: **no `device_id` denormalization,
  no `created_at`/time columns, no partitioning, no HNSW index, and no `segment_id` index** for the
  idempotent delete-by-segment rewrite. Since both tables are **empty**, a recreate into the
  production-shaped (partitioned + indexed + denormalized) form is the cheapest path — exactly the
  `0003` rationale ("the table is tiny / regenerable, so a recreate is the cheapest path").

  ### What video is even available to process

  Per the contract (`contracts/cameraToBackendContract.md`) + proto
  (`hushai-backend/proto/hushai/v1/segment.proto:4`): `MediaType { UNSPECIFIED=0; AUDIO=1; VIDEO=2;
  MUXED=3 }`. The Android client emits **two** streams — `cam0-video` (VIDEO-only) and `cam0-audio`
  (AUDIO-only); the `feed_segments.py` reference client emits `cam0-muxed` (MUXED). So the vision path
  must process **`media_type IN (VIDEO, MUXED)`** and the ASR path stays on `AUDIO`/`MUXED`. (Today
  VIDEO-only `cam0-video` segments actually sit as `error` in `segment_transcription_status` because
  the audio path's `-vn` ffmpeg finds no audio stream — AGENTS.md "Fast-follows". This ticket's vision
  path is what finally gives those segments a reason to exist; resolve the media-type routing as part
  of the work so video segments stop erroring.) Video segments start on a keyframe (contract §"Ordering
  metadata"); `container` is `mp4` (self-contained, Android) or `fmp4` (needs init prepended) and is
  decoded exactly as `media.rs:extract_pcm` already keys off `container`.

  ### The "2-second reality" — much milder for faces than for voices

  The same fixed 2s wall-clock segmentation that hurts speaker ECAPA embeddings (`< 1s of voiced
  speech` is common) is **far less of a problem for faces**: a **single sharp, frontal, large-enough
  frame is sufficient** for detection + a good face embedding. So the vision path samples a small
  number of frames per segment (recommend 1–3 via ffmpeg `-vf fps=…` or keyframe extraction) rather
  than needing windowing for *identity*. The honest v1 claim mirrors the speaker ticket's honesty: a
  **labeled catalogue of recurring, reasonably-frontal faces for "who have we seen / when did we see
  Bob / was Bob happy", NOT forensic face identification.** The accuracy killers are the opposite of
  audio's: tiny / blurry / motion-smeared / back-of-head / profile / poorly-lit faces. Most always-on
  indoor frames will contain **no usable face at all** — that is **expected** (no observation written),
  not a failure.

  Mandatory quality gates before embedding a face (the visual analogue of the speaker VAD/min-speech
  gate — embedding a bad crop is the dominant accuracy killer):

  - **Detection-confidence gate:** only embed detections above `FACE_MIN_DET_SCORE`.
  - **Min-face-size gate:** skip faces whose bbox is smaller than `FACE_MIN_PX` (a 20px face embeds to
    noise).
  - **Blur / sharpness gate:** skip crops below a variance-of-Laplacian sharpness floor
    (`FACE_MIN_SHARPNESS`).
  - **Pose gate (best-effort):** down-rank / skip extreme-yaw (profile) and back-of-head detections;
    a profile embedding poisons a frontal centroid. If the detector emits 5-point landmarks, **align
    the crop** (similarity transform to canonical landmarks) before embedding — ArcFace accuracy
    depends heavily on alignment.

  A skipped face writes **no** `scene_objects` row and folds **no** centroid (a confidently-wrong
  attribution and a blended/garbage centroid are both worse than nothing — same principle as the
  speaker multi-speaker refusal).

  ### Face-identity strategy: online global centroid match, serialized, idempotent (mirrors speakers)

  A new `hushai-worker/src/vision/` module mirrors the ASR layer at the inference boundary:

  - **`detect.rs`** — face **detection** + 5-point landmarks. Recommend **SCRFD** (or RetinaFace)
    exported to ONNX, run via the `ort` crate on `tokio::task::spawn_blocking` with `Arc<Session>`,
    exactly like `asr.rs:Transcriber` / the speaker ticket's `SpeakerEmbedder`. Returns
    `Vec<{bbox, score, landmarks}>` per frame.
  - **`face_embed.rs`** — face **embedding**. Recommend **ArcFace / InsightFace** (`w600k_r50` or
    `buffalo_l`) exported to ONNX → **512-d** L2-normalized vector (matches
    `scene_objects.embedding vector(512)` exactly). Align the crop to canonical landmarks first.
  - **`frames.rs`** — sample N frames from the segment's blob via ffmpeg (reconstruct the decodable
    fragment exactly as `media.rs` does — key off `container`, never the source), decode to RGB.

  Identity assignment is **online incremental, GLOBAL (cross-device), serialized, idempotent**, run
  inside the segment write transaction — *identical in shape to the speaker matcher*:

  - **Global serialization:** `pg_advisory_xact_lock(<constant person-space key>)` at the top of the
    identity step. Because identity is cross-device there is one global person space, so a single
    global lock serializes match-or-mint and fixes the cold-start duplicate-mint race under
    `WORKER_CONCURRENCY=2`. (Use a **different** lock key from the speaker space.) Same documented
    tradeoff: a global lock serializes all face assignment — fine at single-user/local volume, a
    scaling bottleneck to revisit.
  - **Match or mint:** `SELECT person_id, centroid, n_samples FROM persons`, cosine-compare each
    L2-normalized face embedding to every centroid. Best **distance ≤ `FACE_MATCH_THRESHOLD`** → reuse
    that `person_id`, update the running-mean centroid (`centroid=(centroid*n+emb)/(n+1)`,
    re-normalize, `n_samples++`); else **mint** a new `person_id` (UUIDv7), INSERT a `persons` row
    seeded with this embedding (`display_name` NULL = anonymous).
  - **Threshold honesty:** `FACE_MATCH_THRESHOLD` is **cosine DISTANCE** (`= 1 − similarity`).
    ArcFace same-identity cosine similarity is typically high (~0.4–0.7+ on good crops) but **collapses
    on the low-res/off-angle crops an always-on camera produces** — so an over-tight default
    over-mints (a new ID for every frame), an over-loose default merges different people. Ship a
    deliberately-chosen starting value, a loud `WARN "face matching uncalibrated"`, and treat
    **empirical tuning on the real fixture (sweep distance, watch distinct-ID count vs known people)
    as an in-ticket deliverable**, not a follow-up. Over-splitting is the safe failure (a human merges
    duplicates); merging two people is the unsafe one (bias the threshold toward over-split).
  - **Idempotency:** `scene_objects` is itself the per-observation source of truth (one row per
    detected face, holding the raw 512-d embedding). Before the per-segment `DELETE FROM scene_objects
    WHERE segment_id=$1`, read the segment's prior `(person_id, embedding)` rows; on reprocess **reuse
    those `person_id`s and skip the running-mean update** (centroid + `n_samples` unchanged), then
    re-INSERT. Reprocessing must never duplicate observations, mint duplicate persons, or perturb a
    centroid. (No separate `person_segments` table is needed — unlike `speaker_segments`, because
    `scene_objects` already stores the raw per-observation vector and there can be many faces per
    segment, so delete-by-segment-then-insert is the right idempotency unit, exactly like
    `transcript_sentences`.)

  ### Emotion + activity + scene strategy: local vision-LLM via Ollama (de-risked, mirrors sentiment)

  Recommend deriving **emotion (happy/sad), per-person activity, and the segment scene caption** from
  a **local vision-capable LLM served by the in-stack Ollama** (e.g. `llama3.2-vision`, `llava`, or
  `moondream` — pulled with `ollama pull`, fully offline, reusing the rig Ollama client surface already
  compiled in `hushai-rag/src/llm.rs`). This is the visual analogue of the speaker ticket's
  *sentiment-via-Ollama* choice: zero bespoke model wiring for the descriptive signals, one strict
  prompt per sampled frame ("for each visible person describe expression∈{happy,sad,neutral,…} and a
  short activity phrase; then one overall scene caption + vibe"), strict parse → NULL on
  unparseable/timeout, wrapped in `VISION_LLM_TIMEOUT_MS` so a slow model never stalls the always-on
  pipeline. Writes `scene_objects.emotion` (new column, see migration) + `scene_objects.action`, and
  `video_events.scene_label/vibe/disposition` + the embedded 1024-d caption.

  **Throughput risk — flag loudly, do not hide:** a vision-LLM on a CPU-only box at a 2s cadence is
  **heavy** and will fall behind real-time. Mitigations (pick + document): run the vision-LLM at a
  **reduced cadence** (caption/emotion every Nth segment or 1 frame / N seconds, while the cheap ONNX
  detect+embed runs on every sampled frame for identity), make it **`VISION_LLM_ENABLED` opt-in**, and
  always timeout-guard it. The **face-identity path (cheap ONNX) must not be gated on the vision-LLM**
  — identity ships even if emotion/activity is disabled. *Alternative considered:* a dedicated **FER
  (facial-expression-recognition) ONNX model** for emotion only (e.g. an AffectNet-trained model) is
  faster and per-face-precise but adds a third ONNX model and only covers emotion, not activity/scene
  — keep as a fallback if the vision-LLM proves too slow. Acoustic/visual fine-grained emotion beyond
  coarse happy/sad/neutral is explicitly out of scope (single low-res faces don't support it honestly).

  ### Anonymous-now / name-later / cross-device flow into RAG (mirrors speaker attribution)

  1. Worker mints anonymous `person_id` UUIDs, denormalizes them onto `scene_objects.person_id`
     (`text`, matching `device_id` / the speaker ticket's `transcript_sentences.speaker_id` text
     convention — avoids a uuid-vs-text `ANY()` runtime type error in RAG). For each video segment it
     also writes a `video_events` row: `scene_label` + `vibe` + `disposition` + an **embedded
     1024-d caption** ("Bob is sitting at the kitchen table, smiling") so the existing RAG vector path
     can retrieve visual events the same way it retrieves sentences.
  2. A human assigns names via authenticated backend endpoints (`GET /v1/persons`,
     `PATCH /v1/persons/{id}`, `POST /v1/persons/{id}/merge`) and, in Phase D, via an Android "People"
     screen that shows a **representative face crop** so a human can identify who's who **by sight**
     (the visual analogue of the speaker ticket's by-ear audio snippet). Name resolution is **global /
     cross-device**.
  3. RAG resolves name → `person_id`s and filters on the denormalized column (never a JOIN — the 0003
     recall-cliff lesson). Two retrieval paths, mirroring the speaker ticket: the **semantic** path
     answers "what was Bob doing / who looked upset in the kitchen" by NN-searching the
     `video_events` caption embeddings (1024-d, same space as sentences) with an
     `AND person_id = ANY($::text[])` / scene filter; a **non-semantic `list_sightings_by_person`**
     answers exhaustive "every time we saw Bob / when was Bob here" over `scene_objects`
     (filter person + optional device + time, `ORDER BY start_unix_nanos`, no distance prune, backed
     by a new btree index) so attribution is never truncated by the ANN cliff or the distance prune.

  ### Privacy / biometric posture (resolved: store + retention/purge; louder than voice)

  The 512-d ArcFace vectors in `persons.centroid` and `scene_objects.embedding` **are face biometric
  templates** and the stored face **crops** (for the labeling UI) are biometric images — categorically
  **more sensitive and more regulated than voiceprints** (GDPR special-category data; US BIPA/CCPA face
  templates). v1 **stores them server-side** and must: add a `scene_objects` retention helper
  (`drop_scene_object_partitions_before`, mirroring transcript retention); have device-deletion /
  person-deletion **purge** centroids + raw vectors + cached face crops; and acknowledge the same
  cascade gap as the speaker ticket (deleting a `segments` row cascades its `scene_objects` rows but
  does **not** recompute the running-mean `persons.centroid` until a recluster). **At-rest encryption
  and a consent/notice flow are deferred to a follow-up but flagged as higher-priority than for voice
  given face-template regulation.**

  ---

  ### Changes by component

  **Migration — new `hushai-backend/migrations/000N_person_identity_vision.sql`** (next free number:
  `0006` if the speaker ticket's proposed `0005` has landed, else `0005` — pick the next sequential
  number at PR time; auto-applied on worker/backend startup via the existing `sqlx::migrate!`):

  - `CREATE TABLE persons (person_id uuid PRIMARY KEY, centroid vector(512), n_samples bigint NOT NULL
    DEFAULT 0, display_name text NULL, first_seen_device_id text NULL REFERENCES devices(device_id),
    created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now())` —
    the global face catalogue (mirrors the speaker ticket's `speakers`; `centroid` is its own
    **512-d** space, NOT the 1024-d text space; stored L2-normalized; bind via `to_pgvector_text` +
    `::vector`, never the pgvector Rust type — sqlx-0.8 constraint per AGENTS.md).
  - `CREATE INDEX persons_name_idx ON persons (lower(display_name))` — global name→ID resolution.
  - **Recreate `scene_objects` into the production-shaped, partitioned form** (it is empty — cheap,
    per the 0003 rationale): add `device_id text` (denormalized), `start_unix_nanos`/`end_unix_nanos
    bigint`, **`emotion text`** (the new per-face expression column the user asked for),
    `frame_offset_nanos` (which sampled frame within the 2s), `det_score real`, keep `person_id text`
    / `action text` / `bbox jsonb` / `object_label text` / `embedding vector(512)` /
    `embedding_model` / `embedding_dim`, add `created_at timestamptz NOT NULL DEFAULT now()` as the
    **RANGE partition key** (`PRIMARY KEY (id, created_at)`), `RANGE PARTITION BY (created_at)`.
    Parent indexes (propagate to all partitions): `scene_objects_segment_id_idx (segment_id)` for the
    idempotent delete-by-segment; `scene_objects_person_time_idx (person_id, start_unix_nanos)` for the
    non-semantic `list_sightings_by_person`; `scene_objects_embedding_hnsw USING hnsw (embedding
    vector_cosine_ops)` for face NN search (512-d). `ensure_scene_object_partitions(N)` +
    `drop_scene_object_partitions_before(cutoff)` helpers mirroring the transcript helpers; call
    `ensure_scene_object_partitions(3)` from `lib.rs:run()` next to `ensure_transcript_partitions(3)`.
  - **Recreate `video_events` similarly** (empty): add `device_id text`,
    `start_unix_nanos`/`end_unix_nanos`, `created_at` partition key, keep `scene_label`/`vibe`/
    `disposition`/`embedding vector(1024)`/model/dim; add `video_events_device_time_idx (device_id,
    start_unix_nanos)`, `video_events_segment_id_idx (segment_id)`, and
    `video_events_embedding_hnsw USING hnsw (embedding vector_cosine_ops)` (1024-d caption space) so
    RAG can NN-search visual events alongside sentences.
  - Re-add the `ON DELETE CASCADE` FKs (`0004` re-applies on the recreated tables — fold its
    `scene_objects` / `video_events` clauses into this migration since those tables are recreated).
  - **Migration-safety header comment:** `CREATE INDEX` on a partitioned parent is not concurrent and
    locks per partition; fine for the empty dev tables, note `CONCURRENTLY`-per-partition for prod.

  **`hushai-worker`:**

  - New `vision/` module: `detect.rs` (SCRFD/RetinaFace ONNX via `ort`), `face_embed.rs` (ArcFace
    512-d ONNX via `ort`, with landmark alignment), `frames.rs` (ffmpeg frame sampling → RGB),
    `face_match.rs` (the advisory-locked global match-or-mint + running-mean centroid, idempotent
    delete-by-segment), and `scene.rs` (the Ollama vision-LLM emotion/activity/scene caption,
    timeout-guarded, `VISION_LLM_ENABLED`-gated, reduced-cadence).
  - **Route by `media_type`** in the worker: audio path (existing ASR) for `AUDIO`/`MUXED`; vision
    path for `VIDEO`/`MUXED`. A `MUXED` segment runs both. This also resolves the AGENTS.md fast-follow
    where `cam0-video` segments currently dead-end as `error`. Decide whether vision shares the
    existing `segment_transcription_status` claim row or gets its own status table — recommend a
    separate `segment_vision_status` so an ASR failure and a vision failure are tracked/retried
    independently (state the choice; reuse `claim.rs`'s `FOR UPDATE SKIP LOCKED` lease pattern either
    way).
  - New write fn (mirror `process.rs:write_transcript`): one tx — take the global person advisory
    lock; read prior `scene_objects` person assignments for the segment **before** the DELETE; `DELETE
    FROM scene_objects WHERE segment_id=$1`; match-or-mint each face + update centroids; batched
    multi-row INSERT of `scene_objects` (denormalized `device_id`, `person_id`, `emotion`, `action`,
    `bbox`, 512-d `embedding`); INSERT the `video_events` caption row (1024-d embedding via the
    existing `Embedder`); mark vision status `done`. Idempotent: reprocess replaces, never duplicates.
  - `config.rs` + `.env.example`: `FACE_DETECT_MODEL_PATH`, `FACE_EMBED_MODEL_PATH` (default under
    `./models/`), `FACE_MATCH_THRESHOLD` (cosine distance; ship loose, tune in-ticket),
    `FACE_MIN_DET_SCORE`, `FACE_MIN_PX`, `FACE_MIN_SHARPNESS`, `FRAMES_PER_SEGMENT`,
    `VISION_LLM_ENABLED`, `VISION_LLM_MODEL` (e.g. `llama3.2-vision`), `VISION_LLM_TIMEOUT_MS`,
    `VISION_LLM_EVERY_N_SEGMENTS`, via the existing `opt()/parse()` pattern; thread through
    `lib.rs:run()`.
  - `lib.rs:run()`: construct the detector + face embedder + scene classifier once (beside
    `Transcriber::new`/`Embedder::new`), clone into each worker task; add
    `ensure_scene_object_partitions(3)`.
  - `Cargo.toml`: add `ort` + `ndarray` (+ an image-decode/resize crate, e.g. `image`, for crop +
    alignment) as direct deps. **Native-lib provisioning is first-class, not a footnote** (same as the
    speaker ticket): `ort-sys` does not vendor `libonnxruntime` — choose `download-binaries` (one-time
    build-time fetch, cached) or a vendored lib via `ORT_LIB_LOCATION` for build-time-offline. If the
    speaker ticket already added `ort`, reuse it.

  **`hushai-rag`:**

  - `retrieve.rs`/`Filters`: add `person_id: Option<Vec<String>>` and surface `person_id` on the
    `Source` for visual sources. Add a `nearest_video_events()` (NN over `video_events.embedding`,
    1024-d, with optional `AND person_id = ANY($::text[])` + device/time filters) and a non-semantic
    `list_sightings_by_person()` over `scene_objects` (person + device? + time, `ORDER BY
    start_unix_nanos`, no prune) backed by `scene_objects_person_time_idx`.
  - `routes.rs`/`QueryFilters`: add `person_id` and `person_name`; resolve `person_name` → `Vec<Uuid>`
    globally via `persons` (same failure-mode contract as the speaker ticket: unknown name → empty
    sources, not error; ambiguous → union; `person_id` wins over `person_name`; NULL person rows never
    match a name filter). Decide how visual events join the answer — recommend the query can draw from
    **both** `transcript_sentences` and `video_events` (a person was both heard and seen), or expose a
    `modality` filter (`audio | video | both`). `llm.rs:build_prompt` includes the resolved
    `display_name` (or "unknown person") per visual source so the model attributes correctly.

  **`hushai-backend`** (new derived surface; no proto/contract change — mirror `/v1/speakers`):

  - New module `persons.rs` (register in `lib.rs`), handlers shaped like `ingest.rs` /
    the speaker ticket's `speakers.rs`, on the **authenticated** sub-router (`auth::require_bearer`):
    - `GET /v1/persons` — list discovered persons (`person_id`, `display_name`, `n_samples`, a few
      **bounded** recent sightings via `LATERAL LIMIT 3` over the partitioned `scene_objects` using
      `scene_objects_person_time_idx`).
    - `PATCH /v1/persons/{id}` — set `display_name` (idempotent).
    - `POST /v1/persons/{id}/merge {into}` — manual merge of two IDs for the same person (over-split is
      guaranteed by the conservative matcher): bulk `UPDATE scene_objects SET person_id=<canonical>`
      across partitions, combine centroids weighted by `n_samples` (re-normalize) + sum `n_samples` on
      the survivor, preserve the survivor's `display_name`, DELETE the losing `persons` row.
    - `GET /v1/persons/{id}/sample-face` (**Phase D enabler**) — return a representative **cropped
      face image** (decode the best-scoring stored sighting's frame from `segments.blob_uri`, crop to
      `bbox`) so a human can identify the person **by sight** when naming. A label or 512-d vector
      alone is not human-identifiable.
  - `db.rs`: `list_persons`, `set_person_name`, `merge_persons`. **Same build-ordering trap as the
    speaker ticket:** compile-time `sqlx::query!` macros need the new tables present at build +
    `cargo sqlx prepare` re-run, **or** use **runtime `sqlx::query`** for the new handlers — recommend
    runtime queries (lower friction; backend is the only crate using the offline `.sqlx/` cache).

  **`hushai-android`** (Phase D — the end-user labeling UI):

  - New "People" screen mirroring `ui/CaptureScreen.kt` (and the speaker ticket's "Voices" screen):
    list persons from `GET /v1/persons`, **show the `sample-face` crop** to ID the person by sight,
    type a name and `PATCH`, and a merge action. New JSON net client over the existing OkHttp,
    reusing url/token from the `config/Settings.kt` DataStore. Global catalogue (device filter
    optional). No change to the capture/upload contract.

  ### Phasing (all in scope; recommend separate PRs per phase)

  - **Phase A — face identity MVP (the headline, the user's "most important part"):** model
    provisioning (pin SCRFD + ArcFace ONNX + shas; resolve `ort`/`libonnxruntime`); the migration's
    `persons` table + recreated partitioned `scene_objects`; worker `vision/` detect+align+embed +
    quality gates + the advisory-locked global match-or-mint + idempotent per-segment write +
    media-type routing; the empirical `FACE_MATCH_THRESHOLD` tuning loop. Ships the catalogue of
    anonymous faces with cross-device matching. ~500+ lines across worker+migration touching
    high-stakes surfaces (a partitioned-parent migration, new biometric storage) → warrants a deeper
    review.
  - **Phase B — emotion + activity + scene:** worker `scene.rs` (Ollama vision-LLM, timeout-guarded,
    reduced cadence, `VISION_LLM_ENABLED`); write `scene_objects.emotion`/`action` + `video_events`
    caption (embedded). Must not gate Phase A.
  - **Phase C — backend `GET/PATCH/merge /v1/persons` + `GET /v1/persons/{id}/sample-face`** (runtime
    sqlx, authenticated): the labeling surface.
  - **Phase D — Android "People" screen:** label unknown faces by sight, name, merge.
  - **Phase E — RAG attribution:** `video_events` NN retrieval + `list_sightings_by_person` + name
    resolution + prompt attribution, so "when did I see Bob / who was here / was Bob happy" works.
  - **Phase F — accuracy heal (mirrors speaker Phase C):** batch agglomerative recluster over
    `scene_objects` embeddings that merges over-split anonymous person IDs, recomputes centroids,
    bulk-remaps `scene_objects.person_id`, **preserving any human-assigned name** on the surviving
    canonical ID.

  **Committed fast-follow (separate ticket):** rolling multi-frame / short-window **scene** captioning
  + better activity recognition (a 2s timer-cut frame is a poor unit for "what are they doing over
  time"), per the decouple-from-2s-transport lesson in `hushai-conversation-analysis-roadmap`.

- **Acceptance Criteria**:
  - [ ] **(A)** Feeding a video containing **known, distinct people's faces** + running the worker
        yields `scene_objects` rows with non-NULL `person_id` for frames with a usable frontal face,
        a populated `bbox`, and the **face `embedding` is 512-d** and L2-normalized; the count of
        distinct `person_id`s is within a **sane band** of the true number of people (NOT hundreds —
        guards against an over-tight threshold passing for the wrong reason; NOT collapsed to 1 —
        guards against over-loose).
  - [ ] A `persons` row exists per minted `person_id` with `n_samples>0`, `centroid` non-NULL +
        L2-normalized, `display_name` initially NULL, `first_seen_device_id` set.
  - [ ] Frames with **no usable face** (no face / too small / too blurry / back-of-head / extreme
        profile) write **no** `scene_objects` person row, mint **no** `persons` row, fold **no**
        centroid, and **do not crash or error** the worker — and `cam0-video` segments no longer pile
        up as `error` in the queue (media-type routing resolved).
  - [ ] **Cross-device:** the same person captured under **two different `device_id`s** matches to
        **one** `person_id` (not split by device), and a name set once resolves across both.
  - [ ] **(B)** For segments with a clearly happy and a clearly sad/neutral face, `scene_objects.emotion`
        is populated with **more than one** distinct value across the fixture (a constant classifier
        fails this); `scene_objects.action` and a `video_events` row (scene caption + `vibe` +
        embedded 1024-d vector) are written; NULL on unparseable/timeout; with `VISION_LLM_ENABLED=false`
        the face-identity path (A) still works fully.
  - [ ] **(C)** `GET /v1/persons` (Bearer) returns discovered persons with **bounded** recent
        sightings; `PATCH /v1/persons/{id} {"display_name":"Bob"}` returns 200 and persists (re-GET
        shows it); `POST /v1/persons/{id}/merge {"into":<other>}` repoints `scene_objects`, combines
        centroid/`n_samples`, preserves the survivor's name, deletes the losing row;
        `GET /v1/persons/{id}/sample-face` returns a **real cropped face image** of that person.
  - [ ] **(D)** The Android "People" screen lists discovered persons, **shows a face crop** per ID,
        lets a human name an unknown face (PATCH) and merge duplicates, reusing url/token from the
        Settings DataStore; `./gradlew :app:assembleDebug` green.
  - [ ] **(E)** `POST /v1/rag/query` (`:8090`) with `filters.person_name="Bob"` returns an answer +
        sources where **every** source resolves to Bob; "when did I see Bob" / "every time we saw Bob"
        routes through the exhaustive `list_sightings_by_person` and is **not** truncated by the
        distance prune; asking about a **different** person filtered to Bob returns a no-info decline
        or only Bob's sightings (no cross-attribution); unknown `person_name` → empty sources (not an
        error).
  - [ ] **Idempotency (strict):** reprocessing a segment leaves `scene_objects` row count for that
        segment unchanged, each observation's `person_id` unchanged, **and** every affected
        `persons.n_samples` + `centroid` byte-for-byte unchanged. No duplicate `persons` minted, no
        duplicate `video_events`.
  - [ ] **Cold-start:** ingesting a brand-new face's first segments under `WORKER_CONCURRENCY>=2` does
        **not** mint duplicate `person_id`s for the same first-seen face (global advisory lock
        serializes mint).
  - [ ] **(F)** Running the batch recluster over a deliberately over-split set merges duplicate
        anonymous person IDs, recomputes centroids, bulk-remaps `scene_objects.person_id`, and
        **preserves any human-assigned name** on the canonical ID.
  - [ ] **Build/link:** `cargo build -p hushai-worker` **links `libonnxruntime`**, and the detector +
        ArcFace embedder load their `.onnx` and return a detection + a 512-d vector for a fixture frame
        (a check separate from a generic "worker builds" that would pass while `ort` is unwired).
        Worker/RAG/backend build + start; the migration applies cleanly on startup; existing tests pass.
  - [ ] **Runtime offline:** no cloud calls — face models load from disk, scene/emotion + RAG use local
        Ollama. (Build-time offline holds only on the vendored-`libonnxruntime` path; `download-binaries`
        fetches once at build — stated, not hidden.)
  - [ ] **Privacy:** `drop_scene_object_partitions_before(...)` retention helper exists and works;
        device/person deletion purges centroids + raw face vectors + cached crops; the centroid cascade
        gap is documented; the face-template regulatory sensitivity is called out.
  - [ ] **Pre-feature / no-face history:** existing audio-only corpus has no `scene_objects`; a person
        name filter never matches and never errors; "when did I see Bob" correctly excludes
        pre-feature history (documented, not a bug).

- **How to Test** (real end-to-end on the live local stack; human-in-the-loop steps marked **[HUMAN]**;
  I'll drive every DB/curl/adb/ffmpeg/build step myself — the face-identity correctness and the
  emotion/by-sight labeling genuinely need human eyes, so I'll prompt you for those specifically):

  Setup: Postgres (pgvector ≥0.8) + local Ollama up (`mxbai-embed-large`, `llama3.2:3b`, **plus a
  vision model — `ollama pull llama3.2-vision` / `llava` / `moondream`** for Phase B); place the pinned
  SCRFD + ArcFace `.onnx` under `./models/` (record shas) and provision `libonnxruntime` (dev:
  `ort` `download-binaries`; air-gapped: vendor + `ORT_LIB_LOCATION`); apply the new migration (start
  backend once). `cd hushai-backend && SQLX_OFFLINE=true cargo run` (`:8080`, `DEVICE_TOKEN=
  dev-secret-token`).

  1. **(A — identity) [HUMAN, ears/eyes]** Create a committed video fixture with **two genuinely
     distinct people's faces** appearing in clear frontal frames at different times (record on the
     phone, or two distinct stock clips), confirm it has a video track, mux to mp4 under
     `local_dev/captures/` (or `models/test-assets`), document expected person count = 2. Feed it as
     real 2s segments: `python3 local_dev/feed_segments.py --device cam-faces --video .../faces.mp4
     --seg-seconds 2 --url http://localhost:8080/v1/segments --token dev-secret-token` (confirm HTTP
     200s + pending status rows). Run `SQLX_OFFLINE=true cargo run -p hushai-worker`; wait until vision
     status done == video-segment count.
  2. **Observable:** `SELECT person_id, emotion, action, bbox FROM scene_objects WHERE
     device_id='cam-faces' ORDER BY start_unix_nanos` → non-NULL `person_id` on clean frontal frames
     (NULL/no-row on no-face/blurry frames), **~2 distinct person_ids (NOT hundreds, NOT 1)**, 512-d
     embeddings present. **Objective separation:** `count(DISTINCT person_id)` in a small band around
     2; cosine distance between the two `persons` centroids **exceeds** `FACE_MATCH_THRESHOLD`. **Run
     the threshold sweep here** (vary `FACE_MATCH_THRESHOLD`, watch distinct-ID count vs the known 2,
     record the calibrated value). **[HUMAN eyes]** spot-check that IDs roughly track the two real
     people across their frames (per-frame labeling is coarse; boundary/missed-face errors expected —
     a documented v1 limit, not a failure).
  3. **Cross-device:** re-feed the **same** `faces.mp4` under a second device (`--device cam-faces-2`),
     run the worker. **Observable:** the two faces map to the **same two `person_id`s** as device 1
     (global match), not four new IDs.
  4. **(B — emotion/activity) [HUMAN]** Add (or include in the fixture) a clearly **happy** face
     (smiling) and a clearly **sad/neutral** face; with `VISION_LLM_ENABLED=true` re-run.
     **Observable:** `SELECT DISTINCT emotion FROM scene_objects WHERE device_id='cam-faces'` →
     **more than one** distinct non-NULL value (e.g. both `happy` and a non-happy label); `action`
     populated; a `video_events` row exists with a non-empty `scene_label`/caption + a non-NULL 1024-d
     embedding. Then set `VISION_LLM_ENABLED=false`, reprocess → identity (A) still fully populated.
  5. **(C — catalog + label) [HUMAN eyes]** `GET http://localhost:8080/v1/persons` (Bearer
     dev-secret-token) → the discovered rows with bounded sightings. `GET
     /v1/persons/{id}/sample-face` for each ID → **a real cropped face image**; eyeball which crop is
     "Bob". Name it: `curl -X PATCH -H 'Authorization: Bearer dev-secret-token' -H 'Content-Type:
     application/json' -d '{"display_name":"Bob"}' .../v1/persons/<bob-uuid>`; re-GET →
     `display_name="Bob"`. If the matcher over-split Bob, exercise merge: `curl -X POST
     .../v1/persons/<loser-uuid>/merge -d '{"into":"<bob-uuid>"}'` → `scene_objects` repointed + one
     fewer `persons` row, centroid/`n_samples` combined, name preserved.
  6. **(E — RAG attribution)** `SQLX_OFFLINE=true cargo run -p hushai-rag` (`:8090`). Exhaustive: `curl
     -X POST .../v1/rag/query -d '{"query":"when did I see Bob?","filters":{"person_name":"Bob"}}'`.
     **Observable:** every source resolves to Bob and spans **both** fed devices (cross-device, no
     device filter needed); **[HUMAN eyes]** the cited sightings/times match where Bob actually
     appeared, not the other person. Emotion query: `{"query":"was Bob happy?","filters":
     {"person_name":"Bob"}}` → grounded in Bob's `emotion`/caption rows.
  7. **Negative / scope:** query about the **other** person filtered to Bob → no-info decline or only
     Bob's sightings (no cross-attribution). Unknown `person_name` → empty sources (not an error). A
     no-face fixture (`ffmpeg -f lavfi -i color=c=black:s=320x240:r=15 -t 6 .../black.mp4`, fed) →
     **zero** `scene_objects`/`persons`, worker logs a clean skip, no `error` rows.
  8. **Idempotency (strict):** `UPDATE segment_vision_status SET status='pending' WHERE segment_id=$x`
     (or the chosen vision-status mechanism); let the worker re-claim. **Observable:** `scene_objects`
     count for `$x` unchanged, each `person_id` unchanged, every affected `persons.n_samples` +
     `centroid` **byte-for-byte unchanged**, no duplicate `persons` or `video_events`.
  9. **(F — recluster)** drive the matcher to over-split (tight threshold) so one face gets ≥2 IDs,
     name one, run the recluster. **Observable:** duplicate anonymous IDs merge,
     `scene_objects.person_id` is bulk-remapped, the human name survives on the canonical ID.
  10. **(D — Android) [HUMAN on device]** `adb reverse tcp:8080 tcp:8080`; install
      (`JAVA_HOME=/opt/homebrew/opt/openjdk@17 ANDROID_HOME="$HOME/Library/Android/sdk" ./gradlew
      :app:installDebug`). Open the new "People" screen → it lists discovered persons with **face
      crops**; pick an unknown face, **eyeball the crop**, type a name, save (PATCH); merge two
      duplicates. **Observable:** the name persists (re-open + `GET /v1/persons`), and a subsequent
      `/v1/rag/query` with that name attributes correctly. `./gradlew :app:assembleDebug` green.
  11. **End-to-end on the real camera [HUMAN, on device]** (the real-world proof, not just fed
      fixtures): run the actual Android capture client pointed at the live backend with a **real person
      in front of the camera** (`./local_dev/run_hushai_app.sh --url http://localhost:8080 --token
      dev-secret-token --duration 120`), let the worker process the real `cam0-video` segments, then
      `GET /v1/persons` → a new anonymous person appears for the real face; name them; re-appear in
      front of the camera later → **the new sighting auto-attributes to the named person** (the whole
      point: label once, recognized thereafter). I'll drive backend/worker/adb; **[HUMAN]** you stand
      in front of the camera and confirm the by-sight crop + the recognition.
  12. **Single-/no-face regression sanity:** reprocessing an existing **audio-only** capture writes no
      `scene_objects` and does not error → expected (no video), not a regression.
  13. **(Supporting — run last, never the whole proof)** worker tests: the quality gates
      (reject tiny/blurry/low-score crops), the match-or-mint reuses-vs-mints by threshold (two near +
      one far 512-d vectors), the uuid→text `ANY` filter matches a stringified id, reprocess leaves
      centroid/`n_samples` byte-identical; extend the test `cleanup()` to also `DELETE FROM persons`
      (scene_objects cascades from segments, but `persons` has no FK to segments and would leak stale
      centroids that poison the next test's match-or-mint); a `hushai-rag` test asserting the person
      filter restricts results and global name→ID resolution works; a backend integration test for
      `GET/PATCH/merge /v1/persons` + `sample-face` behind auth. All live-DB tests gate on
      `DATABASE_URL` and clean up by `device_id`.

  ---

  **Deferred (explicitly out of scope for this ticket):** rolling multi-frame / short-window scene &
  activity captioning (committed fast-follow); at-rest encryption of face templates + consent/notice
  flow (privacy follow-up, flagged higher-priority than voice); fine-grained emotion beyond coarse
  happy/sad/neutral; re-identification across long time gaps / aging; backfilling pre-feature
  audio-only history. **Depends-on / shares-with:** `Issues/speaker-identity-sentiment-rag-attribution.md`
  (the `ort`/`libonnxruntime` provisioning + the `/v1/speakers` endpoint + RAG-attribution patterns —
  reuse if it lands first, carry them here if not).
