# AGENTS.md — Project Hushai workspace orientation

Orientation for the next agent/session: what the components are, how to run them, and the
non-obvious invariants to preserve. Keep it current when you change a component, a convention,
or how the stack runs. **Dated session-by-session history lives in [`CHANGELOG.md`](CHANGELOG.md)** —
put narrative there, keep this file durable and present-tense.

## What this is

**Hushai** is a local-first data-intake + retrieval system: capture clients send
audio/video **segments** to a backend that stores them exactly-once; a worker
transcribes + embeds + runs vision on them; a RAG service answers questions over the
results. Everything runs **on-machine, no egress** (local Postgres, local Ollama, local
whisper.cpp).

The authoritative interface between any capture client and the backend is
[`contracts/cameraToBackendContract.md`](contracts/cameraToBackendContract.md) (v0.1.0) —
**it wins** on the endpoint, the `hushai.v1.SegmentManifest` message, client obligations, and
response guarantees. The proto lives once at
`hushai-backend/proto/hushai/v1/segment.proto` and is compiled by both Rust (prost) and
Kotlin (Square Wire).

## Component map

| Dir | What it is | Deep dive |
|-----|------------|-----------|
| `hushai-backend/` | Rust/Axum ingest server (`POST /v1/segments`, `:8080`): content-addressed media blobs + Postgres metadata; owns the schema/migrations. Also hosts the authenticated **admin surface**: speakers, persons, plates, devices, events, alert-rules, watchlist, audit. | [`hushai-backend/README.md`](hushai-backend/README.md) |
| `hushai-worker/` | Durable, resumable, idempotent processing of stored segments over **two SKIP-LOCKED queues** — audio (whisper ASR → embeddings → sentiment → speaker ID) and vision (faces → objects → ALPR) — plus event production + alert delivery. No HTTP port (liveness heartbeat row + `/metrics`). | [`hushai-worker/README.md`](hushai-worker/README.md), [`docs/worker-parallelism-and-scaling.md`](docs/worker-parallelism-and-scaling.md) |
| `hushai-rag/` | Axum RAG service (`:8090`): grounded Q&A + multi-turn SSE chat over the embeddings, an **auto-routed agent** registry, and local neural TTS. | [`hushai-rag/README.md`](hushai-rag/README.md) |
| `hushai-viewer/` | The **unified webapp** (`127.0.0.1:8070`): scrubbable HLS NVR **+ chat panel** + admin modals (Voices/People/Plates) + System dashboard + Events/Files pages. Reverse-proxies `/v1/*` (one browser origin, server-side bearer). Also a capture **source** (browser camera/mic). Vanilla ES modules, **no build step**. | [`hushai-viewer/README.md`](hushai-viewer/README.md) |
| `hushai-android/` | Native Kotlin capture client: Camera2 + dual MediaCodec → ~2s segments; live preview, battery-saver, **audio-only mode**, on-device **voice assistant** (RAG chat + TTS), and Voices/People/Plates/Events screens. First real client. | [`hushai-android/README.md`](hushai-android/README.md) |
| `hushai-eval/` | End-to-end regression harness: inject **known** clips into the live pipeline → wait for completion → score vs ground truth → improvement/regression verdict + exit code. | [`hushai-eval/RECURSIVE_TESTING.md`](hushai-eval/RECURSIVE_TESTING.md), [`hushai-eval/README.md`](hushai-eval/README.md) |
| `hushai-loadtest/` | Capacity harness: replay ONE clip as N **synthetic cameras**, ramp 1→N, sample worker/host load, report the **saturation point** + bottleneck stage. | [`hushai-loadtest/README.md`](hushai-loadtest/README.md), [`docs/hardware-sizing-30-cameras.md`](docs/hardware-sizing-30-cameras.md) |
| `hushai-advisor/` | The **Ahithophel advisor** (`:8095`): multi-agent advice pipeline grounded in an ingested book — sufficiency gate (asks follow-up questions) → chapter routing → draft/critique/refine loop → streamed answer → Q&A memory (migrations 0026/0027). Corpus is populated by its `ingest-book` binary from `Agent Ahithophel/books/chapters_text/`. SSE protocol extends the rag chat one with `phase`/`questions`/`chapters` events. Sets `num_ctx` explicitly (`ADVISOR_NUM_CTX`, default 16384) — the only service that does; multi-chapter prompts silently truncate at Ollama's 4096 default otherwise. | [`hushai-advisor/v1_spec.md`](hushai-advisor/v1_spec.md) (executable live-verification spec), [`hushai-advisor/v2_integration_spec.md`](hushai-advisor/v2_integration_spec.md) (client-integration contract: viewer slash-command, voice-by-name, rig E2E), [`local_dev/AhithophelPlan.md`](local_dev/AhithophelPlan.md) |
| `contracts/` | The camera→backend contract — the authoritative boundary. | [`contracts/cameraToBackendContract.md`](contracts/cameraToBackendContract.md) |
| `docs/` | Human-facing runbooks + design references (onboarding, device/footage mgmt, vision/ALPR, scaling, perception hardening, LAN URL, feature parity). | [`docs/`](docs/) |
| `local_dev/` | Scripts to run + provision the stack — see "Running the stack" below. | this file |
| `Issues/` | Tickets: `finished/` (completed, archive) + `unfinished/` (open work). | [`Issues/`](Issues/) |

## Running the stack

Postgres (Homebrew `postgresql@16` + pgvector ≥ 0.8) runs on `localhost:5432`, DB `hushai`,
migrations auto-applied on backend/worker startup. Ollama models (`mxbai-embed-large`,
`qwen2.5:7b` for RAG answers, `llama3.2:3b` for worker sentiment) and the whisper model
(`models/ggml-base.en.bin`) are on disk. Vision/TTS ONNX models are operator-provisioned
(gitignored `models/`; fetch/export via `local_dev/`).

### Fresh machine: `local_dev/onboard.sh`

On a clean checkout (or a new computer), `./local_dev/onboard.sh` is the interactive front
door: it asks how many devices you're connecting (USB / WiFi / IP-camera / browser) and
which AI lanes you want, then installs missing deps (confirming first), starts Postgres +
Ollama, downloads the whisper model, writes `hushai-backend/.env` + root `.env`
authoritatively, mints a per-device token per camera via `run_stack.sh --add-camera`, brings
the stack up, drives USB phones with `run_hushai_app.sh`, and prints a summary of every
URL/token/password. It orchestrates the scripts below rather than replacing them.
Cross-platform (macOS + Linux) via `local_dev/lib_platform.sh` (OS detection + adapters for
pkg-manager, Postgres start, LAN-IP detection, CA trust, dynamic-linker path).

### One command (recommended): `local_dev/run_stack.sh`

