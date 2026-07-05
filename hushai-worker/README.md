# hushai-worker

The **perception worker** for the Hushai stack: a durable, resumable, idempotent
background service that drains the segment backlog written by
[`hushai-backend`](../hushai-backend/README.md) and turns stored audio/video into
searchable, attributed data. It **claims** pending segments from Postgres with
`FOR UPDATE SKIP LOCKED` (so many worker tasks/instances never double-process),
transcribes + embeds + runs vision on each, and writes the results in a single
per-segment transaction. Every write is **idempotent per source segment**
(delete-by-segment then insert, terminal status), so a crash mid-flight, a lease
re-claim, or a full reprocess replaces rows rather than duplicating them. After the
backlog drains the worker keeps running: it waits on a Postgres `NOTIFY`
(`hushai_segment_ingested`) with a poll-interval backstop and picks up new segments
with near-immediate latency. The worker has **no HTTP port** — it self-reports
liveness into `worker_heartbeat` and (optionally) exposes a tiny `/metrics` +
`/healthz` server.

It reuses `hushai-backend` as a **path dependency** for the shared `Config`,
DB pool, migrations, and schema (it never duplicates the schema-owning crate's knobs).

## Two lanes, one process

The worker fans out two independent claim/status queues so an ASR failure and a
vision failure retry separately:

- **Audio lane** — `worker_loop` → `process::process_segment`. Claims
  **AUDIO/MUXED** segments (`media_type IN (1,3)`) from `segment_transcription_status`.
- **Vision lane** — `vision_worker_loop` → `vision::write::process_vision_segment`.
  Claims **VIDEO/MUXED** segments (`media_type IN (2,3)`) from `segment_vision_status`.

The vision models are loaded once and shared across all vision loops via a cheap
`Arc` clone (ORT `Session` is `Send + Sync`), so N loops share ONE set of models with
no duplication. Vision self-disables (with a warning) if `VISION_ENABLED=false` or its
models / ORT dylib aren't provisioned — the audio lane keeps running, so an audio-only
deployment works out of the box.

## Audio pipeline (`process.rs`)

A claimed audio segment flows through, in order:

1. **Load + decode** — `media::load_segment` resolves the content-addressed blob;
   `media::extract_pcm` decodes it to 16 kHz mono f32 PCM via ffmpeg.
2. **Skip-silent gate** — cheapest-first: a free RMS floor short-circuits dead air, else
   the Silero VAD makes the definitive speech/no-speech call. No speech ⇒ terminal
   `skipped` (whisper/sentiment/speaker/embed all skipped). **Fails open** (a VAD error
   falls through to normal ASR).
3. **Transcribe** — whisper.cpp (`asr::Transcriber`) → utterances with timestamps.
4. **Chunk** — `chunk::chunk_into_sentences` splits utterances into sentences with
   absolute capture timestamps.
5. **Sentiment** — one segment-level `positive|neutral|negative` label (lexical, local
   LLM), denormalized onto every sentence; NULL on disable/timeout/parse-fail.
6. **Speaker identify** — build a look-back window of contiguous same-stream segments,
   run VAD to strip static/silence, quality-gate + multi-speaker-refuse, then embed the
   cleaned speech (TitaNet 192-d). `None` ⇒ `speaker_id` stays NULL.
7. **Embed sentences** — `embed::Embedder` (Rig → Ollama, `mxbai-embed-large`, 1024-d).
8. **Write transcript** — one tx: speaker **match-or-mint** (`speaker_match`, advisory-
   locked, multi-vector k-NN + mint-guard hysteresis) → `DELETE`+batched-`INSERT` of
   `transcript_sentences` → mark status `done`.
9. **Derive events** — `events_producer::derive_audio_events` materializes a `speech`
   event + evaluates alert rules (best-effort; never fails the transcript).

When the queue drains, worker 0 opportunistically **auto-merges** near-certain duplicate
voices among recently-active speakers (rate-limited, advisory-locked).

## Vision pipeline (`vision/write.rs`)

A claimed video segment flows through:

1. **Load** — `media::load_segment`.
2. **Skip-static gate** — compare a representative frame against the same camera's last
   analyzed frame (in-memory per-camera fingerprint, `vision/motion.rs`). Near-identical
   scene ⇒ terminal `skipped` with no inference. With the one-frame probe (default) only
   ONE frame is decoded until motion is confirmed.
3. **Sample frames** — `vision/frames.rs` decodes a few RGB frames per ~2s segment.
4. **Per frame (on the blocking pool):**
   - **Faces** (required lane): detect (SCRFD default, YuNet fallback) → **clean up** the
     crop (margin → super-resolve if tiny → blind-face-restore → align) → quality-gate →
     ArcFace 512-d embed. "Recover-then-embed" — a small/blurry face is restored rather
     than dropped; already-clean faces skip restoration.
   - **Objects** (optional): RF-DETR runs once per frame; each region + the whole frame is
     CLIP-embedded (open-vocab).
   - **Plates** (optional ALPR): for each vehicle ROI, zoom → detect plate → rectify →
     enhance → OCR.
5. **Aggregate** — cluster + vote plate reads across frames; tag best-shot faces/plates;
   persist cleaned crops under `<blob_dir>/{face_crops,plate_crops}`.
6. **Write** — one tx: face match-or-mint into `persons`/`person_segments` +
   idempotent `scene_objects` reconcile + plate match-or-mint into `license_plates`.
7. **Derive events** — `events_producer::derive_vision_events` (person/plate/object
   events + alert rules; best-effort).

## Module map

| Module | Responsibility |
|--------|----------------|
| `lib.rs` | Boot: config, pool, migrations, model load, spawn the audio + vision loops, heartbeat, governor, delivery, metrics; graceful shutdown. |
| `main.rs` | Thin `#[tokio::main]` → `hushai_worker::run()`. |
| `config.rs` | `WorkerConfig::from_env()` — every env knob + defaults + the derived CPU thread budget. |
| `claim.rs` | Durable work-claiming (`FOR UPDATE SKIP LOCKED`), lease/crash re-claim, status backfill, startup self-heal/reconcile, vision-queue twins. |
| `media.rs` | Resolve a segment's blob + decode to PCM (container-keyed fMP4/MP4 reconstruction); speaker-window candidate loading. |
| `asr.rs` | Local ASR via whisper.cpp (`whisper-rs`); shared model, per-call blocking inference. |
| `chunk.rs` | Pure whisper-utterances → sentences with absolute timestamps (unit-tested). |
| `embed.rs` | Local text embeddings via Rig/Ollama; hard 1024-dim check. |
| `sentiment.rs` | Per-segment lexical sentiment via local Ollama LLM (hard-timeout guarded). |
| `vad.rs` | Pure speaker-accuracy guards: silence verdict, quality classification, vector helpers. |
| `speaker.rs` | Speaker embeddings (sherpa-onnx TitaNet 192-d) + the Silero `VoiceDetector`. |
| `speaker_match.rs` | Advisory-locked, idempotent speaker match-or-mint (k-NN + mint-guard + self-healing centroid). |
| `process.rs` | The audio per-segment pipeline + the idempotent transcript write tx. |
| `vision/` | The vision lane — `model` (ORT setup), `frames`, `detect`/`detect_scrfd`, `face_embed`, `enhance`, `geom`, `motion`, `objects`, `face_match`, `plates/` (detect/rectify/ocr/normalize/plate_match), `write` (the pipeline + write tx). |
| `governor.rs` | Device load governor — samples backlog-lag trend + OS load, publishes Normal/Elevated/Saturated to pace the loops. |
| `events_producer.rs` | Turn committed detections into sessionized `events` (the proactive VSaaS layer). |
| `alerts.rs` | Alert evaluator — match one event against `alert_rules`, write `alert_deliveries` outbox rows. |
| `delivery.rs` | Notification delivery loop — drain the outbox → outbound webhook POSTs (crash-safe lease + backoff). |

## Efficiency gates

The worker actively avoids paying for inference it doesn't need. Summarized (full detail
in [`../AGENTS.md`](../AGENTS.md), section *"Worker efficiency: skipped status / ingest
hint gate / skip-silent / skip-static / load governor"*):

- **Skip-silent (audio)** — RMS floor then Silero VAD before whisper; a silent segment is
  terminal `skipped`, writing nothing. Fails open.
- **Skip-static (vision)** — per-camera motion fingerprint; an unchanged scene skips ALL
  vision inference (`skipped/static_gate`). One-frame probe by default.
- **Ingest-hint gate** — the backend pre-terminates below-threshold lanes as `skipped`
  using the Android client's raw per-segment hints (with 2% audit sampling the worker's
  own gate scores as `agree`/`disagree`). Owned by the backend; the worker records the
  audit verdict.
