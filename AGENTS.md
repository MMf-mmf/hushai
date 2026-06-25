# AGENTS.md — Project Hushai workspace orientation

Orientation for the next agent/session. Keep this current when you change a
component, convention, or how the stack runs.

## What this is

**Hushai** is a local-first data-intake + retrieval system: capture clients send
audio/video **segments** to a backend that stores them exactly-once; a worker
transcribes + embeds them; a RAG service answers questions over the transcripts.
Everything runs **on-machine, no egress** (local Postgres, local Ollama, local
whisper.cpp).

The authoritative interface between any capture client and the backend is
`contracts/cameraToBackendContract.md` (v0.1.0) — **it wins** on the endpoint,
the `hushai.v1.SegmentManifest` message, client obligations, and response
guarantees. The proto lives once at `hushai-backend/proto/hushai/v1/segment.proto`
and is compiled by both Rust (prost) and Kotlin (Square Wire).

## Components (all built + verified as of 2026-06-24)

| Dir | What | Status |
|-----|------|--------|
| `hushai-backend/` | Rust/Axum segment-ingest server (`POST /v1/segments`, `:8080`). Metadata in Postgres, media as content-addressed blobs under `{BLOB_DIR}/blobs/ab/cd/<sha256>`. | ✅ built + verified |
| `hushai-worker/` | Drains stored segments (NOTIFY-driven, poll backstop) → whisper.cpp ASR → `mxbai-embed-large` (1024-dim) embeddings → `transcript_sentences` (batched insert, `device_id` denormalized). | ✅ built + verified |
| `hushai-rag/` | Axum `POST /v1/rag/query` (`:8090`): pgvector NN over embeddings + `llama3.2:3b` answer via Rig. | ✅ built + verified |
| `hushai-android/` | Native Android capture client (Kotlin). Camera2 + dual MediaCodec → ~2s segments → uploads. **First real client.** See its `README.md`. | ✅ built + **verified E2E on a physical Galaxy S8** |
| `contracts/` | The camera→backend contract (the boundary). | — |
| `local_dev/` | Helper scripts: `feed_segments.py` (replay a video as segments — reference client), `run_hushai_app.sh` (drive the Android app), `export_capture.sh` (reassemble uploaded segments into a playable file). | — |
| `Issues/` | The tickets: `initial-backend.md`, `transcription-embedding-and-rag.md`, `initial-android-app.md`. | all done |

## Run the full stack locally

Postgres (Homebrew `postgresql@16` + pgvector) runs on `localhost:5432`, DB `hushai`,
migrations already applied. Ollama models (`mxbai-embed-large`, `llama3.2:3b`) and the
whisper model (`models/ggml-base.en.bin`) are on disk.

```bash
# 0. dependencies
ollama serve &                                   # localhost:11434 (worker embeddings + rag LLM)

# 1. backend (ingest)            -> :8080
cd hushai-backend && SQLX_OFFLINE=true cargo run

# 2. worker (transcribe+embed)   -> drains pending segments, then polls
cd <root> && SQLX_OFFLINE=true cargo run -p hushai-worker

# 3. rag service                 -> :8090
cd <root> && SQLX_OFFLINE=true cargo run -p hushai-rag
# query:
curl -s localhost:8090/v1/rag/query -H 'content-type: application/json' \
  -d '{"query":"what did people say about the cameras?"}' | jq

# 4. Android client (needs a USB-connected phone — see hushai-android/README.md)
adb reverse tcp:8080 tcp:8080
./local_dev/run_hushai_app.sh --url http://localhost:8080 --token dev-secret-token --duration 120
```

Config is env-driven; root `.env` (gitignored) feeds worker+rag, `hushai-backend/.env`
feeds the backend. Dev token: `dev-secret-token`.

## Build/run gotchas (non-obvious)

- **Android toolchain** is installed no-sudo in non-standard spots: `JAVA_HOME=/opt/homebrew/opt/openjdk@17`,
  `ANDROID_HOME=~/Library/Android/sdk`, **adb is NOT on PATH** (`$ANDROID_HOME/platform-tools/adb`).
  Build with the project's `./gradlew` (pinned 8.9) — the system `brew` Gradle (9.x) is too new for AGP 8.7.3.
- **Phone connection:** the test phone is a **Galaxy S8 = Android 9**, so wireless `adb pair` does NOT
  exist (Android 11+). Use **USB** + Developer options → USB debugging. With USB connected, prefer
  `adb reverse tcp:8080 tcp:8080` + app URL `http://localhost:8080`.