```bash
./local_dev/run_stack.sh                 # infra preflight + backend + worker + rag + viewer
./local_dev/run_stack.sh --with-android  # ...also build+drive the USB phone client (best effort)
./local_dev/run_stack.sh --no-build      # skip cargo build; run existing target/debug bins
./local_dev/run_stack.sh --release       # build/run the release binaries
./local_dev/run_stack.sh --pull          # `ollama pull` any missing models first
./local_dev/run_stack.sh --tls           # native TLS + auth (self-signed certs), dev creds in the banner
./local_dev/run_stack.sh --lan           # (implies --tls) bind the viewer on the LAN for https://hushai.local/
./local_dev/run_stack.sh --add-camera N  # mint + persist a per-device token, print a config card
./local_dev/run_stack.sh --test-db       # point the stack at the hushai_test DB (for hushai-eval)
./local_dev/run_stack.sh --down          # stop a stack started earlier
```

**On start it self-heals the ports**: it tears down any stack left from a previous run (via
`stop_from_pidfiles` + reclaiming its own binaries still listening on 8080/8090/8070), so a
plain re-run is always clean and `--down` is rarely needed. A *non-Hushai* process on one of
those ports is never killed — the script stops and names it instead.

It preflights the two infra deps (starts Postgres — `brew services` on macOS, `systemctl` on
Linux, via `lib_platform.sh` — and `ollama serve` if down, leaving a *pre-existing* one
running on exit), warns about missing model files, builds once, then launches all four
services **directly as compiled binaries** (each tracked PID is the server → one Ctrl-C tears
the whole thing down cleanly). Per-service logs stream to `local_dev/logs/<svc>.log`
(gitignored). The simplest LAN path is **`./local_dev/serve.sh`** (cert + CA-trust +
`setup_hostname.sh` + `run_stack --lan` in one shot; `--check` reports status).

Two non-obvious things the script encodes — **preserve them if you touch it**:
- **CWD per service** (for `dotenvy` + relative model/blob paths): backend runs from
  `hushai-backend/` (loads `hushai-backend/.env`, `BLOB_DIR=./data`); worker/rag/viewer run
  from the **repo root** (load root `.env`; `models/*` + blobs are root-relative).
- **`DYLD_FALLBACK_LIBRARY_PATH=target/<profile>/deps`** for the sherpa-linked binaries
  (worker, rag) — set via bash `export` and `exec`'d **directly**, NOT through `env`/any
  `/usr/bin` shim (SIP strips `DYLD_*` when exec'ing a protected binary, and the worker then
  dies with `@rpath/libonnxruntime.1.17.1.dylib … no LC_RPATH's found`). Same requirement as
  the launchd worker plist / the manual `cargo run` path. On Linux the equivalent var is
  `LD_LIBRARY_PATH` (`launch()` picks the right one via `lib_platform.sh`'s `hushai_lib_path_var`).

The Android client can't be containerized/auto-spawned (it needs a physically connected USB
phone), so `--with-android` is best-effort: it chains `run_hushai_app.sh` only if `adb` sees
an authorized device, else warns and leaves the rest of the stack up.

### Manual (one terminal per service)

```bash
ollama serve &                                   # localhost:11434 (worker embeddings + rag LLM)
cd hushai-backend && SQLX_OFFLINE=true cargo run                       # backend  -> :8080
cd <root> && SQLX_OFFLINE=true cargo run -p hushai-worker              # worker   -> drains, then polls
cd <root> && SQLX_OFFLINE=true cargo run -p hushai-rag                 # rag      -> :8090
cd <root> && SQLX_OFFLINE=true cargo run -p hushai-viewer              # viewer   -> 127.0.0.1:8070
cd <root> && SQLX_OFFLINE=true cargo run -p hushai-advisor             # advisor  -> :8095
./local_dev/run_hushai_app.sh --duration 120                          # Android over USB (adb reverse)

curl -s localhost:8090/v1/rag/query -H 'content-type: application/json' \
  -d '{"query":"what did people say about the cameras?"}' | jq
```

Config is env-driven; root `.env` (gitignored) feeds worker+rag, `hushai-backend/.env` feeds
the backend. Dev token: `dev-secret-token`. `OLLAMA_BASE_URL` is the shared default for every
Ollama call; set `EMBED_OLLAMA_BASE_URL` (worker + rag query embedding) and/or
`LLM_OLLAMA_BASE_URL` (rag answer generation) to route them at separate instances under load.

### Viewer HLS gotchas (non-obvious — preserve if you touch `remux.rs` / `ui/js/app.js`)

- **One PTS clock for video + alt-audio.** Each ~2s blob is remuxed to its own MPEG-TS, so
  each restarts at PTS 0; serving separate video + alt-audio renditions that way stalls hls.js
  after the first audio segment (`bufferStalledError`). Fix (`remux.rs`): stamp every TS with
  `-output_ts_offset <capture_start_seconds>` so both land on one absolute wall-clock PTS.
- **Upright video.** Android stamps an MP4 rotation matrix, but `-c copy` to MPEG-TS drops it →
  sideways playback. `remux.rs` `ffprobe`s each video's display-matrix rotation and, when
  non-zero, re-encodes UPRIGHT with libx264 (ffmpeg autorotate); 0°/matrix-less clips keep the
  fast `-c copy` path. Toggle `VIEWER_UPRIGHT_REENCODE`; purge `cache/ts/` once on deploy. The
  Android app can also bake pixels at the source (`Settings.uprightBake`).
- **Load ONE coverage run per HLS window** (`ui/js/app.js` `computeWindow`): a window spanning
  a coverage **gap** or an **audio-only** stretch desyncs the alt-audio rendition
  (`fragParsingError`, frozen picture). `computeWindow` clamps the master to the single
  continuous coverage span containing the playhead — don't widen it back to the full range.
- **Verify with the REAL Google Chrome** (puppeteer-core), not open-source Chromium — Chromium
  lacks the H.264/AAC codecs the HLS remux plays. See [`hushai-viewer/e2e/`](hushai-viewer/e2e/).

## Data model & migrations

Migrations live in `hushai-backend/migrations/` and **auto-apply** on backend/worker startup
via `sqlx::migrate!`. Full one-line index: [`hushai-backend/migrations/README.md`](hushai-backend/migrations/README.md).
Current head: **`0024_entity_profiles`** (24 migrations, `0001`→`0024`).

### `transcript_sentences` storage (post-0003 — read before touching RAG/worker writes)

The chunk store is **RANGE-partitioned by `created_at` (monthly)** with a `DEFAULT` catch-all.
Parent-level indexes propagate to every partition:
- `…_embedding_hnsw` — HNSW `vector_cosine_ops` (the ANN index; per partition).
- `…_device_time_idx` — `(device_id, start_unix_nanos)`. **`device_id` is denormalized onto the
  table** so RAG filters sit on the same table as the HNSW index (no JOIN).
- `…_segment_id_idx` — makes the worker's idempotent delete-by-segment an index scan.
- Also denormalized (post-0006): **`speaker_id` (text)** backed by `…_speaker_time_idx`; and
  the `sentiment`/`emotion` columns (now written).

