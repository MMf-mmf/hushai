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
| `hushai-backend/` | Rust/Axum segment-ingest server (`POST /v1/segments`, `:8080`). Metadata in Postgres, media as content-addressed blobs under `{BLOB_DIR}/blobs/ab/cd/<sha256>`. Also the authenticated speaker catalog: `GET /v1/speakers`, `PATCH /v1/speakers/{id}` (name), `POST /v1/speakers/{id}/merge`, `POST /v1/speakers/recluster` (centroid) + `POST /v1/speakers/recluster-deep` (raw-embedding heal), `GET /v1/speakers/duplicates` (suggested duplicate groups), `POST /v1/speakers/merge-group` (one-tap group merge), `GET /v1/speakers/unattributed` + `POST /v1/speakers/unattributed/name` (cluster NULL-speaker audio into candidate voices and name one — mints + repoints), `GET /v1/speakers/{id}/sample-audio` + `GET /v1/speakers/unattributed/sample-audio?segment_id=` (segment-keyed clip so a still-unattributed candidate voice can be heard before naming) (`speakers.rs`, runtime sqlx — NOT the `.sqlx` macros). | ✅ built + verified |
| `hushai-worker/` | Drains stored segments (NOTIFY-driven, poll backstop) → whisper.cpp ASR → `mxbai-embed-large` (1024-dim) embeddings → `transcript_sentences` (batched insert, `device_id` denormalized). Also derives per-segment **sentiment** (`sentiment.rs`, lexical via `llama3.2:3b`) and **speaker identity** (`speaker.rs`+`vad.rs`+`speaker_match.rs`: real Silero **VAD** strips static/silence → TitaNet 192-d embedding → **multi-vector k-NN** match-or-mint with **mint-guard hysteresis** + **self-healing centroid**, online global advisory-locked into `speakers`/`speaker_segments`; worker also runs a going-forward **auto-merge** of near-certain duplicate voices). The ASR path claims **AUDIO/MUXED** (`media_type IN (1,3)`); a separate **vision path** (`vision/`, see "Vision pipeline" below) claims **VIDEO/MUXED** (`media_type IN (2,3)`) → YuNet+ArcFace face identity into `persons`/`person_segments` (ONNX via `ort` load-dynamic). On startup it **self-heals** by re-queueing `done` audio segments that have no `speaker_segments` row yet (`SPEAKER_BACKFILL_ON_START`, default on), so a window where the speaker stage was down can't permanently strand a voice. It also writes a **liveness heartbeat** (`spawn_heartbeat`, one `worker_heartbeat` row per process every `WORKER_HEARTBEAT_SECS`=10, deleted on clean shutdown) since it has no HTTP port — the viewer's System dashboard reads it to tell "idle" from "dead". | ✅ built + verified |
| `hushai-rag/` | Axum `:8090`. `POST /v1/rag/query`: pgvector NN over embeddings + a `qwen2.5:7b` answer via Rig (`RAG_LLM_MODEL`; upgraded from `llama3.2:3b`, which embellished the strict extractive attribution answers with sightings not in the sources — the worker's sentiment lane still uses `llama3.2:3b` independently), with optional **speaker attribution** (`filters.speaker_name`/`speaker_id` → text[] filter; `exhaustive:true` routes to non-semantic `list_by_speaker`; prompt prefixes each passage with the resolved speaker name **and a human-readable relative time** (e.g. "yesterday at 5:14 PM") and deliberately omits raw UUIDs/nanoseconds so the small model can't echo them as "ids"/"timestamps" — these display strings are computed once by `retrieve::enrich_for_display` via `humanize.rs` and ride along on each `Source` (`speaker_name`/`time_label`) for the prompt, the SSE/JSON citations, and persistence; an unnamed speaker renders as "someone we haven't identified yet" — `speakers.rs`/`humanize.rs`). `POST /v1/rag/chat`: **multi-turn, SSE token-streaming** grounded chat over recordings (`chat.rs`) — DB-backed conversations (`chat_sessions`/`chat_messages`, migration 0008; plain tables, NOT partitioned), retrieval re-anchored on the latest message + prior turns as LLM history, citations returned for video deep-linking; `GET /v1/rag/agents` + `GET /v1/rag/chat/sessions[/{id}/messages]`. **Agents** = a code registry (`agents.rs`): persona (the former hardcoded `llm.rs` preamble) + default retrieval scope, selected by `chat_sessions.agent_id` (text, no FK — agents live in code); adding one = appending a struct. Each agent has an `AgentKind`: `Grounded` (transcript retrieval), `Reflection`, `Objects` (open-vocab "when did I see a car" — CLIP-text NN over `scene_objects`, see "Vision pipeline"), or `People` (face attribution — `list_by_person`/`list_co_occurring_persons`/`list_recent_persons` over `person_segments`; routing in `routes::resolve_people_sources`, shared by the query + chat paths: explicit/mentioned name → that person's sightings; first-person "who was I with" (`is_co_occurrence_query`) → co-occurrence around the owner, declining with the owner-setup hint only when no owner is configured; **otherwise → the ROSTER of everyone seen, "who have you seen so far"** — a no-name question no longer needs an owner). **Unified auto-routing:** the web chat is now ONE box bound to a synthetic `auto` agent (`agents::AUTO_AGENT_ID`); per message the handler calls `llm::classify_agent` (a cheap qwen2.5:7b classification → `agents::parse_agent_label`, default `recordings`, with the last turn or two as context so clarification follow-ups route right) and dispatches to one of the 5 concrete kinds — no manual tab picker (rig has no tool-calling, so routing is a classification prompt). **Local time:** `ChatRequest`/`QueryRequest` carry `tz_offset_secs` (the browser's live UTC offset, sent by the viewer) which overrides `ANALYSIS_TZ_OFFSET_SECS` for all `humanize_time` rendering, so spoken times match the user's clock (the bug was UTC-only). **Camera clarification:** a deictic question ("who's in *this video*") with no camera scoped and >1 camera (`routes::is_deictic_video_query` + `camera_count`) returns the `CAMERA_CLARIFY` precomputed answer instead of silently answering across all cameras. The **`reflection`** agent (an introspective "how have my conversational skills/mood/social patterns been?" coach) does NOT do top-k retrieval — it runs `analytics::compute_digest` (`analytics.rs`): deterministic SQL rollups over ONE target speaker's whole history (talk-vs-listen balance, question rate, sentiment dist + weekly trend, top interlocutors, social rhythm — all by gap-grouping `start_unix_nanos` per device since there's no conversations table, and collapsing the segment-level sentiment/speaker denormalization), then the LLM only narrates the rendered digest (it computes NO numbers). Target-speaker precedence: request `speaker_id`/`speaker_name` → agent default → configured **owner** (`OWNER_SPEAKER_ID`/`OWNER_SPEAKER_NAME`; with neither set and no request filter it declines and asks you to name your voice). `/v1/rag/query` also takes an optional `agent_id` so the Android voice assistant can reach `reflection` (it routes introspective questions on-device via `ReflectionIntent`). Tune via `ANALYSIS_WINDOW_DAYS_DEFAULT`/`CONVERSATION_GAP_SECS`/`ANALYSIS_TZ_OFFSET_SECS`/`REFLECTION_LLM_MODEL` (a larger model is recommended for the digest→coaching synthesis). `POST /v1/tts`: local neural TTS (Kokoro-82M via the `sherpa-onnx` crate) of the answer → `audio/wav` for the Android client. Kokoro model is fetched (not committed) via `local_dev/fetch_tts_model.sh` → `models/kokoro-en-v0_19`; tune with `RAG_TTS_*` env. | ✅ built + **verified E2E in headless Chrome** |
| `hushai-viewer/` | The **unified webapp** (`127.0.0.1:8070`): a scrubbable HLS NVR **+ a chat-over-recordings panel beside it**. Stitches stored ~2s segments into one timeline (lazy per-segment ffmpeg remux to MPEG-TS cached by content hash, windowed VOD playlists with `PROGRAM-DATE-TIME`+`DISCONTINUITY`, Android audio as an HLS alt-audio rendition), hls.js + canvas scrub-bar. Also **reverse-proxies `/v1/*`** (`proxy.rs`: one browser origin, no CORS, server-side bearer, body **streamed unbuffered** so chat SSE flows): most paths → **hushai-rag** (`RAG_BASE_URL`/`RAG_TOKEN`), but `/v1/speakers*` is dispatched by path → **hushai-backend** (`BACKEND_BASE_URL`/`BACKEND_TOKEN`, the latter defaulting to `DEVICE_TOKEN`) since the speaker-admin surface lives there. **Browser capture (the viewer is also a capture *source*):** a **● Capture** topbar modal (`ui/js/capture/*`) records the local camera+mic via `MediaRecorder` as ~2s **H.264/AAC fMP4** segments (rotate-a-fresh-recorder per cut → keyframe-aligned), hand-builds the `hushai.v1.SegmentManifest` protobuf in vanilla JS (`container="fmp4"`, `media_type=MUXED`, `codec_init_data`=fMP4 init), and POSTs multipart `{manifest,body}` to `/api/capture/segments` → `proxy.rs::forward_capture` forwards to backend `POST /v1/segments` (bearer injected, 16 MiB body) — so the web app streams in exactly like Android (`source_kind="web_browser"`, `device_id="web-<uuid>"`) and is processed by the same transcription+vision lanes. H.264/AAC is mandatory (the HLS remux only plays that); Chrome/Edge/Safari only (Firefox MediaRecorder is WebM-only). Serves the chat UI (`ui/js/chat/*` + `ui/js/store.js`) — ONE **unified auto-routed chat** (no agent tabs; `workspace.js` mounts a single `ChatPane` bound to the `auto` agent and sends `tz_offset_secs`; `agent-picker.js` is unused) with a **camera-scope dropdown** (`filters.device_id`) + **New chat** — and a **⚙ Voices** settings modal (`ui/js/settings/voices.js`: list/name/merge speakers + play sample audio — including a **Play** on still-unattributed candidate voices via `GET /v1/speakers/unattributed/sample-audio?segment_id=` — the web twin of the Android Voices screen) and a **👤 People** modal (`ui/js/settings/people.js`: list/name/merge faces, showing each `GET /v1/persons/{id}/sample-face` crop — the web twin of the Android People screen). Chat citations deep-link the timeline via `store.js` → `app.js` `seekToCitation` (`player.js` untouched; `timeline.js`/`app.js` were extended for the **AI processing-status ribbons**, see below). The scrub bar also carries two thin **AI-status ribbons** under the coverage track (audio + vision lanes) showing how far the pipeline has processed each stretch — backed by `GET /api/devices/{id}/processing` (`processing.rs`). Also serves a **System dashboard** (`▦ System` nav link → `ui/dashboard.html` + `ui/js/dashboard/dashboard.js`, a separate scrolling page): cameras (connected/idle/offline by `devices.last_seen` recency) + every background process — the 4 services (backend/rag/viewer `/healthz`, backend `/readyz` for degraded; worker via the `worker_heartbeat` row, migration 0010), Postgres (`SELECT 1` + pool gauges), Ollama (`/api/tags`, optional), disk free-space — plus the transcription/vision work-queue stats. One aggregating endpoint `GET /api/dashboard` (`hushai-viewer/src/dashboard.rs`) fans out the DB queries + HTTP probes server-side (browser can't reach the localhost siblings); the page polls it every 6s. Vanilla ES modules, **no build step**. Reuses `hushai-backend` as a lib. | ✅ built + **verified E2E in headless Chrome** |
| `hushai-android/` | Native Android capture client (Kotlin). Camera2 + dual MediaCodec → ~2s segments → uploads; live preview, battery-saver, voice assistant, an **audio-only mode** (mic-only FGS, no camera), and a **Voices screen** (`ui/VoicesScreen.kt` + `net/SpeakersClient.kt`: list/name/merge speakers, play a sample-audio snippet, plus a **"Clean up voices"** section that surfaces backend-suggested duplicate groups and merges them in one tap / "Merge all"), and a **People screen** (`ui/PeopleScreen.kt` + `net/PersonsClient.kt`: the visual twin of Voices — list/name/merge faces, showing each face's sample-face crop via OkHttp + `BitmapFactory` + Compose `Image`, no image lib), and a **Plates screen** (`ui/PlatesScreen.kt` + `net/PlatesClient.kt`: the vehicle twin of People — list/search/name/merge license plates, showing each plate's `GET /v1/plates/{id}/sample-crop` crop the same OkHttp+`BitmapFactory` way; unnamed plates are labelled by their OCR `plate_text`). These are screen-state toggles in `MainActivity` (`Screen.{Capture,Voices,People,Plates}`), no nav framework. **First real client.** See its `README.md`. | ✅ built + **verified E2E on a physical Galaxy S8** |
| `contracts/` | The camera→backend contract (the boundary). | — |
| `local_dev/` | Helper scripts: **`run_stack.sh` (ONE command to bring up the whole backend/AI/web stack — see "Run the full stack")**, `feed_segments.py` (replay a video as segments — reference client), `run_hushai_app.sh` (drive the Android app), `export_capture.sh` (reassemble uploaded segments into a playable file), `gen_certs.sh` (local CA + LAN TLS cert), `setup_hostname.sh` (one-time: Bonjour `hushai.local` + a `pf` 443→8070 redirect), **`serve.sh` (the ONE macOS command for `https://hushai.local/`: cert + CA-trust + setup_hostname + `run_stack --lan`, idempotent; `--check` reports status. Linux/Windows runbooks: `docs/friendly-url-{linux,windows}.md`)**. | — |
| `Issues/` | The tickets: `initial-backend.md`, `transcription-embedding-and-rag.md`, `initial-android-app.md`. | all done |

## Run the full stack locally

Postgres (Homebrew `postgresql@16` + pgvector) runs on `localhost:5432`, DB `hushai`,
migrations already applied. Ollama models (`mxbai-embed-large`, `llama3.2:3b`) and the
whisper model (`models/ggml-base.en.bin`) are on disk.

### One command (recommended): `local_dev/run_stack.sh`

```bash
./local_dev/run_stack.sh                 # infra preflight + backend + worker + rag + viewer
./local_dev/run_stack.sh --with-android  # ...also build+drive the USB phone client (best effort)
./local_dev/run_stack.sh --no-build      # skip cargo build; run existing target/debug bins
./local_dev/run_stack.sh --release        # build/run the release binaries
./local_dev/run_stack.sh --pull          # `ollama pull` any missing models first
./local_dev/run_stack.sh --down          # stop a stack started earlier
```

It preflights the two infra deps (starts Postgres via `brew services` and `ollama serve`
if down — and leaves a *pre-existing* one running on exit), warns about missing model
files, builds once, then launches all four services **directly as compiled binaries**
(so each tracked PID is the server → one Ctrl-C tears the whole thing down cleanly).
Per-service logs stream to `local_dev/logs/<svc>.log` (gitignored). Health-checks every
port before printing the URL map. **Verified end-to-end 2026-06-28** (all four healthy,
a real `/v1/rag/query` answered, clean teardown).

Two non-obvious things the script encodes — preserve them if you touch it:
- **CWD per service** (for `dotenvy` + relative model/blob paths): backend runs from
  `hushai-backend/` (loads `hushai-backend/.env`, `BLOB_DIR=./data`); worker/rag/viewer
  run from the **repo root** (load root `.env`; `models/*` + blobs are root-relative).
- **`DYLD_FALLBACK_LIBRARY_PATH=target/<profile>/deps`** for the sherpa-linked binaries
  (worker, rag) — set via bash `export` and `exec`'d **directly**, NOT through `env`/any
  `/usr/bin` shim (SIP strips `DYLD_*` when exec'ing a protected binary, and the worker
  then dies with `@rpath/libonnxruntime.1.17.1.dylib … no LC_RPATH's found`). This is the
  same requirement as the launchd worker plist / the manual `cargo run` path below.

The Android client can't be containerized/auto-spawned (it needs a physically connected
USB phone), so `--with-android` is best-effort: it chains `run_hushai_app.sh` only if
`adb` sees an authorized device, else warns and leaves the rest of the stack up.