- **sqlx:** query metadata is committed under each crate's `.sqlx/` (backend only — worker/rag use
  runtime queries); build with `SQLX_OFFLINE=true`. If you add/alter a backend `query!` macro, run
  `cd hushai-backend && DATABASE_URL=… cargo sqlx prepare -- --lib` and commit the `.sqlx/` change.
  pgvector is bound as **text + `::vector` cast** (sqlx 0.8 can't bind `pgvector::Vector`); switching to
  native binary binding needs the workspace bumped to sqlx 0.9 + pgvector 0.4.2 — a deferred capacity item.
- **Migrations auto-apply** on worker/backend startup via `sqlx::migrate!`. `0003_scalability.sql`
  recreates `transcript_sentences` as a **monthly RANGE-partitioned** table — see the schema note below.
- **No TLS yet** — backend serves cleartext HTTP; Android debug build permits cleartext to the LAN.

## `transcript_sentences` storage (post-0003 — read before touching RAG/worker writes)

The chunk store is **RANGE-partitioned by `created_at` (monthly)** with a `DEFAULT`
catch-all partition. Parent-level indexes propagate to every partition:
- `…_embedding_hnsw` — HNSW `vector_cosine_ops` (the ANN index; built per partition).
- `…_device_time_idx` — `(device_id, start_unix_nanos)`. **`device_id` is denormalized
  onto the table** so RAG filters sit on the same table as the HNSW index (no JOIN).
- `…_segment_id_idx` — makes the worker's idempotent delete-by-segment an index scan.

Rules:
- **Writes** (`hushai-worker/src/process.rs::write_transcript`) must set `device_id` and use
  the batched multi-row INSERT. `created_at` defaults to `now()` — don't set it.
- **Retrieval** (`hushai-rag/src/retrieve.rs::nearest`) runs in a txn that `SET LOCAL`s
  `hnsw.iterative_scan='strict_order'` + `hnsw.ef_search` + `statement_timeout`. Filter on
  `ts.device_id` / `ts.start_unix_nanos` (local columns), never via a JOIN to `segments`.
- **Partitions:** worker calls `ensure_transcript_partitions(3)` at startup; for long uptimes a
  cron/pg_cron job must also call it monthly. Retention = `drop_transcript_partitions_before(cutoff)`.
- Requires **pgvector ≥ 0.8** (iterative_scan + HNSW on a partitioned parent). Installed: 0.8.0.
- New segments are queued for transcription **at ingest** (status row written in the segment
  txn in `hushai-backend/src/db.rs`) and a `pg_notify('hushai_segment_ingested', …)` wakes the
  worker. The old idle full-table backfill scan is gone (startup backfill remains for old rows).

## Recent fixes (2026-06-24, this session)

- **RAG/chunk-storage scalability** (migration `0003` + worker/rag/backend): see the
  `transcript_sentences` section above. Closed: the filtered-search recall cliff (device filter
  was on a JOINed table the HNSW index couldn't use), the per-write full-table scan (no index on
  `segment_id`), per-row inserts, the repeated idle backfill scan, and unbounded single-index
  growth (now partitioned + retention helpers). Deferred (capacity-tier): native binary vector
  encoding (sqlx 0.9 bump). Multi-tenancy is intentionally out of scope (single-tenant).

- **Worker container handling** (`hushai-worker/src/media.rs`): the worker previously
  ALWAYS prepended `codec_init_data` to the blob, assuming the `feed_segments.py` fMP4
  convention. The Android client uploads **self-contained MP4s** (`container="mp4"`) with
  raw SPS/PPS in `codec_init_data`; prepending corrupted them ("moov atom not found").
  Fixed to prepend only when `container == "fmp4"` (keys off `container`, per §7). The
  Android client is the conformant side. After the fix, Android audio transcribes and is
  RAG-retrievable end-to-end (verified).

## Fast-follows (not yet done)

- **Worker skips/erroring on video-only segments**: `cam0-video` segments have no audio, so
  ffmpeg `-vn` extraction errors → they sit as `error` in `segment_transcription_status`
  (43 of them). The worker should filter to `media_type IN (AUDIO, MUXED)` (or treat
  "no audio stream" as an empty transcript, not an error). Harmless but noisy.
- Android: crash-durable on-disk retry queue, TLS, Doze/battery hardening, shared-proto golden-vector CI.
- The multi-agent adversarial review of the Android client stalled; re-run if deeper scrutiny is wanted.
- Nothing in this work is committed yet — it's all uncommitted on `master` (incl. the worker fix above).

## Where to look

- Per-component detail: each crate's `README.md` (esp. `hushai-backend/README.md`, `hushai-android/README.md`).
- Session memories (auto-loaded): `hushai-android-task`, `hushai-android-build-env`, `hushai-transcription-rag`.