Rules:
- **Writes** (`hushai-worker/src/process.rs::write_transcript`) set `device_id` and use the
  batched multi-row INSERT (11 binds/row: + `sentiment`, `emotion`, `speaker_id`). `created_at`
  defaults to `now()` — don't set it. Speaker match/mint runs **inside this same txn** (global
  `pg_advisory_xact_lock` → prior-assignment read BEFORE the transcript DELETE → `SET LOCAL`
  HNSW GUCs + k-NN vote over `speaker_segments` + mint-guard hysteresis). `assign_speaker`
  returns `Option<Uuid>` (None ⇒ `speaker_id` NULL: refused/low-quality).
- **Retrieval** (`hushai-rag/src/retrieve.rs::nearest`) runs in a txn that `SET LOCAL`s
  `hnsw.iterative_scan='strict_order'` + `hnsw.ef_search` + `statement_timeout`. Filter on
  `ts.device_id` / `ts.start_unix_nanos` / `ts.speaker_id` (local columns), never via a JOIN. A
  speaker filter binds stringified uuids as `text[]` (`= ANY($::text[])`); an empty array
  matches nothing (the unknown-name contract). `list_by_speaker` is the non-semantic exhaustive
  path.
- **Partitions:** worker calls `ensure_transcript_partitions(3)` at startup (~3 months
  headroom); for long uptimes schedule `local_dev/partition_maintenance.sh` (launchd plist) or
  pg_cron (`partition_maintenance.pg_cron.sql`) to call it monthly. Retention is
  `drop_transcript_partitions_before(cutoff)`, 12-month default (`RETAIN_MONTHS`, `0` = never;
  *dropping a partition is irreversible* — rehearse with `DRY_RUN=1`).
- New segments are queued for processing **at ingest** (status row written in the segment txn
  in `hushai-backend/src/db.rs`) and a `pg_notify('hushai_segment_ingested', …)` wakes the
  worker; a poll loop is the backstop.

## Subsystem reference

Each subsystem has a dedicated doc/README; the notes below are the orientation + the invariants
you must not break.

### Worker pipeline & parallelism → [`docs/worker-parallelism-and-scaling.md`](docs/worker-parallelism-and-scaling.md)

The worker drains TWO independent SKIP-LOCKED queues in parallel; both fan out.
- **Audio:** `WORKER_CONCURRENCY` (default 2) `worker_loop`s. Whisper is genuinely parallel —
  `Transcriber` shares one `Arc<WhisperContext>` and calls `create_state()` per call on
  `spawn_blocking` (no ASR mutex). The TitaNet speaker embedder is `Arc<Mutex<…>>` (light).
- **Vision:** `VISION_CONCURRENCY` (default 2) `vision_worker_loop`s sharing ONE `Arc`-cloned
  `VisionModels` (`ort::Session` is `Send+Sync`; no duplication). Each *segment* is processed
  start-to-finish on one loop (intra-segment frame ordering preserved).
- **CPU thread budget (load-bearing):** `run()` derives `asr_n_threads()`/`ort_intra_op_threads()`
  = `clamp(cores / (audio+vision loops), 1, cores)` so N parallel loops don't oversubscribe.
  Override with `ASR_THREADS`/`ORT_INTRA_THREADS` (`0` = auto).
- **Invariants:** the speaker + face match/mint `pg_advisory_xact_lock`s stay **GLOBAL**
  (cross-device identity) — never shard per device. The "worker 0 only" speaker auto-merge lives
  solely in the audio loop. `claim_one*` SKIP LOCKED makes fan-out safe across loops AND across
  worker **processes/hosts** — a 2nd worker against the same DB scales out for free. CPU Whisper
  is the ceiling; ~30 cameras needs GPU/Metal whisper and/or a 2nd worker host.

### Worker efficiency gates (read before touching `db.rs::persist_segment`, `process.rs`, `vision/write.rs`, or the `lib.rs` loops)

Cut wasted compute + keep the device out of overload. All default ON with conservative,
**uncalibrated** thresholds + a kill switch each. The durable queue still guarantees nothing is
dropped — **media is ALWAYS stored regardless of gating.**

- **`skipped` status (migration 0022)** — both lane status tables are
  `pending | processing | done | error | skipped`. `skipped` is TERMINAL (a content gate decided
  there's nothing to infer); `skip_reason` names the decider (`*_hint` = ingest gate from device
  hints; `*_gate` = worker backstop). Never claimable and invisible to
  `reconcile_missing_speaker_segments` (matches `done` only), so skips are never resurrected. To
  re-evaluate after a calibration change: `UPDATE …_status SET status='pending', attempts=0,
  skip_reason=NULL, claimed_at=NULL, updated_at=now() WHERE status='skipped';` then NOTIFY.
- **Ingest hint gate** (backend `hints.rs` + `db.rs::persist_segment`) — the Android app attaches
  raw per-segment measurements as contract-§8 attrs; the backend owns ALL thresholds
  (`INGEST_HINT_GATE_ENABLED`, `INGEST_AUDIO_RMS_FLOOR`, `INGEST_MOTION_THRESHOLD`). Below-floor
  lanes are INSERTED pre-terminated `skipped`. **Fails open** on absent/malformed/unversioned
  hints; branches only on hint PRESENCE, never `source_kind`. `INGEST_HINT_AUDIT_PCT` (2%) of
  would-be skips are enqueued for the worker to cross-check (`audit_verdict`), surfaced on the
  dashboard.
- **Skip-silent (audio)** — `process::process_segment`, after `extract_pcm`: a free `vad::rms`
  floor, else Silero VAD `speech_secs` vs `AUDIO_SILENCE_MIN_SPEECH_SECS`. A silent segment
  writes NOTHING (no empty transcript, no reject tombstone). **Fails OPEN** (VAD error ⇒ ASR).
- **Skip-static (vision)** — `vision::write`, before the model loop: cross-segment motion via a
  32×32 mean-subtracted-MSE fingerprint cached in-memory per camera. With
  `WORKER_GATE_ONE_FRAME_PROBE=true` it decodes one mid-segment frame first, paying full decode
  only after motion confirms. `distance <= VISION_MOTION_THRESHOLD` ⇒ skip all inference. **Set
  `VISION_MOTION_SKIP_ENABLED=false` for any full reprocess/calibration run** (the per-camera
  baseline is meaningless replaying old segments out of order).
- **Load governor** (`governor.rs`) — samples the audio backlog-lag trend (+ optionally OS load)
  and publishes Normal/Elevated/Saturated with hysteresis (`LOAD_*`). At Saturated the expensive
  vision lane pauses so audio keeps up; at Elevated an inter-segment cooldown paces. It only
  paces — never reorders or drops; strict-oldest-first ordering is unchanged.