### Manual (one terminal per service)

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
#    Over USB this runs the WHOLE loop on the cable with no shared network: the
#    script auto-creates the `adb reverse` tunnels (8080+8090) and defaults both
#    the backend + RAG/TTS URLs to localhost. No --url / manual reverse needed.
./local_dev/run_hushai_app.sh --duration 120

# 5. viewer (browser NVR) -> 127.0.0.1:8070   (read-only; only needs Postgres + BLOB_DIR + ffmpeg)
cd <root> && SQLX_OFFLINE=true cargo run -p hushai-viewer   # then open http://127.0.0.1:8070/
```

**hushai-viewer gotcha (non-obvious):** each ~2s blob is remuxed to its own MPEG-TS, so each
restarts at PTS 0. Serving separate video + alt-audio renditions that way makes hls.js stall after
the first audio segment (`bufferStalledError`) — the two renditions don't share a clock. Fix
(`remux.rs`): stamp every TS with `-output_ts_offset <capture_start_seconds>` so video AND audio land
on one absolute (wall-clock) PTS clock; hls.js then interleaves them and corrects the 33-bit TS
rollover. Anything that changes the remux must preserve this. Verify with the headless-Chrome drive
script pattern (puppeteer-core + the *real* Google Chrome — open-source Chromium lacks H.264/AAC).

**hushai-viewer gotcha #2 — load ONE run per HLS window (`ui/js/app.js` `computeWindow`):** even
with the shared PTS clock, a window that spans a coverage **gap** or an **audio-only** stretch (e.g.
a session that recorded sound before the camera started) desyncs the alt-audio rendition — hls.js
reports `fragParsingError`/"Found no media in fragment 0" on the audio level and the picture freezes
on play (video buffers, but the video∩audio buffer intersection is empty → `readyState` stuck at 1).
So `computeWindow` clamps the loaded master to the **single continuous coverage span** containing the
playhead (not the whole device range), and `selectDevice` opens on `firstVideoMs()`. Don't widen the
window back to the full device range.

Config is env-driven; root `.env` (gitignored) feeds worker+rag, `hushai-backend/.env`
feeds the backend. Dev token: `dev-secret-token`. `OLLAMA_BASE_URL` is the shared default for
every Ollama call; set `EMBED_OLLAMA_BASE_URL` (worker + rag query embedding) and/or
`LLM_OLLAMA_BASE_URL` (rag answer generation) to route them at separate instances under load.

## Build/run gotchas (non-obvious)

- **Android toolchain** is installed no-sudo in non-standard spots: `JAVA_HOME=/opt/homebrew/opt/openjdk@17`,
  `ANDROID_HOME=~/Library/Android/sdk`, **adb is NOT on PATH** (`$ANDROID_HOME/platform-tools/adb`).
  Build with the project's `./gradlew` (pinned 8.9) — the system `brew` Gradle (9.x) is too new for AGP 8.7.3.
- **Phone connection (default = USB, no shared network):** the test phone is a **Galaxy S8 = Android 9**.
  Drive the **whole** dev loop over the USB cable — control (adb), uploads (:8080), and voice-assistant
  RAG/TTS (:8090) — with `adb reverse tcp:8080` + `tcp:8090` + both URLs at `localhost`. The debug build
  permits cleartext, so nothing on the phone needs WiFi. `local_dev/run_hushai_app.sh` does all of this
  automatically (auto reverse tunnels, localhost defaults, `--rag-url` flag → `rag_url` Intent extra,
  which overrides any stale LAN-IP left in DataStore by a wireless session). **Fallback (needs same LAN):**
  wireless adb — over an initial USB link run `adb tcpip 5555`, then `adb connect <phone-ip>:5555` (plain
  tcpip; `adb pair` is Android 11+ and the S8 is 9), and pass `--url`/`--rag-url` with the Mac's LAN IP.
- **sqlx 0.9 + pgvector 0.4.2:** query metadata is committed under each crate's `.sqlx/` (backend's
  `db.rs` macros only — worker/rag use runtime queries, as does the backend's `speakers.rs` **on
  purpose**, so its queries against the new speaker tables don't require a `.sqlx` re-prepare). Build
  with `SQLX_OFFLINE=true`. If you add/alter a backend `db.rs` `query!` macro, run
  `cd hushai-backend && DATABASE_URL=… cargo sqlx prepare -- --lib` and commit the
  `.sqlx/` change. **`cargo sqlx prepare` needs sqlx-cli 0.9** (`cargo install sqlx-cli --version 0.9.0
  --no-default-features --features postgres,rustls` — do NOT pass `--locked`; sqlx 0.9 dropped its
  tracked lockfile). Embeddings bind as **native `pgvector::Vector`** (sqlx 0.9 binary protocol) — see
  `hushai-worker/src/process.rs` (insert) and `hushai-rag/src/retrieve.rs` (`nearest`); there are no
  `::vector` text casts left. sqlx 0.9 also requires non-`'static` query strings be wrapped in
  `sqlx::AssertSqlSafe(...)` (only the `SET LOCAL` GUC lines in `retrieve.rs` need it).