- **Load governor** — under saturation the expensive vision lane pauses first and both
  lanes take an inter-segment cooldown; nothing is reordered or dropped (deferral only).
- **Terminal `skipped` status** — dashboard-visible, never re-claimed, immune to the
  startup speaker reconcile (statuses: `pending | processing | done | error | skipped`).

## Parallelism & scaling

Audio + vision loops each run their own inference; the per-call whisper `n_threads` and
per-session ORT intra-op threads are sized to `cores / (audio + vision loops)` so parallel
loops fill the CPU instead of thrashing it. Full technical reference (verified
thread-safety, thread budget, capacity results):
[`../docs/worker-parallelism-and-scaling.md`](../docs/worker-parallelism-and-scaling.md).
Hardware sizing for 30 cameras: [`../docs/hardware-sizing-30-cameras.md`](../docs/hardware-sizing-30-cameras.md).

## Configuration

Read from the environment (`.env` in cwd, falling back to `hushai-backend/.env`). DB /
blob / token config comes from the backend's `Config` (`DATABASE_URL`, `BLOB_DIR`,
`DEVICE_TOKEN`). The most important worker knobs (see [`config.rs`](src/config.rs) for the
complete set; [`.env.example`](.env.example) documents every one with commentary):

| Env var | Default | Effect |
|---------|---------|--------|
| `WHISPER_MODEL_PATH` | `./models/ggml-base.en.bin` | whisper.cpp GGML ASR model. |
| `EMBED_MODEL` | `mxbai-embed-large` | Text-embedding model (must be 1024-dim). |
| `EMBED_OLLAMA_BASE_URL` | `OLLAMA_BASE_URL` / `:11434` | Ollama endpoint for embeddings. |
| `LLM_OLLAMA_BASE_URL` | `OLLAMA_BASE_URL` / `:11434` | Ollama endpoint for sentiment. |
| `SENTIMENT_MODEL` / `SENTIMENT_ENABLED` | `llama3.2:3b` / `true` | Per-segment sentiment classifier + master switch. |
| `WORKER_CONCURRENCY` | `2` | Concurrent audio pipelines. |
| `VISION_CONCURRENCY` | `2` | Concurrent vision pipelines. |
| `ASR_THREADS` / `ORT_INTRA_THREADS` | `0` (derived) | Per-call thread budget override; `0` ⇒ `cores/(audio+vision)`. |
| `POLL_INTERVAL_SECS` | `5` | Backstop poll when the queue is drained (NOTIFY drives keep-up). |
| `MAX_ATTEMPTS` | `5` | Attempts before a segment is left `error`. |
| `LEASE_TIMEOUT_SECS` | `300` | A `processing` claim older than this is treated as crashed + re-leased. |
| `FFMPEG_BIN` | `ffmpeg` | ffmpeg binary for decode. |
| `AUDIO_SILENCE_SKIP_ENABLED` | `true` | Skip-silent gate master switch. |
| `AUDIO_SILENCE_RMS_FLOOR` / `AUDIO_SILENCE_MIN_SPEECH_SECS` | `0.005` / `0.2` | Skip-silent RMS floor + post-VAD speech floor. |
| `SPEAKER_MODEL_PATH` | `./models/nemo_en_titanet_large.onnx` | TitaNet speaker embedder (192-d). |
| `VAD_MODEL_PATH` | `./models/silero_vad.onnx` | Silero VAD (speaker gate + skip-silent stage 2). |
| `SPEAKER_MATCH_THRESHOLD` / `SPEAKER_MINT_DISTANCE_FLOOR` | `0.5` / `0.72` | Match-or-mint hysteresis (cosine distance). |
| `SPEAKER_WINDOW_ENABLED` / `SPEAKER_WINDOW_TARGET_SECS` | `true` / `6.0` | Aggregate short clips before speaker embedding. |
| `SPEAKER_BACKFILL_ON_START` | `true` | Re-queue `done` audio missing a voiceprint (self-heal). |
| `SPEAKER_AUTOHEAL_ENABLED` | `true` | Auto-merge near-certain duplicate voices after a drain. |
| `VISION_ENABLED` | `true` | Vision lane master switch (self-disables if models missing). |
| `VISION_COREML` | `true` | Register the CoreML EP (Apple Silicon) ahead of CPU. |
| `ORT_DYLIB_PATH` | `./models/onnxruntime/.../libonnxruntime.1.20.0.dylib` | ONNX Runtime dylib `ort` dlopen()s (1.20.x; separate from sherpa's bundled 1.17.1). |
| `VISION_MOTION_SKIP_ENABLED` / `VISION_MOTION_THRESHOLD` | `true` / `8.0` | Skip-static gate + distance threshold. |
| `WORKER_GATE_ONE_FRAME_PROBE` | `true` | Decode one frame for the motion gate before the full sample. |
| `FRAMES_PER_SEGMENT` | `3` | Frames sampled per ~2s vision segment. |
| `FACE_DETECTOR_KIND` | `scrfd` | `scrfd` (default) or `yunet`; falls back to whichever is provisioned. |
| `FACE_DETECT_MODEL_PATH` / `FACE_SCRFD_MODEL_PATH` / `FACE_EMBED_MODEL_PATH` | `yunet` / `scrfd_10g_bnkps` / `w600k_r50` | YuNet / SCRFD / ArcFace ONNX paths. |
| `FACE_RESTORE_MODEL_PATH` / `FACE_UPSCALE_MODEL_PATH` | `gfpgan_v1.4` / `realesrgan_x4plus` | Optional face restore + super-res (self-disable if absent). |
| `OBJECT_DET_MODEL_PATH` / `CLIP_IMAGE_MODEL_PATH` / `OBJECT_REQUIRED` | `rf-detr-nano` / `clip_vit_b32_image` / `false` | Object lane models + hard-require switch. |
| `PLATE_ENABLED` / `PLATE_DETECT_MODEL_PATH` / `PLATE_OCR_MODEL_PATH` | `true` / `lp_detector` / `lp_ocr_cct` | ALPR lane + models (self-disables if absent). |
| `EVENTS_ENABLED` / `EVENTS_ALERTS_ENABLED` | `true` / `true` | Proactive event production + alert-rule evaluation. |
| `ALERT_DELIVERY_ENABLED` | `true` | Webhook delivery loop. |
| `LOAD_GOVERNOR_ENABLED` | `true` | Device load governor (pace under load). |
| `WORKER_HEARTBEAT_SECS` / `WORKER_ID` | `10` / `<host>:<pid>` | Liveness heartbeat cadence + row id. |
| `WORKER_METRICS_ADDR` | `127.0.0.1:9100` | Prometheus `/metrics` + `/healthz`; empty disables. |

## Running it

The worker is normally launched as part of the full stack:

```bash
../local_dev/run_stack.sh        # infra + backend + worker + rag + viewer
```

Directly (CWD = repo root, so `models/*` + `blobs` resolve relative to it):

```bash
export DATABASE_URL=postgres://localhost/hushai   # from the backend Config
export BLOB_DIR=./data                             # shared blob root
export DEVICE_TOKEN=dev-token
cargo run -p hushai-worker
```

It applies the backend migrations, ensures the RANGE partitions, loads the models, and
starts draining. Requires a reachable Postgres + Ollama (for embeddings/sentiment) and
`ffmpeg` on `PATH`. See [`../hushai-backend/README.md`](../hushai-backend/README.md) for
DB provisioning.

## Testing

```bash
cargo test -p hushai-worker
```

- **DB-gated** suites (`tests/worker_db.rs`, `tests/delivery.rs`) skip cleanly when
  `DATABASE_URL` is unset; set it (and run the backend migrations) to exercise the
  claim/lease, idempotent write, mint-guard, tombstone-reconcile, and delivery paths
  against a live DB.
- **Model/dylib-gated** suites (`tests/vision_pipeline.rs`, `tests/ort_coexistence.rs`)
  skip when the ORT dylib / vision models / a real mp4 aren't present. `ort_coexistence`
  proves `ort` (1.20) and sherpa (1.17.1) load two onnxruntimes in one process without
  symbol collision. Run e.g.
  `cargo test -p hushai-worker --test vision_pipeline -- --nocapture`.

## Models required

All models are **gitignored / operator-provisioned** and fetched via `local_dev/` scripts;
paths above are the defaults. Reference: [`../docs/vision-image-cleanup-and-alpr.md`](../docs/vision-image-cleanup-and-alpr.md)
and [`../docs/perception-hardening.md`](../docs/perception-hardening.md).

| Model | Purpose | Provisioned by |
|-------|---------|----------------|
| whisper GGML (`ggml-base.en.bin`) | ASR | see [`../hushai-backend/README.md`](../hushai-backend/README.md) |
| Silero VAD (`silero_vad.onnx`) | VAD (speaker + skip-silent) | `../local_dev/fetch_vad_model.sh` |
| TitaNet-large (`nemo_en_titanet_large.onnx`) | Speaker embeddings | sha-pinned in `.env.example` |
| ONNX Runtime dylib (1.20.x) | Vision inference runtime | `../local_dev/fetch_onnxruntime.sh` |
| SCRFD (`scrfd_10g_bnkps.onnx`) | Face detection (default) | `../local_dev/fetch_scrfd.sh` |
| ArcFace (`w600k_r50.onnx`) | Face embeddings | `../local_dev/provision_vision.sh` |
| RF-DETR + CLIP (`rf-detr-nano.onnx`, `clip_vit_b32_image.onnx`) | Objects (open-vocab) | `../local_dev/provision_vision.sh` |
| Real-ESRGAN (`realesrgan_x4plus.onnx`) | Super-resolution (faces + plates) | `../local_dev/fetch_realesrgan.sh` |
| GFPGAN/CodeFormer (`gfpgan_v1.4.onnx`) | Blind face restoration | operator-provided (see docs) |
| Plate detector (`lp_detector.onnx`) | ALPR detection | `../local_dev/fetch_plate_detector.sh` (`PLATE_DETECTOR_ONNX_URL`) |
| Plate OCR (`lp_ocr_cct.onnx` + charset) | ALPR OCR | `../local_dev/provision_vision.sh` (`export_plate_ocr.py`) |

Vision degrades gracefully: only face detect + embed + the ORT dylib are required to run
the vision lane; objects, restoration, super-res, and plates each self-disable if their
models aren't present (unless `OBJECT_REQUIRED` / `PLATE_REQUIRED` force a hard failure).