### Speaker identity (durable invariants; overhaul history in [`CHANGELOG.md`](CHANGELOG.md))

- **Embed VAD-cleaned speech, not the raw whisper slice.** `speaker.rs`/`vad.rs` run Silero VAD
  (in the already-linked sherpa lib — no extra crate, just `VAD_MODEL_PATH`) and embed the
  concatenated speech-only PCM, so background static no longer fragments one person into many
  "unknown speaker" rows. The voiceprint is embedded over a segment PLUS its contiguous
  predecessors (`select_window`) since ~2s clips rarely have enough speech alone.
- **Match-or-mint = multi-vector k-NN vote** over `speaker_segments` raw embeddings (centroid is
  the cold-start fallback) with **mint-guard hysteresis** (`SPEAKER_MATCH_THRESHOLD` <
  gray-zone < `SPEAKER_MINT_DISTANCE_FLOOR`) and a **quality gate** (Mint/AttachOnly/Reject) — a
  NEW identity mints only from clean, high-SNR, long-enough audio; marginal audio attaches or
  stays NULL, never duplicates. The centroid **self-heals** from recent `quality='clean'` rows.
- **Reject tombstone:** a processed-but-no-voice segment writes a `quality='reject'` row (NULL
  speaker/embedding) so restart backfill won't re-queue it forever; matching/clustering/centroid
  all ignore tombstones so a later windowed run can still attribute it.
- **Admin/heal:** `speakers.rs` clusters at the raw-embedding level (per-segment LATERAL k-NN
  over the partitioned table + union-find); endpoints `recluster`, `recluster-deep`,
  `duplicates`, `merge-group`, `unattributed`(+`/name`). The worker runs a rate-limited
  going-forward `auto_merge_recent` (worker 0 only, recent speakers only — never mass-collapses
  the backlog). **All `SPEAKER_*`/`VAD_*` thresholds are uncalibrated guesses** — see Known gaps.
- **Naming is a retro trigger** (2026-07): `rename_speaker` / `set_speaker_owner` (and the ops
  route `POST /v1/speakers/{id}/retro-attach`) run `retro_attach_pass` — attach NULL-speaker
  history on MULTI-VECTOR evidence (>=2 distinct raw neighbors each within the existing 0.5
  match distance; strictly tighter than the online gray-zone attach) + fold ANONYMOUS duplicate
  ids at the 0.15 auto-heal tightness (never a named<->named fold). The worker repeats the attach
  going-forward (`auto_attach_unattributed_recent`, beside auto-merge). Attached rows keep
  `quality='marginal'` so they never feed the clean-only centroid. Tests: `tests/retro_attach.rs`.

### Entity profiles ("running memory", migration 0024)

- `hushai-backend/src/profiles.rs`: one polymorphic `entity_profiles` row per person/speaker —
  an append-only observation log (one line per coalesced visit/conversation) folded
  INCREMENTALLY from the already-sessionized `events` table. Deterministic (no LLM in
  accumulation); the RAG narrates at chat time (`is_profile_query` → "tell me about <name>",
  the reflection-agent precedent). Anonymous identities accumulate too; naming just records
  "[date] identified as <name>" (profiles are keyed by id); merges fold via `merge_in_tx`
  hooks in `persons::merge_person` / `speakers::merge_speaker` / `collapse_cluster`.
- Drivers: worker drain-time pass (`PROFILES_*` knobs, worker 0) + RAG chat-time
  `refresh_subject` (`PROFILE_CHAT_REFRESH`, grace `PROFILE_GRACE_SECS` — eval pins 0).
  Watermark = `events.updated_at` wall clock with a settle grace >= 2x the 30s session bucket;
  DERIVED/rebuildable data (deleting a row only forgets the narrative).
- Known gap (documented): merges don't repoint `events.subject_id`, so loser events not yet
  consumed at merge time never fold in (bounded to the grace window).

### Conversation threading (migration 0025)