- **Migrations auto-apply** on worker/backend startup via `sqlx::migrate!`. `0003_scalability.sql`
  recreates `transcript_sentences` as a **monthly RANGE-partitioned** table — see the schema note below.
  `0006_speaker_identity.sql` adds `transcript_sentences.speaker_id` (**text**, denormalized) + the
  `speakers` (global catalog, `centroid vector(192)`) and `speaker_segments` (monthly-partitioned raw
  192-d voiceprints) tables + their `ensure_/drop_speaker_segment_partitions` helpers.
  `0007_speaker_segment_knn.sql` adds a per-partition **HNSW `vector_cosine_ops` index** on
  `speaker_segments.embedding` (backs the matcher's k-NN + the raw-level recluster/duplicate queries)
  and a `quality text` column (`'clean'|'marginal'`; only `'clean'` rows feed the self-healing centroid).
- **Speaker embedding model** (`SPEAKER_MODEL_PATH`, default `./models/nemo_en_titanet_large.onnx`)
  is **not committed** — download it (k2-fsa `speaker-recongition-models` release, sha256 in
  `hushai-worker/.env.example`). The worker links **sherpa-rs** (`download-binaries` feature fetches a
  prebuilt sherpa-onnx native lib once at build; runtime offline). TitaNet outputs 192-d; the worker
  L2-normalizes and refuses non-finite embeddings (degenerate/near-silent input).
- **VAD model** (`VAD_MODEL_PATH`, default `./models/silero_vad.onnx`) is **not committed** — fetch via
  `local_dev/fetch_vad_model.sh` (k2-fsa `asr-models` release, sha256 in `hushai-worker/.env.example`).
  Silero VAD runs in the **same already-linked sherpa-onnx lib** (no extra crate). The worker now embeds
  VAD-cleaned speech (not the raw whisper-span slice), so background static no longer fragments one
  person into many "unknown speaker" rows. A NEW identity is minted only from clean, high-SNR, long-enough
  audio (the `SPEAKER_MINT_*` gates); marginal audio attaches to a known voice but never mints. Matching
  is a multi-vector k-NN vote over `speaker_segments` (centroid is the cold-start fallback) with two-
  threshold hysteresis (`SPEAKER_MATCH_THRESHOLD` < `SPEAKER_MINT_DISTANCE_FLOOR`).
- **TLS + LAN access control (2026-06-28)** — all three services terminate **native rustls** TLS
  (`axum-server`) via the shared `hushai-backend/src/tls.rs` helper when `TLS_CERT_PATH`/`TLS_KEY_PATH`
  are set; **unset ⇒ cleartext** (the localhost/USB dev path is unchanged). Certs come from
  `local_dev/gen_certs.sh` (a local CA + IP-SAN leaf in gitignored `local_dev/certs/`); clients trust
  the **CA**. See the "LAN security model" section below for the full picture (per-device tokens,
  viewer IP-allowlist + password gate, rag auth, Android release CA-trust). The Android **debug** build
  still permits cleartext to the LAN; the **release** build forbids it and trusts the bundled LAN CA.
- **Android UI** is a single Compose screen, `app/.../ui/CaptureScreen.kt` (master on/off control
  + grouped cards). All color/typography/shape comes from `HushaiTheme` in `app/.../ui/theme/`
  (an Uber-style **light** scheme, applied app-wide in `MainActivity`; system dark mode is ignored
  on purpose). Don't hardcode `Color(...)` in composables — add to `ui/theme/Color.kt` and read via
  `MaterialTheme.colorScheme`.

## `transcript_sentences` storage (post-0003 — read before touching RAG/worker writes)

The chunk store is **RANGE-partitioned by `created_at` (monthly)** with a `DEFAULT`
catch-all partition. Parent-level indexes propagate to every partition:
- `…_embedding_hnsw` — HNSW `vector_cosine_ops` (the ANN index; built per partition).
- `…_device_time_idx` — `(device_id, start_unix_nanos)`. **`device_id` is denormalized
  onto the table** so RAG filters sit on the same table as the HNSW index (no JOIN).
- `…_segment_id_idx` — makes the worker's idempotent delete-by-segment an index scan.

Also denormalized (post-0006): **`speaker_id` (text)**, backed by `…_speaker_time_idx
(speaker_id, start_unix_nanos)`; and the long-present **`sentiment`/`emotion`** columns (now written).

Rules:
- **Writes** (`hushai-worker/src/process.rs::write_transcript`) must set `device_id` and use
  the batched multi-row INSERT (now 11 binds/row: + `sentiment`, `emotion`, `speaker_id`). `created_at`
  defaults to `now()` — don't set it. Speaker match/mint runs **inside this same txn** (a global
  `pg_advisory_xact_lock`, then the prior-assignment read BEFORE the transcript DELETE, then — post-2026-06-26
  — `SET LOCAL` HNSW GUCs + a **k-NN vote over `speaker_segments`** + mint-guard hysteresis; see the
  de-dup overhaul section) — `speaker_match.rs`; `speaker_id`/`sentiment` are segment-level. `assign_speaker`
  returns `Option<Uuid>` (None ⇒ `speaker_id` NULL: refused/low-quality). It writes the raw 192-d vector +
  a `quality` tag to `speaker_segments` (only `quality='clean'` rows feed the centroid recompute).