- **What it is:** persisted conversations — `conversations` catalog + denormalized
  `transcript_sentences.conversation_id`/`turn_index` — assigned by a batch threader
  (`hushai-backend/src/threading.rs` pure core + `conversations.rs` orchestration, driven by
  worker 0 every `THREADER_INTERVAL_SECS`, checked BEFORE claiming so load can't starve it).
  Industry-standard conversation disentanglement: silence-gap blocks (`CONVERSATION_GAP_SECS`,
  the shared truth with rag/profiles), then within-block speaker-pair graphs (reply-shaped
  adjacency + topic affinity over the 1024-d sentence embeddings) separate two concurrent
  group conversations on ONE mic. Split gate is conservative: size floor, temporal interleave
  required (`THREADER_TOPIC_ONLY_SPLIT=false` — sequential topic drift never splits), low
  cross-group similarity.
- **Determinism contract:** the pure core has total tie-breaks and injected id-minting; same
  input + same `config_hash(ThreaderCfg)` ⇒ byte-identical output (the eval lineage gate).
  All `THREADER_*`/`CONVO_*` knobs are in the eval manifest KNOB_PREFIXES.
- **Mutability contract (0025 header is normative):** `open` conversations are provisional
  (revisable while in the batch window); `closed` are frozen except append-only late-attach of
  reprocessed rows inside their span; `conversation_id NULL` = unthreaded → every consumer
  falls back to the query-time gap heuristic (dual-path, no backfill required). Threading
  windows derive from the BATCH'S CAPTURE TIMES, never the wall clock — backlog uploads (an
  offline phone's store-and-forward day, eval fixtures pinned in the past) thread in their own
  capture-time context. Pre-feature history stays NULL until `thread_backfill`
  (`THREADER_BACKFILL_ON_START` one-shot).
- **Cross-device = LINK, never merge** (`link_group_id`): overlapping wall-clock + shared
  speaker ⇒ same link group; merging would interleave duplicate ASR text of the same audio.
- **Lifecycle events:** close (gap + `CONVO_CLOSE_GRACE_SECS`) emits ONE `conversation` event
  (`dedup_key='convo:<id>'`) with participants metadata — feeds alerts/feed/profiles later.
- **RAG consumption:** grounded hits expand to conversation neighborhoods
  (`retrieve::expand_to_conversations`, `RAG_EXPAND_*` knobs + kill switch), but only after
  the relative-margin prune (`retrieve::prune_rel_margin`, `RAG_PRUNE_REL_MARGIN`, default
  0.25): hits with `distance > best + margin` are dropped so one marginal hit from an
  unrelated conversation — it can sit just under the absolute `RAG_DISTANCE_THRESHOLD` —
  never drags that whole conversation into the prompt. The prompt then
  renders per-conversation sections with a never-combine instruction
  (`llm::build_grouped_prompt` — enrich the FLAT list first, then regroup, or unnamed-speaker
  ordinals collide across groups). `take_latest_conversation` stops the backscan when the
  persisted conversation id CHANGES (two back-to-back conversations < gap apart no longer
  glue); `group_conversations` buckets threaded rows by id and gap-splits only the NULL
  remainder. New intent `is_participants_conversation_query` ("what did X and Y talk about")
  → `conversations WHERE speaker_ids @> …`. Voice recency questions anchor to the asking
  phone's device (`caller.device_id`) when no explicit filter — the group-A-not-group-B
  guarantee for voice. Endpoints: `GET /v1/rag/conversations[/{id}]`.
- **Honest limits (do not promise 100%):** same-topic interleaved groups on one mono mic are
  information-theoretically inseparable (no spatial audio); acoustically overlapped 2s
  segments arrive with `speaker_id NULL` (the multi-speaker refusal) and topic-attach as
  orphans — safe degradation, never a speaker misattribution. Device separation and ≥gap
  separation are absolute; disjoint-speaker-set separation needs the speaker lane to actually
  separate the voices.

### Gotham entity graph (migrations 0028–0030, Wave 1 / Pillar G1) — spec `Gotham.md`

- **What it is:** the intelligence layer's materialized inter-entity relationships — the links
  the perception pipeline perceives but never joins. `entity_edges` (0028) carries five edge
  types: `co_present` (overlapping visits, same device), `conversed_with` (speaker pairs from
  closed conversations), `arrived_with_vehicle` (person→plate temporal correlation),
  `visits_place` (entity→device), and the review-queued voice↔face binding
  `same_identity_candidate`. Nodes are the EXISTING catalogs (persons/speakers/plates/devices) —
  no node table; endpoints are `(node_type text, node_id text)` with NO FK (the `events.subject_id`
  contract). `graph_state` is the watermark singleton.
- **Producer:** `hushai-backend/src/graph.rs` (pure deterministic cores — canonical undirected
  ordering, co-presence pairing, binding Jaccard, evidence merge, journey stitch, anomaly
  predicates, `GraphCfg`+`config_hash`, unit-tested) + `graph_pass.rs` (the `profiles.rs` sibling:
  advisory-locked `GRAPH_LOCK_KEY`, watermark drain over settled `events` + closed
  `conversations`, idempotent edge upserts). Driven by **worker 0** at drain time
  (`GRAPH_*` knobs, `graph_interval_secs`), the exact profiles-driver idiom. Merge folds via
  `graph_pass::merge_in_tx` beside the `profiles::merge_in_tx` hooks in
  `persons`/`speakers`/`plates` merges; hard-delete cascades via `delete_entity_in_tx`.
- **Voice↔face binding (§1.4):** deterministic trials over closed conversations; NEVER
  auto-merges. An edge surfaces to the review queue (`status='candidate'`) only past all three
  gates (min sessions, min Jaccard, margin over runner-up — defeats the always-together
  confound); user confirm/reject via `/v1/graph/bindings/{id}/{confirm,reject}` (audit-logged,
  rejection sticky). `confirmed` is the only state consumers may union history across
  (`bound_person_for_speaker`). The owner seed idempotently confirms the `is_owner`↔`is_owner`
  edge. **Catalogs never merge — 192-d and 512-d spaces don't mix; the edge IS the identity.**
- **Determinism:** integer/ratio math, no LLM anywhere in materialization; watermarks are WALL
  clock, "now" is the DB clock (pinned-capture fixtures still settle); confidences rounded to 4
  decimals; `config_hash` (SHA-256 hex[..8] of the ★ knobs) stamps every row — a knob change is a
  new eval lineage. DERIVED/rebuildable: `POST /v1/graph/rebuild` truncates + refolds byte-stable.
- **API (`graph_api.rs`, `/v1/graph/*`, bearer-authed, proxied via viewer `is_backend_path`):**
  entity page, timeline, edges, neighbors (bounded recursive CTE, hops ≤ 3), path (≤ 4), binding
  queue + confirm/reject, rebuild. `journeys`/`digests` endpoints ship their table contract now;
  their producers (`patterns.rs` baselines/anomalies/digests = 0029/Wave 2; journey stitcher =
  0030/Wave 4) are later waves.
- **Wave-1 scope note:** co-presence is batch-local (the `profiles::co_present` precedent — a rare
  batch split costs one observation, converges as events drain); binding accumulates
  `together`+`speaker_only` (person_only is a documented follow-up). Journeys (0030) NOT populated
  yet (Wave 4).
- **Wave-2 / G2 (baselines + anomalies):** `hushai-backend/src/patterns.rs` is the "patterns"
  producer, called inside the `graph_pass` transaction for every subject touched this pass:
  recompute the `entity_baselines` row (168 hour-of-week histogram, dwell p50/p90, device/companion
  top-K — deterministic pure math in `graph.rs`) and emit `off_schedule_presence` anomalies.
  Anomalies are ordinary `events` rows (`event_type='pattern_anomaly'`, dedup by
  `anom:<kind>:<subject>:<civil-day>`, `ON CONFLICT DO NOTHING`) → ride the shipped A-pillar; the
  worker-0 driver alert-evaluates the fresh anomaly ids AFTER the pass commits (the evaluator lives
  in the worker crate, so the backend pass only emits + reports ids in `GraphStats.anomaly_event_ids`).
  **off_schedule is AS-OF**: each visit judged against the subject's STRICTLY-EARLIER visits (a
  running prior histogram), so first appearances (incl. the enrollment clip an hour before a case)
  never fire — only a later rhythm violation does. The drain queries EXCLUDE
  `pattern_anomaly`/`gotham_briefing` so the graph never folds its own output. Eval: `expect_anomaly`
  /`expect_no_anomaly`/`expect_baseline` on the graph modality; F4/F5/F6 gate under `d4acc862`.
  **ALL FOUR predicates are now wired.** `off_schedule_presence` is per-subject AS-OF in
  `recompute_and_flag`; the three EDGE predicates — `first_time_pairing`, `new_vehicle_for_person`,
  `unknown_person_cluster` — are judged in `patterns::flag_edge_anomalies` over the 0→1 edge
  transitions + unknown clusters a pass observes (collected at drain time in `graph_pass::EdgeTransitions`;
  `upsert_edge` returns the prior `observation_count`), emitted AFTER the baseline recompute so endpoint
  maturity (read from the `entity_baselines` table) is available for the rebuild AND the incremental
  worker. `first_time_pairing` is emitted per-endpoint (assignment-invariant); `unknown_person_cluster`
  is DEVICE-keyed (`subject_type='device'`, NULL `subject_id`). NO new `GraphCfg` field was added, so
  `config_hash` stays `d4acc862` and F1–F7 baselines are untouched. Proof: `tests/graph_db.rs`
  (deterministic, all three) + `anomaly_first_pairing` (train fixture, live gate ×2).
- **Wave-2 / G2 daily digest (PR5, Phase E):** `patterns::build_and_upsert_digest` materializes one
  civil day's `daily_digests` row — deterministic `sections` jsonb (`new_entities`, `top_visitors`,
  `anomalies`, `conversations`, `first_time_pairings`, `journeys` + a `counts` sub-object) and a
  template `rendered_text`, **NO LLM at write time** (the `analytics::render_digest` discipline; the
  RAG/G3 layer narrates at read time). Same no-self-fold exclusion + capture-anchored day window
  `[D*86400-tz, (D+1)*86400-tz)`. Two triggers: `POST /v1/graph/digests/{date}` (force a pinned date;
  the eval + operator path) and the worker-0 wall-clock driver (`GRAPH_DIGEST_HOUR_LOCAL` default 21,
  **NON-hashed** — a report schedule, not a stored derivation, so NOT in `GraphCfg`/`config_hash` and
  deliberately NOT in `eval.env`). Eval `expect_briefing` (`BriefingGt`: `counts` + `mentions` vs the
  structured `sections`, never prose); F7 `briefing_daily` gates ×2 under `d4acc862`. `rebuild` does
  NOT truncate `daily_digests` (date-partitioned reports, not fold state).

### Chat correctness (2026-07 overhaul — the "executive chat" fixes)

- **Visit coalescing:** `presence.rs` counts VISITS (gap-coalesced continuous appearances,
  `PRESENCE_VISIT_GAP_SECS`), never raw per-2s-segment sightings (the "seen 62 times" bug). The
  citation list stays per-segment (video deep-links) — visit count != citation count by design.
- **Distinct-people counts:** `is_people_count_query` → deterministic roster enumeration
  (`render_people_count`), never the single-person frequency rollup.
- **Footage stats:** `is_footage_stats_query` → `hushai-rag/src/stats.rs` SUM over `segments`
  with window clamping; deterministic pre-route + precomputed answer (the LLM never narrates
  the numbers).
- **Window summary:** `is_window_summary_query` ("what have we spoken about today") →
  `retrieve::conversations_in_window` (per-device gap grouping) + `answer_window_summary`;
  the bare form defaults to the local TODAY.
- **Session hygiene:** the viewer session pointer is per-tab (`sessionStorage`) + a 60-min
  auto-restore staleness guard; server `load_history` age-filters the LLM-visible turns
  (`RAG_CHAT_HISTORY_MAX_AGE_SECS`) BEFORE the condenser; the voice client honors spoken
  "new chat"/"start over" (`VoiceSession.isResetCommand`) without sending it to the server.
- **Natural-language windows** ("last 10 minutes", "today") now apply to the People/Objects/
  Plates/Events arms too (`timeparse::window_in_query`, incl. a relative last/past-N parser).

### Vision: faces / objects / ALPR → [`docs/vision-image-cleanup-and-alpr.md`](docs/vision-image-cleanup-and-alpr.md), [`docs/perception-hardening.md`](docs/perception-hardening.md)

The vision path processes VIDEO/MUXED (`media_type IN (2,3)`) into cross-device catalogs, the
visual siblings of the speaker system.
- **Faces** (`vision/{detect,detect_scrfd,face_embed,face_match,write}.rs`): SCRFD (default,
  `FACE_DETECTOR_KIND=scrfd`) or YuNet detect → similarity-transform align → ArcFace 512-d embed
  → `face_match.rs` (a near-verbatim mirror of `speaker_match.rs`: global advisory lock with a
  DIFFERENT key, k-NN vote + mint-guard, self-healing centroid) into `persons`/`person_segments`
  (migration 0009). A **detect → expand-crop → super-res/restore/deskew → recognize** cleanup
  cascade (`enhance.rs`) is *recover-then-embed instead of reject*; restored best-shot crops go to
  `<BLOB_DIR>/face_crops/` (migration 0012).
- **Objects** (`vision/objects.rs`, optional lane): RF-DETR region boxes + per-region & whole-frame
  OpenCLIP image embeddings into `scene_objects` (its OWN 512-d CLIP-space HNSW — **never** shared
  with the ArcFace face index; a shared index returns garbage NN). Class ids map via the canonical
  **COCO-91** layout. RAG queries it via a CLIP **text** tower.
- **ALPR** (`vision/plates/`, optional): per vehicle ROI → plate detect → homography rectify →
  OCR (fast-plate-ocr, NOT tesseract) → normalize + temporal vote → `plate_match.rs` (match-or-mint
  by normalized STRING, fuzzy trigram/edit-distance — NOT k-NN). Migration 0013.
- **ORT coexistence (load-bearing):** sherpa-rs statically bundles ONNX Runtime **1.17.1**;
  `ort` = `2.0.0-rc.9` targets **1.20.0**, so `ort` uses `load-dynamic` (no link-time ORT) and
  `dlopen`s its OWN 1.20 dylib via `ORT_DYLIB_PATH` — a second image alongside sherpa's, coexisting
  under macOS two-level namespaces (proven by `tests/ort_coexistence.rs`). Every vision lane
  self-disables when unprovisioned; faces/objects/plates degrade independently. All models
  gitignored, operator-fetched (`local_dev/provision_vision.sh` + `fetch_*`/`export_*`).

### RAG service & agents → [`hushai-rag/README.md`](hushai-rag/README.md)

`POST /v1/rag/query` (single-shot) and `POST /v1/rag/chat` (multi-turn, SSE token-streaming;
DB-backed `chat_sessions`/`chat_messages`, migration 0008) answer over pgvector retrieval with a
`qwen2.5:7b` LLM (`RAG_LLM_MODEL`). Citations deep-link the viewer timeline.
- **Agents** = a code registry (`agents.rs`): **7 registry entries** — `auto` (the synthetic
  router) + 6 concrete `AgentKind`s: **Grounded** (`recordings`), **Reflection**, **Objects**,
  **People**, **Plates**, **Events**. Each = persona + default retrieval scope; adding one =
  appending a struct.
- **Unified auto-routing:** the web/voice chat is ONE box bound to `auto`; per message the handler
  deterministically pre-routes some phrasings, else calls `llm::classify_agent` (a cheap
  qwen2.5:7b classification) and dispatches. The auto-router stays a classification prompt by
  DESIGN (one cheap call, no loop) — NOT because the runtime lacks tools. `rig = "0.37"` resolves
  **`rig-core 0.38.2`**, which ships the full tool stack (`Tool` trait, `ToolSet`, agent
  `.tool(..)`/`.multi_turn(n)`, `PromptHook`), wired to Ollama's native `tools` JSON. That
  tool-calling machinery is what the **Gotham "Detective" agent** (`hushai-rag/src/gotham/`, G3)
  uses for its plan→act→observe loop; see `Gotham.md` Part 2 §2.1. Pin `rig` EXACTLY (the facade
  floats rig-core minors — a lockfile refresh would silently shift the agent-loop semantics).