- **Retrieval** (`hushai-rag/src/retrieve.rs::nearest`) runs in a txn that `SET LOCAL`s
  `hnsw.iterative_scan='strict_order'` + `hnsw.ef_search` + `statement_timeout`. Filter on
  `ts.device_id` / `ts.start_unix_nanos` / `ts.speaker_id` (local columns), never via a JOIN. A
  speaker filter binds stringified uuids as `text[]` (`= ANY($::text[])`) — never `Vec<Uuid>` against
  the text column; an empty array matches nothing (the unknown-name contract). `list_by_speaker` is
  the non-semantic exhaustive path (no vector order, no distance prune).
- **Partitions:** worker calls `ensure_transcript_partitions(3)` at startup (≈3 months headroom);
  for long uptimes a scheduled job must also call it monthly so inserts never fall to `_default`.
  Ship that schedule via **`local_dev/partition_maintenance.sh`** (run by launchd —
  `local_dev/com.hushai.partition-maintenance.plist` — or system cron) or **pg_cron** in-DB
  (`local_dev/partition_maintenance.pg_cron.sql`). Retention is `drop_transcript_partitions_before(cutoff)`;
  the maintenance job enforces a **12-month** window by default (`RETAIN_MONTHS`, `0` = never drop —
  *dropping a partition is irreversible*; rehearse with `DRY_RUN=1` first).
- Requires **pgvector ≥ 0.8** (iterative_scan + HNSW on a partitioned parent). Installed: 0.8.0.
- New segments are queued for transcription **at ingest** (status row written in the segment
  txn in `hushai-backend/src/db.rs`) and a `pg_notify('hushai_segment_ingested', …)` wakes the
  worker. The old idle full-table backfill scan is gone (startup backfill remains for old rows).

## Recent fixes (2026-06-24, this session)

- **RAG/chunk-storage scalability** (migration `0003` + worker/rag/backend): see the
  `transcript_sentences` section above. Closed: the filtered-search recall cliff (device filter
  was on a JOINed table the HNSW index couldn't use), the per-write full-table scan (no index on
  `segment_id`), per-row inserts, the repeated idle backfill scan, and unbounded single-index
  growth (now partitioned + retention helpers). Multi-tenancy is intentionally out of scope (single-tenant).

- **Capacity-tier scalability follow-ups** (`Issues/rag-storage-scalability-followups.md`, all 3 done):
  (1) **Native binary vector encoding** — workspace bumped to **sqlx 0.9 + pgvector 0.4.2**; embeddings
  now bind as `pgvector::Vector` over the binary protocol (no more `[..]::vector` text literals); verified
  behavior-neutral (identical RAG sources + distances before/after). (2) **Embed/LLM endpoint split** —
  `EMBED_OLLAMA_BASE_URL` / `LLM_OLLAMA_BASE_URL` independently override `OLLAMA_BASE_URL` so ingest-time
  embedding can't starve query answering. (3) **Scheduled partition maintenance** — `local_dev/partition_maintenance.{sh,pg_cron.sql}`
  + launchd plist, default 12-month retention. The cross-segment embedding batch/queue (item 2(b)) was
  left undone — it's explicitly optional and the per-segment embed call already batches a segment's sentences.

- **Worker container handling** (`hushai-worker/src/media.rs`): the worker previously
  ALWAYS prepended `codec_init_data` to the blob, assuming the `feed_segments.py` fMP4
  convention. The Android client uploads **self-contained MP4s** (`container="mp4"`) with
  raw SPS/PPS in `codec_init_data`; prepending corrupted them ("moov atom not found").
  Fixed to prepend only when `container == "fmp4"` (keys off `container`, per §7). The
  Android client is the conformant side. After the fix, Android audio transcribes and is
  RAG-retrievable end-to-end (verified).

## Speaker de-duplication overhaul (2026-06-26)

Fixed the "background static fragments one person into many `unknown speaker` rows" problem.
Three root causes, three layers (all built + verified: workspace `cargo check` clean, worker
lib + DB tests pass, backend tests pass, Android `assembleDebug` ok, live-DB end-to-end of the
new endpoints + the mint-guard). Touched `hushai-worker/{speaker,vad,speaker_match,process,config,lib}.rs`,
`hushai-backend/src/{speakers,routes}.rs` + migration `0007`, and `hushai-android/.../{net/SpeakersClient,ui/VoicesScreen}.kt`.

1. **Real VAD (the core fix).** `vad.rs` never did VAD — it embedded the whole whisper-timestamp
   slice *including static*. Now `speaker::VoiceDetector` (sherpa Silero VAD, in the already-linked
   native lib — no new crate, just `VAD_MODEL_PATH`) strips static/silence and the worker embeds the
   concatenated **speech-only** PCM.
2. **Identity model.** Single drifting running-mean centroid + "mint on any miss" → replaced with a
   **multi-vector k-NN vote** over `speaker_segments` raw embeddings (centroid = cold-start/aged-out
   fallback), **mint-guard hysteresis** (`SPEAKER_MATCH_THRESHOLD` < gray-zone < `SPEAKER_MINT_DISTANCE_FLOOR`),
   a **quality gate** (`vad::assess_quality` → Mint/AttachOnly/Reject) so only clean, high-SNR,
   long-enough audio can MINT a new identity (marginal audio attaches or is left NULL — never a
   duplicate), and a **self-healing centroid** (recomputed from recent `quality='clean'` segments).
   `assign_speaker` now returns `Option<Uuid>` (None = refuse). Lock + idempotency invariants preserved.
3. **Heal + auto-merge + UX.** `speakers.rs` clusters at the **raw-embedding** level (`compute_edges`
   per-segment LATERAL k-NN over the partitioned table — references the base table directly, NOT a CTE,
   so the new HNSW index is used; `build_clusters` union-find; `collapse_cluster` repoints both child
   tables + recomputes the centroid from raw segments + deletes losers). New endpoints
   `POST /v1/speakers/recluster-deep`, `GET /v1/speakers/duplicates`, `POST /v1/speakers/merge-group`
   (name-conflict-guarded). The worker calls `auto_merge_recent` after a drain (worker 0, rate-limited,
   tight `SPEAKER_AUTOHEAL_DISTANCE`, scoped to recently-active speakers — does NOT mass-collapse the
   historical backlog). Android `VoicesScreen` gained a **"Clean up voices"** section (suggested
   groups + Merge group / Merge all).

All thresholds (`SPEAKER_MINT_*`, `SPEAKER_KNN_*`, `VAD_*`, `SPEAKER_AUTOHEAL_*`) are documented in
`hushai-worker/.env.example`. **They are uncalibrated starting guesses** — see Fast-follows.

## Vanishing-voice fix (2026-06-26)

Fixed "a frequent speaker (the owner) never appears in the Voices list to be named, and chat
lumps them in as the one *'someone we haven't identified yet'* who talks the most." Root cause
was operational + a backfill gap, not the matcher logic: the worker had been down, leaving a
large `pending` backlog, and segments transcribed during a window when the speaker stage wasn't
running were stuck `done` with `speaker_id = NULL` and **no `speakers` row** — and nothing ever
re-ran speaker work on a `done` segment (`claim_one` only claims `pending`/`error`). Five layers:

1. **Self-healing speaker backfill** (`hushai-worker/src/{claim,lib,config}.rs`):
   `reconcile_missing_speaker_segments` re-queues `done` AUDIO/MUXED segments lacking a
   `speaker_segments` row, run at startup behind `SPEAKER_BACKFILL_ON_START` (default on).
   Idempotent + convergent (`NOT EXISTS` on `segment_id`; once a voiceprint exists it's a no-op).
   Convergence depends on the **reject tombstone** below — a no-voice segment writes a
   `quality='reject'` row (NULL speaker, NULL embedding) so it isn't re-queued every restart;
   `assign_speaker` ignores that tombstone so a later, better run (e.g. windowed) can still
   attribute it. Tombstones are inert to matching/clustering/centroid (all filter them out).
2. **Skip video** (`hushai-worker/src/claim.rs` `claim_one`/`ensure_status_rows`,
   `hushai-backend/src/db.rs` ingest): both filter to `media_type IN (1,3)`.
3. **Surface unattributed audio** (`hushai-backend/src/speakers.rs` + `routes.rs`):
   `GET /v1/speakers/unattributed` clusters NULL-speaker `speaker_segments` (the matcher writes a
   voiceprint even when it refuses to attribute) into candidate voices via the same raw-embedding
   HNSW machinery (`compute_null_segment_edges`/`build_segment_clusters`, keyed by segment id);
   `POST /v1/speakers/unattributed/name` mints a speaker from a chosen cluster and repoints both
   child tables (advisory-locked, race-safe: only still-NULL segments are claimed). This is the
   human-in-the-loop path for a speaker who is *only ever* marginal-quality (the mint-guard
   correctly never auto-mints them). Android `VoicesScreen` gained an **"Identify new voices"**
   section (`net/SpeakersClient.kt` `listUnattributed`/`nameUnattributed`).
4. **Chat de-collapsing** (`hushai-rag/src/{speakers,retrieve,llm,analytics}.rs`): `display_label`
   + `assign_unnamed_ordinals` render distinct unnamed speakers as `unidentified speaker N` and
   NULL audio as the non-person `unattributed audio`, so the LLM no longer merges several
   different unidentified people into one. `name_map` stays named-only.
5. **Supervised worker** (`local_dev/com.hushai.worker.plist`): launchd KeepAlive daemon so the
   worker stays up and the backlog can't silently accumulate again (`WorkingDirectory` = repo
   root is required — model paths are relative; `DYLD_FALLBACK_LIBRARY_PATH` must point at the
   build dir so sherpa's bundled `libonnxruntime` resolves).
6. **Rolling-window speaker embedding** (`hushai-worker/src/{media,process,config}.rs`): short
   ~2s clips rarely have ≥`SPEAKER_MIN_SPEECH_SECS` of post-VAD speech alone, so the voiceprint
   is embedded over a segment PLUS its contiguous same-stream predecessors (`select_window`,
   stopping at a `gap_before`/sequence break, budget `SPEAKER_WINDOW_TARGET_SECS`, cap
   `SPEAKER_WINDOW_MAX_SEGMENTS`). `extract_pcm`'s ffmpeg temp file is uuid-suffixed (per-call,
   not per-segment) — windowing decodes a segment as a neighbor while another worker decodes it
   as its current one, and a segment-keyed temp would collide.
7. **Gate calibration for real phone audio**: defaults lowered for short/noisy clips —
   `SPEAKER_MIN_SPEECH_SECS` 0.8→0.3 (reject gate), `SPEAKER_MINT_MIN_SNR_DB` 10→3,
   `SPEAKER_WINDOW_TARGET_SECS` 6. The voiced-fraction gate still guards minting, so sparse
   windows go `Action::Null` (clusterable) rather than minting duplicates. After a calibration
   change, run once with `SPEAKER_REPROCESS_REJECTS_ON_START=true` to clear sticky `reject`
   tombstones and re-evaluate those segments under the new gates.

## Vision pipeline — Phase A: face identity (2026-06-26, built + verified E2E)

The worker now has **eyes**: a vision path beside the ASR path that processes **VIDEO/MUXED**
segments (`media_type IN (2,3)`) into a cross-device **person/face catalog**, the visual sibling of
the speaker system. Built + verified end-to-end on the live stack (fed a 2-face clip → 2 distinct
persons minted, all observations attributed; worker unit tests green; ort/sherpa coexistence proven).

- **Models (ONNX via the `ort` crate, NOT committed — operator-fetched, `models/` gitignored):**
  YuNet `face_detection_yunet_2023mar.onnx` (face detect + 5-pt landmarks, Apache-2.0) → ArcFace
  `w600k_r50.onnx` (512-d embedding; **weights are non-commercial-research** — code stays MIT) with
  closed-form similarity-transform landmark alignment + quality gates (det-score / min-px /
  variance-of-Laplacian sharpness; `Mint`/`AttachOnly`/`Reject`, mirroring the VAD gate). Code:
  `hushai-worker/src/vision/{model,frames,detect,face_embed,face_match,write}.rs`.
- **Identity** (`vision/face_match.rs`) is a near-verbatim mirror of `speaker_match.rs`: global
  `pg_advisory_xact_lock` (DIFFERENT key, `0x6873_7670_736e`), multi-vector k-NN vote over
  `person_segments` + centroid fallback, mint-guard hysteresis (`FACE_MATCH_THRESHOLD` <
  `FACE_MINT_DISTANCE_FLOOR`), idempotent delete-by-segment (MANY faces/segment), self-healing
  centroid. Tables `persons` + `person_segments` (migration `0009_person_vision.sql`, partitioned
  + HNSW, same shape as `speakers`/`speaker_segments`). `scene_objects` was recreated objects-only
  (CLIP space, Phase B); faces and CLIP objects are **separate 512-d tables/HNSW indexes** (different
  vector spaces — a shared index would return garbage NN).
- **Work queue:** a SEPARATE `segment_vision_status` table + `claim_one_vision` /
  `ensure_vision_status_rows` / `mark_vision_{done,error}` (`claim.rs`), so a vision failure and an
  ASR failure retry independently. Backend ingest (`db.rs`) queues vision status for VIDEO/MUXED
  (runtime query, no `.sqlx`); `lib.rs` builds the models resiliently (missing model/dylib →
  vision self-disables, audio continues) and runs a `vision_worker_loop`.
- **ORT runtime — the load-bearing gotcha (`AGENTS.md` "vision ONNX runtime"):** `ort`
  `=2.0.0-rc.9` with `default-features=false, features=["load-dynamic","ndarray","coreml"]`.
  sherpa-rs statically bundles ONNX Runtime **1.17.1**; `ort-sys` rc.9 targets **1.20.0**, so `ort`
  uses `load-dynamic` (no link-time ORT) and `dlopen`s its OWN 1.20 dylib via `ORT_DYLIB_PATH`
  (`local_dev/fetch_onnxruntime.sh` → `models/onnxruntime/.../libonnxruntime.1.20.0.dylib`, NOT
  committed) — a second image alongside sherpa's 1.17.1, coexisting under macOS two-level namespaces
  (proven by `hushai-worker/tests/ort_coexistence.rs`). CoreML EP loads on Apple Silicon.
- **LAUNCH NOTE:** the worker binary must be started via `cargo run` OR with
  `DYLD_FALLBACK_LIBRARY_PATH=target/debug/deps` so sherpa's `libonnxruntime.1.17.1.dylib` resolves
  (a bare `./target/debug/hushai-worker` fails "Library not loaded: …1.17.1.dylib / no LC_RPATH").
  The launchd plist must set this.
- **Honesty:** most always-on frames have NO usable face (the real corpus is camera-on-desk →
  objects, not faces) — written as no rows, not errors. Thresholds are **uncalibrated guesses**
  (clean-photo defaults; degraded/low-res faces shrink inter-person distance → calibrate
  `FACE_MATCH_THRESHOLD`/`FACE_MINT_DISTANCE_FLOOR` on real footage, like the speaker follow-up).