- **Deterministic-first answers** where a wrong number matters: identity ("what's my name"),
  recency ("what did we last discuss"), speaker roster, presence counts (`presence.rs`),
  co-occurrence, camera-clarify — all answered before/around the LLM so it narrates computed
  figures, not top-k guesses. Times render via `humanize.rs` from the client's `tz_offset_secs`.
- **Owner resolution precedence: DB `is_owner` mark → `OWNER_*` env** — the "This is me" tap
  (`POST /v1/speakers/{id}/owner`, migration 0023) makes reflection/co-occurrence/identity work
  with no env config. **Context layer** (`context.rs`, env-gated `RAG_CONTEXT_*`) prepends a
  reliable-facts briefing + annotates transcript passages with same-segment vision.
- **TTS:** `POST /v1/tts` → Kokoro-82M via the `sherpa-onnx` crate → `audio/wav` for Android.

### Viewer → [`hushai-viewer/README.md`](hushai-viewer/README.md)

The unified webapp stitches ~2s segments into one scrubbable HLS timeline (lazy per-segment
ffmpeg remux cached by content hash), with a chat-over-recordings panel, a **Detections** overlay
(bbox synced to playback), **AI processing-status ribbons**, a **System dashboard**
(`/api/dashboard` fans out DB queries + health probes server-side), an **Events/Alerts** center,
a **Files** device/footage-management page, and **browser capture** (records local camera/mic as
H.264/AAC fMP4 and POSTs through `/api/capture` → backend, so the web app is a capture source like
Android). It reverse-proxies `/v1/*`: most paths → hushai-rag, but the admin surface
(`/v1/speakers|persons|plates|devices|events|alert-rules|watchlist|audit*`) → hushai-backend.

### VSaaS: events → alerts → watchlists → push → [`docs/feature-parity-roadmap.md`](docs/feature-parity-roadmap.md)

The proactive layer (Verkada/Rhombus-style parity). End-to-end: **detect → sessionized event →
rule match + cooldown → outbox → in-app feed + web Events UI → outbound webhook / Android push.**
- **Events** (`events` table, migration 0014 — PLAIN, not partitioned, so it can carry a real
  `UNIQUE(dedup_key)`): the worker's `events_producer.rs` materializes sessionized events post-commit
  (`known_person`/`unknown_person`, `plate_of_interest`/`plate_seen`, `object_seen`, `speech`),
  one per distinct subject per `EVENTS_SESSION_BUCKET_SECS` bucket via `dedup_key`.
- **Alerts** (`alert_rules` + `alert_deliveries`, migrations 0014–0016): `alerts.rs::evaluate`
  matches an event (tz time-of-day window incl. midnight-wrap, day-of-week, severity floor,
  device/subject/watchlist filters), enforces cooldown, and fans channels out — idempotent via
  `ON CONFLICT (rule_id,event_id,channel)`. `delivery.rs` drains the webhook outbox (crash-safe
  lease + backoff, at-least-once with a stable idempotency key, optional HMAC signing; blocks
  cloud-metadata IPs always, LAN targets allowed by default).
- **Watchlists** (migration 0019): a "person/plate of interest" owns a managed `alert_rules` row,
  so the same evaluator fires — no worker change. Survives merges (`reconcile_merge`) + is
  self-healing (re-mints a deleted managed rule).
- **Android push** (`AlertNotifier.kt`, FCM-free): polls `/v1/events/feed` over the same LAN/USB
  connection and raises system notifications; dedup via a **persisted** server-time high-water
  mark (feed rows stay `pending` until acked).

### Cloud-native ops: metrics / logging / audit

- **Metrics** (`hushai-backend/src/observe.rs`, dependency-free Prometheus exporter shared by all
  four binaries): `/metrics` everywhere (worker on `WORKER_METRICS_ADDR`, default
  `127.0.0.1:9100`), `/healthz` everywhere, `/readyz` (DB ping) on backend/rag/viewer.
  **Cardinality guard (load-bearing):** never use device/client free text as a label — the
  registry never evicts; `source_kind` is collapsed to an allowlist.
- **Logging** (`hushai-backend/src/logging.rs`, shared): each crate's `init_tracing()` calls
  `logging::init("<service>", "<default RUST_LOG>")` — do NOT re-introduce per-crate
  `tracing_subscriber::fmt()`. `LOG_FORMAT=json` for Loki/ELK; `LOG_DIR` adds a daily-rotated
  file; per-request `request_id` correlation; a panic hook routes panics through `tracing::error!`.
- **Audit** (`audit_log`, migrations 0017/0018, append-only + UPDATE-blocking trigger): the viewer
  gateway (`proxy.rs::forward`) records every mutating request to a backend admin path with the
  real client IP + upstream status, plus auth + export events. Read via `GET /v1/audit`.
  **Known gap:** only actions THROUGH the viewer are audited; a direct backend-port call (device
  token) bypasses it.

## LAN security model

The stack is built to run on a shared WiFi LAN, not just trusted localhost. Three independent
planes — understand which control protects which surface before changing any of it. Runbook:
[`docs/friendly-url-linux.md`](docs/friendly-url-linux.md) / [`docs/friendly-url-windows.md`](docs/friendly-url-windows.md).

**1. Confidentiality in transit — native rustls TLS (all three services).** `hushai-backend/src/tls.rs`
(`serve`/`serve_with_connect_info`) replaces `axum::serve`. TLS is **opt-in by env**:
`[<PREFIX>]TLS_CERT_PATH` + `TLS_KEY_PATH` (prefixes `RAG_`/`VIEWER_`, falling back to bare names).
Unset ⇒ cleartext (the USB/adb + localhost dev path is untouched). Crypto provider is **aws-lc-rs**
(already vendored via sqlx), installed once; `axum-server` uses `tls-rustls-no-provider` so there's
no second `ring` provider. Certs: `local_dev/gen_certs.sh` → a 10-yr local CA + an 825-day IP-SAN
leaf in gitignored `local_dev/certs/`. **Clients trust the CA, not the leaf**, so a DHCP IP change
only needs a leaf re-mint.

**2. The browser/admin plane — the viewer is the admin panel, gated two ways (`hushai-viewer/src/auth.rs`).**
Locked by **IP allowlist + a password**, both required. IP filtering MUST live at the viewer (it
reverse-proxies `/v1/*`, so backend/rag only ever see `127.0.0.1`). Config: `VIEWER_ADMIN_IP_ALLOWLIST`
(empty ⇒ loopback-only via `VIEWER_ALLOW_LOOPBACK`), `VIEWER_ADMIN_PASSWORD`
(or `_HASH`; argon2), `VIEWER_SESSION_SECRET` (HMAC stateless cookie), `VIEWER_SESSION_TTL_SECS` (7d),
`VIEWER_COOKIE_SECURE`, `VIEWER_AUTH_DISABLED` (local escape hatch; IP gate still applies). The
friendly `https://hushai.local/` URL comes from `setup_hostname.sh` (Bonjour LocalHostName + a `pf`
443→8070 redirect to the **LAN IP**, not loopback) + `run_stack.sh --lan`.