- **Phase B objects — built (optional lane) + RAG query side:** `vision/objects.rs` adds RF-DETR
  region boxes + per-region & whole-frame (`object_label='__frame__'`) OpenCLIP image embeddings into
  `scene_objects`, inserted in the SAME vision tx (delete-by-segment idempotency). The lane is
  **optional + non-fatal**: it activates only when both `OBJECT_DET_MODEL_PATH` (RF-DETR) and
  `CLIP_IMAGE_MODEL_PATH` (CLIP) load; else it self-disables (or hard-fails the vision subsystem when
  `OBJECT_REQUIRED=true`) and faces still run. Tuning knobs: `OBJECT_MIN_DET_SCORE`,
  `OBJECT_DET_INPUT_SIZE` (384), `OBJECT_MAX_PER_FRAME` (20), `OBJECT_MIN_BOX_PX` (16).
  - **Provision + VALIDATE the decode (operator step — models are gitignored):** run
    `local_dev/export_rf_detr.py` (→ `models/rf-detr-nano.onnx` + `rf-detr-classes.json`),
    `local_dev/export_clip.py` (→ image+text towers from ONE OpenCLIP ViT-B/32 checkpoint),
    `local_dev/fetch_clip_tokenizer.sh` (→ `clip_tokenizer.json`), `local_dev/make_object_clip.sh`
    (→ `object_clip.mp4`). Then validate against the REAL export:
    `cargo test -p hushai-worker --test vision_pipeline inspect_object_model_io_shapes -- --nocapture`
    (reads the real output names/order/shapes + class count) →
    `OBJECT_TEST_MP4=./object_clip.mp4 cargo test ... detect_objects_from_real_video` (asserts real
    COCO labels, in-frame boxes, unit-norm embeds) → the decisive cross-modal gate
    `CLIP_TEST_IMAGE=car.jpg cargo test -p hushai-rag --test clip_text clip_text_matches_image_cross_modal`
    (`cos(car_img,'a car') > cos(car_img,'a dog')` — proves both towers share one space + the tokenizer).
    ⚠️ The `objects.rs` decode is defensive (boxes by output order, cxcywh, sigmoid logits, **COCO-80**
    labels). RF-DETR may use a **90/91-slot** COCO layout — if `detect_objects_from_real_video` shows
    `class_<i>` labels or off-frame boxes, fix `coco_label()`/the class offset against
    `models/rf-detr-classes.json` before trusting labels. Per-frame detect/embed failures are logged + skipped.
  - **RAG query side (`hushai-rag`):** a CLIP **text** tower (`clip_text.rs`, same load-dynamic ORT
    coexistence as the worker) embeds the query phrase; `retrieve::nearest_objects` NN-searches
    `scene_objects`' OWN HNSW (the OpenCLIP space — **never** `person_segments`/ArcFace), and
    `list_by_object_class` lists exact-class sightings. Surfaced via the **`objects` agent** ("Things
    seen") on `/v1/rag/query` + `/v1/rag/chat`: `{ "query":"when did I see a car", "agent_id":"objects" }`.
    Knobs: `RAG_OBJECTS_ENABLED`, `CLIP_TEXT_MODEL_PATH`, `CLIP_TOKENIZER_PATH`, `ORT_DYLIB_PATH`,
    `RAG_OBJECT_DISTANCE_THRESHOLD` (~0.75, CLIP cosine is looser than mxbai), `RAG_OBJECT_TOP_K_DEFAULT`.
    Disabled / model-absent → the agent returns 503 (chat: a friendly "not loaded" message).
- **Phase C backend `/v1/persons` — built + tested:** `hushai-backend/src/persons.rs` (runtime sqlx, bearer auth):
  `GET /v1/persons` (list + recent sightings), `PATCH /v1/persons/{id}` (name a face), `POST
  /v1/persons/{id}/merge`, `GET /v1/persons/{id}/sample-face` (ffmpeg seek+crop JPEG). The viewer proxy
  (`proxy.rs::is_backend_path`) now routes `/v1/persons*` → backend too (alongside `/v1/speakers*`).
  Integration tests: `hushai-backend/tests/persons.rs` (list/rename/merge, DATABASE_URL-gated).
  **`n_samples` vs `n_sightings`:** `n_samples` is the raw per-frame face-template count (one
  `person_segments` row per detected face per sampled frame) — an internal weight for centroid/merge
  math, NOT a human count (a single few-second clip is sampled across many frames → ~20 rows). The
  list endpoint also returns `n_sightings`, distinct appearances obtained by sessionizing a face's
  detections by time gap (`PERSON_SIGHTING_GAP_SECONDS`, default 60s; a new sighting starts at the
  first detection and whenever the gap to the previous exceeds the threshold). Both the web People
  modal and the Android People screen display `n_sightings` as "N sightings".
- **Phase D Android "People" screen — built:** `ui/PeopleScreen.kt` + `net/PersonsClient.kt` (the visual
  twin of Voices: list/name/merge faces, showing the sample-face crop via OkHttp+`BitmapFactory`+Compose
  `Image` — no image lib). A `Screen.People` branch in `MainActivity` + an "Open People" card in
  `CaptureScreen`. Test: `PersonsClientTest.kt` (MockWebServer).
- **Phase E RAG person attribution — built + tested:** `hushai-rag/src/persons.rs` (name↔uuid resolution,
  mirror of `speakers.rs`; uuid not text) + `retrieve::list_by_person` ("when did I see Bob", deduped per
  segment) + `retrieve::list_co_occurring_persons` ("who was I with", same-segment co-presence around
  `OWNER_PERSON_ID`/`OWNER_PERSON_NAME`). Surfaced via the **`people` agent** on `/v1/rag/query` +
  `/v1/rag/chat`: explicit `filters.person_name`/`person_id`, OR a catalog name found in the free-text
  query, routes to per-person sightings; otherwise co-occurrence. Tests: `tests/persons_retrieve.rs`.
- **Viewer "Detections" overlay — built + verified E2E (the headline of this work):** the browser NVR
  has a **Video | Detections** tab; in Detections mode a canvas overlays classic bounding boxes synced to
  playback — people labeled by name (or "Unidentified"), objects by class. Backed by
  `GET /api/devices/{id}/detections?from&to` (`hushai-viewer/src/detections.rs`: two windowed queries over
  `person_segments`+`persons` and `scene_objects`, merged + grouped by sampled-frame timestamp; `__frame__`
  rows excluded; `truncated` flag, no silent caps) and `ui/js/detections.js` (overlay canvas, intrinsic→
  displayed letterbox scaling against `video.videoWidth/Height` — sound because remux is `-c copy`, snap-to-
  nearest-sampled-frame within tolerance). Verified in real headless Chrome (boxes track the picture).
- **Viewer scrub-bar AI processing-status ribbons — built + verified E2E:** the timeline shows how far the
  AI pipeline has processed each stretch. Two thin ribbons under the coverage track — **AUDIO** lane
  (transcription/speaker/sentiment) and **VISION** lane (faces/objects) — colored by pipeline stage
  (`done`/`processing`/`pending`/`error`; `processing` animates a marching shimmer), plus a live
  "under-the-playhead" badge in the topbar and a hover tooltip carrying per-lane status **and output counts**
  (sentences/speakers, faces/objects). Backed by `GET /api/devices/{id}/processing?from&to`
  (`hushai-viewer/src/processing.rs`: per-lane `segments` LEFT JOIN `segment_transcription_status` /
  `segment_vision_status` gated by `media_type` — a lane's *absence* = "not applicable here", not "pending" —
  merged with per-segment output-count queries and coalesced like `timeline::build_timeline`; `truncated`
  flag, no silent caps; reuses the existing indexes, **no migration**). Frontend: `ui/js/timeline.js`
  (two **labeled** ribbons — AUDIO/VISION drawn on a faint always-visible base track — + a chip hover tooltip
  + rAF-driven shimmer) and `ui/js/app.js` (windowed poll, refreshed **every** auto-refresh tick —
  independent of the device-signature short-circuit, since processing advances without new footage). The key
  lives in a **click-to-open ⓘ popover** (`#aiInfoPop`, reusing the `.ai-badge` pills) next to the "AI status"
  toggle — deliberately NOT crammed into the bottom legend. Always-on; toggle via the checkbox or the `a` key.
  Verified in real headless Chrome (all four states + shimmer + live badge + chip tooltip + popover + a live
  recolor when a region flips processing→done).
- **All phases (B objects, C/D persons API + Android, E person attribution) — built.** Remaining is the
  operator step of provisioning + validating the RF-DETR/CLIP ONNX exports (the model-gated tests SKIP until
  then) and threshold calibration on real footage. Plan: `~/.claude/plans/here-is-the-end-polished-horizon.md`.

## Vision pipeline — image cleanup + license plates (ALPR) (2026-06-28, built)

> **Full technical reference:** `docs/vision-image-cleanup-and-alpr.md` — file map, cascade decision
> tree, exact DB schema, the complete config table, model I/O contracts, provisioning, and E2E steps.
> The summary below is the orientation; that doc is the detail.

A **detect → expand-crop → zoom/restore/deskew/sharpen → recognize** cleanup stage now sits between
detection and recognition for BOTH faces and vehicles ("whenever we identify a person or a car, clean
up the image to get the clearest crop"). Model posture for this work: **best-accuracy weights
regardless of license**, gitignored under `models/`, operator-fetched (same posture as ArcFace). New
shared core in `hushai-worker/src/vision/`:

- **`enhance.rs`** (shared by both lanes): `crop_with_margin`, `resize_rgb` (Lanczos3), `unsharp_mask`,
  `clahe_gray` (tiled CLAHE), `deskew`, `homography_warp` (4-point DLT perspective rectify),
  `bilinear_sample` (the ONE sampler — `face_embed::align_crop` now calls it), plus ONNX wrappers
  `Upscaler` (Real-ESRGAN x4, RGB NCHW [0,1]) and `FaceRestorer` (GFPGAN/CodeFormer, 512² RGB [-1,1]).
  `geom.rs` holds the shared `iou`/`nms_by` (lifted out of `detect.rs`).
- **Face detector is now a trait** `detect::FaceDetect` (`Arc<dyn FaceDetect>` on `VisionModels`).
  **SCRFD** (`detect_scrfd.rs`, `scrfd_10g_bnkps.onnx`) is the **default** (`FACE_DETECTOR_KIND=scrfd`,
  best small/distant recall, same 5-pt landmark contract); **YuNet** stays as the fallback —
  `build_face_detector` loads the configured one and falls back to whichever IS provisioned.
- **Face cleanup cascade** (`write.rs::enhance_and_embed`): already-clean (Mint) faces embed the raw
  aligned crop (legacy ArcFace space preserved); a not-clean-but-recoverable face is margin-cropped →
  super-resolved (if tiny) → **blind-face-restored** → aligned on the restored pixels → re-assessed →
  embedded (`restored=true`). This is **recover-then-embed instead of reject**: a restored face may
  MATCH/attach to a known person but MINTS a new one only when it was genuinely large + frontal
  (landmark **pose proxy** `face_embed::pose_from_landmarks` gates minting; `FACE_MINT_MAX_YAW/PITCH`).
  **Flip-TTA** (`FaceEmbedder::embed_aligned`) averages a crop's embedding with its mirror. The
  restored **best-shot** crop is persisted to `<BLOB_DIR>/face_crops/` (`FACE_PERSIST_CROP`); backend
  `sample_face` now prefers that stored crop over re-cropping the raw frame. `face_match.rs` honors
  `FACE_RESTORED_MAY_MINT` (dev-stage default true; set false on a populated catalog so restored
  embeddings can't drift centroids until validated). Migration **`0012_face_crops.sql`** adds
  `person_segments.{crop_uri,is_best_shot,restored,yaw,pitch,quality_score}` (additive/nullable).
- **ALPR lane** (`vision/plates/`, optional + non-fatal like objects; needs RF-DETR **+** a plate
  detector **+** a plate OCR model): RF-DETR runs ONCE per frame and fans out to the CLIP object lane
  AND the plate lane. Per vehicle ROI (car/truck/bus/motorcycle): zoom in → `plates/detect.rs`
  (defensive YOLO bbox/4-corner decode) → `plates/rectify.rs` (homography deskew to 256×64 +
  SR/CLAHE/unsharp) → `plates/ocr.rs` (fast-plate-ocr CCT / PaddleOCR, NOT tesseract; charset sidecar
  `lp_ocr_charset.json`) → `plates/normalize.rs` (normalize + confusable-fold + per-position temporal
  **vote** + quality gate) → `plates/plate_match.rs` (advisory-locked, idempotent, **match-or-mint by
  normalized STRING** — exact + trigram/edit-distance fuzzy — NOT a k-NN; self-healing canonical text).
  Migration **`0013_license_plates.sql`**: `license_plates` catalog (pg_trgm + fuzzystrmatch, unique
  `plate_text_norm`) + monthly-partitioned `plate_detections`. `ensure_plate_detection_partitions` is
  called at worker startup.
- **Surfaces:** backend `plates.rs` (`GET /v1/plates`, `/v1/plates/search?q=`, `PATCH`, `merge`,
  `sample-crop`); RAG **Plates agent** (`AgentKind::Plates`, `resolve_plate*` + `list_by_plate`) answers
  *"when did I see plate ABC123"*; viewer **🚗 Plates** modal (`ui/js/settings/plates.js` + proxy
  `/v1/plates*` + amber plate boxes on the detections overlay); Android `PlatesClient.kt`/`PlatesScreen.kt`.
- **Provision + VALIDATE (operator step — all gitignored):** `local_dev/fetch_scrfd.sh`,
  `export_gfpgan.py`, `fetch_realesrgan.sh`, `fetch_plate_detector.sh`, `export_plate_ocr.py`; then the
  decode-validation tests `cargo test -p hushai-worker --test vision_pipeline
  inspect_enhance_model_io_shapes / inspect_plate_model_io_shapes / restored_low_quality_recovers_identity
  -- --nocapture` (model-gated; SKIP until provisioned). Thresholds in `.env.example` are uncalibrated
  starting guesses. Every new model/lane self-disables when unprovisioned — faces+objects keep running.

## LAN security model (2026-06-28)

The stack is built to run on a shared WiFi LAN, not just trusted localhost. Three
independent planes; understand which control protects which surface before changing any of it.

**1. Confidentiality in transit — native rustls TLS (all three services).**
`hushai-backend/src/tls.rs` (`serve` / `serve_with_connect_info`) replaces the old `axum::serve`
call in each `lib.rs`. TLS is **opt-in by env**: `[<PREFIX>]TLS_CERT_PATH` + `[<PREFIX>]TLS_KEY_PATH`
(prefixes `RAG_`/`VIEWER_`, falling back to the bare names so one cert serves all three). Unset ⇒
cleartext (the USB/adb + localhost dev path keeps working untouched). Crypto provider is **aws-lc-rs**
(already vendored via sqlx) installed once; `axum-server` uses `tls-rustls-no-provider` so there's no
second `ring` provider. Graceful shutdown is preserved (the helper bridges `shutdown_signal()` to
`axum_server::Handle::graceful_shutdown`). Certs: `local_dev/gen_certs.sh` → a 10-yr local CA + an
825-day IP-SAN leaf in gitignored `local_dev/certs/`. **Clients trust the CA, not the leaf**, so a
DHCP IP change only needs a leaf re-mint (re-run the script). No DYLD interaction (pure-Rust crates).

**2. The browser/admin plane — the viewer is the admin panel, gated two ways (`hushai-viewer/src/auth.rs`).**
The viewer (NVR video + chat + Voices/People admin + dashboard) is now locked by **IP allowlist + a
password**, both required (the router splits into `/healthz` (open) + a login surface (IP-only) + the
gated app (IP + session) — see `routes.rs`). IP filtering MUST live at the viewer: it reverse-proxies
`/v1/*` from its own process, so backend/rag only ever see `127.0.0.1`. Config: `VIEWER_ADMIN_IP_ALLOWLIST`
(comma IPs/CIDRs; empty ⇒ loopback-only via `VIEWER_ALLOW_LOOPBACK`, default on), `VIEWER_ADMIN_PASSWORD`
(or `VIEWER_ADMIN_PASSWORD_HASH`; argon2), `VIEWER_SESSION_SECRET` (HMAC-SHA256 stateless cookie; random
+ WARN if unset), `VIEWER_SESSION_TTL_SECS` (7d), `VIEWER_COOKIE_SECURE` (defaults to whether TLS is on),
`VIEWER_AUTH_DISABLED` (pure-local escape hatch; IP gate still applies). To expose to admin computers,
set `VIEWER_BIND_ADDR=0.0.0.0:8070` + the allowlist. The login page (`ui/login.html`) is self-contained
(served pre-auth, so it can't pull the gated `/styles.css`); `ui/js/api.js` bounces any 401 to `/login`.

**Friendly admin URL — `https://hushai.local/`.** The TLS leaf is issued for `CN=hushai.local` (plus
loopback + LAN IPs), so the intended admin URL is `https://hushai.local/` — not the raw IP. `local_dev/setup_hostname.sh`
makes it work *and* drops the port: it sets this Mac's **Bonjour `LocalHostName`** to `hushai` (advertises
`hushai.local` → the LAN IP) and installs a `pf` redirect **443 → 8070** (persisted via a `RunAtLoad`
LaunchDaemon, loaded into the `com.apple/hushai` sub-anchor so macOS's default `rdr-anchor "com.apple/*"`
evaluates it), so the unprivileged viewer keeps binding `8070` and terminates TLS there. Because `hushai.local`
resolves to the LAN IP (not loopback), the viewer must bind `0.0.0.0` and the caller's IP must be allowlisted —
**`run_stack.sh --lan`** does both (sets `VIEWER_BIND_ADDR=0.0.0.0:8070`, fills `VIEWER_ADMIN_IP_ALLOWLIST`
with this host's LAN IP(s), and sets the cosmetic `VIEWER_HOSTNAME`). Re-run after a DHCP change; undo with
`setup_hostname.sh --remove` (restores the prior `LocalHostName`). **One-command (macOS):** `./local_dev/serve.sh`
chains cert→CA-trust→`setup_hostname.sh`→`run_stack --lan` (idempotent; `--check` reports status without changing
anything). NB the `pf` redirect targets the **LAN IP**, not `127.0.0.1` — on macOS a `rdr` to a *loopback* port
drops direct connections to it (would break the `127.0.0.1:8070` health check). Linux/Windows equivalents (Avahi
+ nftables/`setcap`; hosts/Bonjour + `netsh portproxy`): `docs/friendly-url-{linux,windows}.md`.

**3. The data plane — per-device tokens + rag auth (over TLS).**
Backend ingest auth (`hushai-backend/src/auth.rs`) is now a **`HashMap<token,label>`**: set `DEVICE_TOKENS`
(`label:token,label:token,…`) for per-device revocable tokens (drop an entry + restart to revoke);
`DEVICE_TOKEN` (single) still works and stays required (the viewer proxy's `BACKEND_TOKEN` defaults to it).
Comparison is constant-time (`subtle`). **rag** auth is now expected on: set `RAG_TOKEN` (it WARNs if
unset) — the viewer proxy injects it for the browser and the Android assistant presents it. backend + rag
stay on `0.0.0.0` (the phone hits both over the LAN); they're protected by bearer tokens, the viewer by
IP+password. The Android **release** build (`app/src/release/`) forbids cleartext and trusts the bundled
LAN CA (`res/raw/hushai_lan_ca.pem`, gitignored — copy from `local_dev/certs/ca.crt` before
`assembleRelease`); the assistant's rag bearer flows via the `rag_token` Intent extra / Settings.

**Run it:** `./local_dev/run_stack.sh --tls` auto-generates certs, exports the cert env + `RAG_TOKEN`
+ a dev `VIEWER_ADMIN_PASSWORD`, sets `VIEWER_COOKIE_SECURE`, health-checks over https, and prints the
dev credentials in the banner. Plain `run_stack.sh` (no `--tls`) runs cleartext but still with auth on.
Add **`--lan`** (implies `--tls`) to bind the viewer on the LAN + allowlist this host for the friendly
`https://hushai.local/` URL. Simplest of all: **`./local_dev/serve.sh`** does the cert + CA-trust +
`setup_hostname.sh` + `run_stack --lan` in one shot.

**Onboard a camera:** `./local_dev/run_stack.sh --add-camera <name>` mints + persists a per-device token
(`DEVICE_TOKENS` in `hushai-backend/.env`, seeding an `admin:<DEVICE_TOKEN>` entry so the viewer proxy
keeps authenticating) and prints a config card. **Full step-by-step runbook:
[`docs/onboarding-a-camera.md`](docs/onboarding-a-camera.md)** (`docs/` is the human-facing runbooks dir).

## Device & footage management (2026-06-28, built + verified)

The destructive admin surface for reclaiming storage — **rename a device**, **per-device/per-date
usage**, per-device **retention** ("keep last N days", auto-purged), **delete footage by date / in
bulk / a whole device**, and **export** a date to MP4. New **🗄 Files** page (`ui/manage.html` +
`ui/js/manage/manage.js`, sibling of the System dashboard) plus the friendly `display_name` baked into
the camera dropdown + dashboard cards. Full reference: **[`docs/device-and-footage-management.md`](docs/device-and-footage-management.md)**.

- **Backend (`hushai-backend/src/devices.rs`, bearer-authed, proxied via the viewer like
  `/v1/speakers*`):** `GET /v1/devices`, `GET /v1/devices/{id}/usage?tz=` (local-day buckets; bad tz →
  400), `PATCH /v1/devices/{id}` (rename), `PUT /v1/devices/{id}/retention` (`{retention_days:N|null}`),
  `DELETE /v1/devices/{id}/footage?tz=&day=`, `POST /v1/devices/{id}/footage/bulk-delete`,
  `DELETE /v1/devices/{id}` (device + all footage). Migration **0011** adds `devices.display_name` +
  `devices.retention_days`. Runtime sqlx, `IngestError` (same house style as speakers/persons).
- **Export = a viewer route** (`hushai-viewer/src/export.rs`, `GET /api/devices/{id}/export.mp4?from&to&kind`):
  reuses `remux.rs` per-segment TS, piped through one ffmpeg into a fragmented MP4 — the viewer owns
  ffmpeg + the blob cache, so it isn't proxied. `proxy.rs::is_backend_path` was extended with `/v1/devices*`.
- **Deletion safety (load-bearing — see the doc + `devices.rs` comments):** ① device delete is ONE
  tx that NULLs `speakers/persons.first_seen_device_id` (NO-ACTION FKs that would otherwise 23503) under
  the speaker+person advisory locks, then `segments` (cascades children) → `streams` → `sessions` →
  `devices`; ② day/bulk/retention deletes touch `segments` only (no stream/session GC — races live
  ingest); ③ **blobs are reclaimed AFTER commit** by `storage::reclaim_blobs`, which re-checks each
  content hash against the live DB (content-addressed blobs can be shared) and unlinks the unreferenced
  ones — a crash only orphans a GC-able blob, never dangles a row; ④ retention deletes "fully older"
  footage (end ≤ now−N days), idempotent, runs at startup + every `RETENTION_SWEEP_SECONDS` (6 h);
  ⑤ the worker treats a segment vanishing mid-delete as a benign skip (`claim::segment_exists`).
- **Tests:** `hushai-backend/tests/devices.rs` (cascade, FK-block fix, blob ref-count, day bucketing,
  retention idempotency). Verified end-to-end over HTTP incl. real blob reclamation from disk.

## Fast-follows (not yet done)

- **Live token revocation** is restart-based (`DEVICE_TOKENS` edit + restart). A DB-backed
  `device_tokens` table (the `auth.rs` doc anticipates it) would allow hot revocation — not yet built.
- **Speaker thresholds are uncalibrated** (`Issues/unfinished/speaker-dedup-calibration-followups.md`):
  the de-dup overhaul above ships sensible *guesses*; tune `SPEAKER_MATCH_THRESHOLD` / `SPEAKER_MINT_DISTANCE_FLOOR`
  / `SPEAKER_KNN_*` / `VAD_*` against real **noisy** captures (the prior 0.5/0.84 numbers were clean-room
  `say` voices). Target: identities-minted-per-true-speaker → ~1.0 while attribution accuracy holds.
- **Existing duplicate backlog is not auto-collapsed** (by design — the user chose "fix going forward
  only"). It surfaces under the app's "Clean up voices" for manual one-tap merge; `recluster-deep` can
  be run manually over the whole catalog when wanted.
- **No scheduled deep-heal job.** The engine + endpoint exist; a `local_dev` launchd/cron wrapper
  (mirroring `partition_maintenance.{sh,plist}`) was intentionally left unwired so it stays opt-in.
- **VAD detector is constructed per segment** (the 0.6.8 sherpa wrapper exposes `clear()` but not the
  C-API `Reset`, so per-call construction avoids LSTM state bleed). Cheap for a ~2 MB model; pool it
  only if profiling says so.
- ~~**Worker erroring on video-only segments**~~ — the AUDIO path filters to `media_type IN (1,3)`
  (video has no audio to transcribe). VIDEO/MUXED segments are now processed by the **vision path**
  (`media_type IN (2,3)`, separate queue) — see "Vision pipeline" above. Genuinely-broken/incomplete
  video blobs (no `moov`) decode to no frames and are skipped cleanly (no error, no rows).
- Android: crash-durable on-disk retry queue, TLS, Doze/battery hardening, shared-proto golden-vector CI.
- The multi-agent adversarial review of the Android client stalled; re-run if deeper scrutiny is wanted.
- Nothing in this work is committed yet — it's all uncommitted on `master` (incl. the worker fix above).

## Where to look

- Per-component detail: each crate's `README.md` (esp. `hushai-backend/README.md`, `hushai-android/README.md`).
- Session memories (auto-loaded): `hushai-android-task`, `hushai-android-build-env`, `hushai-transcription-rag`.