**3. The data plane — per-device tokens + rag auth (over TLS).** Backend ingest auth
(`hushai-backend/src/auth.rs`) is a **`HashMap<token,label>`**: set `DEVICE_TOKENS`
(`label:token,…`) for per-device revocable tokens (drop an entry + restart to revoke); the single
`DEVICE_TOKEN` still works (the viewer proxy's `BACKEND_TOKEN` defaults to it). Comparison is
constant-time (`subtle`). Set `RAG_TOKEN` (it WARNs if unset). Onboard a camera:
`./local_dev/run_stack.sh --add-camera <name>` — full runbook [`docs/onboarding-a-camera.md`](docs/onboarding-a-camera.md).

## Testing → [`hushai-eval/RECURSIVE_TESTING.md`](hushai-eval/RECURSIVE_TESTING.md)

`hushai-eval` injects **known** clips into the **live** pipeline, waits for completion, scores vs
ground truth, and emits a verdict + exit code (0 pass · 1 regression · 2 inconclusive). It scores
both perception AND the RAG chat ANSWER (the `chat`/`rag` modality: `must_contain`/`expect_number`/
`expect_routed_agent`/`min_citations`/…). An `advisor` modality (staging split only:
`--fixtures staging`) scripts multi-turn consultations against hushai-advisor
(`expect_questions`/`expect_final_answer`/`expect_chapters_any|all`/`expect_substrings`;
`HUSHAI_ADVISOR_URL`/`ADVISOR_TOKEN`) — absent service or un-ingested corpus yields
INCONCLUSIVE, never FAIL. A `graph` modality (Gotham G1, DB-direct + deterministic like
perception) scores the materialized `entity_edges`: `expect_entity`/`expect_edge`/`expect_no_edge`,
assignment-invariant (assert by enrolled name/device_id, never a minted UUID). Because `graph_pass`
correlates cross-subject edges BATCH-LOCALLY, the harness doesn't observe the worker's incremental
fold — it waits for graph inputs to settle (`poll::wait_graph_inputs_settled`: conversations sealed
+ events committed) then triggers one authoritative `POST /v1/graph/rebuild` (whole-scenario single
batch → deterministic). F1–F3 are in `train` (Phase-C calibrated on the rig, gate ×2, then promoted alongside the full-suite re-baseline the `GRAPH_` config-hash change forced — all baselines now under config_hash `d4acc862`). It REFUSES to run against a non-`*_test` DB (it TRUNCATEs
result tables) — bring it up with `./local_dev/run_stack.sh --test-db` (determinism profile
`local_dev/eval.env`), then `cargo run -p hushai-eval -- run --tier {fast|full}`. A physical
camera-at-screen tier is `local_dev/physical_loopback.py`. Read the playbook before using the loop.

Unit + integration tests: `cargo test --workspace` (the DB-touching integration tests are
`DATABASE_URL`-gated; the vision tests are additionally model-gated and SKIP until provisioned).

## Build/run gotchas (non-obvious)

- **Android toolchain** is installed no-sudo in non-standard spots:
  `JAVA_HOME=/opt/homebrew/opt/openjdk@17`, `ANDROID_HOME=~/Library/Android/sdk`, **adb is NOT on
  PATH** (`$ANDROID_HOME/platform-tools/adb`). Build with the project's `./gradlew` (pinned 8.9) —
  the system `brew` Gradle (9.x) is too new for AGP 8.7.3.
- **Phone connection (default = USB, no shared network):** the test phone is a **Galaxy S8 =
  Android 9**. Drive the whole loop over the USB cable — control (adb), uploads (:8080), and
  voice-assistant RAG/TTS (:8090) — with `adb reverse tcp:8080` + `tcp:8090` + both URLs at
  `localhost`. `local_dev/run_hushai_app.sh` does all of this. Wireless fallback: `adb tcpip 5555`
  then `adb connect <phone-ip>:5555` (the S8 is too old for `adb pair`).
- **sqlx 0.9 + pgvector 0.4.2:** query metadata is committed under each crate's `.sqlx/` (backend's
  `db.rs` macros only — worker/rag/backend `speakers.rs` use runtime queries on purpose). Build with
  `SQLX_OFFLINE=true`. If you add/alter a backend `db.rs` `query!`, run `cd hushai-backend &&
  DATABASE_URL=… cargo sqlx prepare -- --lib` and commit `.sqlx/`. **Needs sqlx-cli 0.9** (install
  without `--locked` — sqlx 0.9 dropped its tracked lockfile). Embeddings bind as native
  `pgvector::Vector` (binary protocol); non-`'static` query strings wrap in `sqlx::AssertSqlSafe(...)`.
- **Android UI** is a single Compose screen (`ui/CaptureScreen.kt`) + screen-state toggles (no nav
  framework); all color/typography/shape comes from `HushaiTheme` (an Uber-style **light** scheme,
  system dark mode ignored on purpose). Don't hardcode `Color(...)` — add to `ui/theme/Color.kt`.

## Known gaps / TODO

Tracked in [`Issues/unfinished/`](Issues/unfinished/); the load-bearing ones:
- **Speaker/face/plate thresholds are uncalibrated** starting guesses (clean-room `say` voices /
  clean photos). Calibrate against real **noisy** captures — needs a LABELED real-capture set
  (person vs TV vs music vs HVAC); naive lowering makes a TV playing dialogue mint a spurious
  speaker. See `Issues/unfinished/speaker-dedup-calibration-followups.md`.
- **Live token revocation is restart-based** (`DEVICE_TOKENS` edit + restart). A DB-backed
  `device_tokens` table (`auth.rs` anticipates it) would allow hot revocation — not built.
- **Existing duplicate identity backlog is not auto-collapsed** (by design — "fix going forward
  only"). Surfaced under "Clean up voices" for manual merge; `recluster-deep` runs the catalog on
  demand. No scheduled deep-heal job is wired (opt-in).
- **Per-face emotion/activity** and **rolling-window ASR conversation grouping** are open
  (`Issues/unfinished/`), as is **phone-as-gateway** backend wakeword/STT.
- **Audit gap:** direct backend-port calls (device token) bypass the viewer-gateway audit log.
- **Android hardening:** crash-durable on-disk retry queue, Doze/battery, shared-proto golden-vector CI.

## Where to look

- **Per-component detail:** each crate's `README.md` (`hushai-backend`, `hushai-worker`, `hushai-rag`,
  `hushai-viewer`, `hushai-android`, `hushai-eval`, `hushai-loadtest`).
- **Dated history:** [`CHANGELOG.md`](CHANGELOG.md). **Human runbooks/design:** [`docs/`](docs/).
  **Migrations index:** [`hushai-backend/migrations/README.md`](hushai-backend/migrations/README.md).
- **Code-review conventions:** [`REVIEW.md`](REVIEW.md).
