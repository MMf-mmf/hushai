# Handoff — Speaker identity + sentiment + RAG attribution

**Status (2026-06-26):** Implemented all four phases (A–D) of
`Issues/unfinished/speaker-identity-sentiment-rag-attribution.md` and verified end-to-end on the
live local stack. **Not yet committed.** On-device Phase-D interaction is the one remaining
human step.

---

## TL;DR of what changed vs the ticket

The ticket's core premise — a single "ECAPA-TDNN, 192-dim, raw-waveform" ONNX model — **does not
exist** (fbank is never baked into ONNX graphs; the only 192-d model with turnkey fbank is NVIDIA
TitaNet, not ECAPA). Decision made with the user: use **sherpa-onnx (`sherpa-rs`) + TitaNet-large
(192-d)**. sherpa does the mel-fbank front-end in C++, so we pass raw 16 kHz f32 PCM — **no
hand-rolled Rust front-end** (the ticket's biggest risk, eliminated). Schema stays `vector(192)`.

Other ticket corrections that the code follows instead of the ticket text:
- Vectors bind **natively** via `pgvector::Vector` (no `to_pgvector_text`; sqlx 0.9, not 0.8).
- It's `rig = "0.37"` (pulls `rig-core 0.38.2` transitively), not `rig-core 0.38.2` directly.
- Migration is **`0006`** (`0005_viewer_timeline_index.sql` already existed).
- `transcript_sentences.sentiment`/`.emotion` already existed (no DDL); `speaker_id` is new in 0006.

---

## What was built, by phase

### Phase A — Sentiment (no migration, no ONNX)
- `hushai-worker/src/sentiment.rs` (new) — `SentimentClassifier` over local Ollama `llama3.2:3b`
  via rig; one batched per-segment prompt → strict `positive|neutral|negative` parse → `None`
  otherwise; wrapped in `SENTIMENT_TIMEOUT_MS` (→ NULL on timeout). Gated by `SENTIMENT_ENABLED`.
- `chunk.rs` `Sentence` gained `speaker_id`/`sentiment`/`emotion` (`emotion` always `None`).
- `process.rs` classifies the segment text once and stamps it onto every sentence; the
  `write_transcript` INSERT writes `sentiment`/`emotion`.
- `config.rs`/`lib.rs`/`.env.example`: `LLM_OLLAMA_BASE_URL`, `SENTIMENT_MODEL`,
  `SENTIMENT_ENABLED`, `SENTIMENT_TIMEOUT_MS`.

### Phase B — Speaker-ID MVP
- **Model:** `./models/nemo_en_titanet_large.onnx` (101 MB, **gitignored**, sha256
  `d51abcf31717ef28162f26acb9d44dd4127c3d44c9b8624f699f3425daca8e77`; download URL + sha in
  `hushai-worker/.env.example`).
- **Dep:** `sherpa-rs = { version = "0.6.8", default-features = false, features = ["download-binaries"] }`
  in `hushai-worker/Cargo.toml` (fetches a prebuilt sherpa-onnx native lib once at build; runtime
  offline).
- **Migration** `hushai-backend/migrations/0006_speaker_identity.sql`: adds
  `transcript_sentences.speaker_id` (**text**, denormalized) + `…_speaker_time_idx`; `speakers`
  (global catalog, `centroid vector(192)`, `n_samples`, `display_name`, `first_seen_device_id`);
  `speaker_segments` (monthly RANGE-partitioned raw 192-d voiceprints) + the
  `ensure_/drop_speaker_segment_partitions` helpers.
- **Worker:** `speaker.rs` (`SpeakerEmbedder` = `Arc<Mutex<EmbeddingExtractor>>` — sherpa's
  `compute_speaker_embedding` is `&mut self`; L2-normalizes; refuses non-finite output),
  `vad.rs` (voiced-duration gate via whisper timestamps + multi-speaker first/second-half refusal +
  math helpers), `speaker_match.rs` (online global match-or-mint **inside `write_transcript`'s tx**:
  `pg_advisory_xact_lock(0x6873_7370_6b72)` → read prior `speaker_segments` BEFORE the transcript
  DELETE for idempotency → match-or-mint running-mean centroid → upsert).
- **RAG:** `retrieve.rs` `Filters`/`Source` gain `speaker_id`; `nearest()` adds
  `AND ts.speaker_id = ANY($::text[])` + ef_search bump; new `list_by_speaker` (exhaustive,
  non-semantic). `routes.rs` resolves `speaker_name`→ids with the precedence/failure-mode contract
  and routes `exhaustive:true`. `speakers.rs` (name↔id resolution). `llm.rs` prefixes each passage
  with the speaker name.
- **Backend:** `speakers.rs` (runtime sqlx, NOT macros): `GET /v1/speakers`,
  `PATCH /v1/speakers/{id}`, `POST /v1/speakers/{id}/merge`, `GET /v1/speakers/{id}/sample-audio`
  (reconstructs the playable mp4 by prepending fMP4 init; path-traversal guarded). All on the
  bearer-authenticated sub-router. New `IngestError::{NotFound,BadRequest}`.

### Phase C — Recluster
- Backend `POST /v1/speakers/recluster` (in `speakers.rs`): agglomerative single-linkage over
  centroids by cosine distance; canonical = the named member (else most-sampled); merges others
  (repoint `transcript_sentences` + `speaker_segments`, weighted+renormalized centroid, summed
  `n_samples`, delete losers); **skips clusters with ≥2 distinct names** so a human label is never
  silently destroyed. Takes the same advisory lock as the worker matcher.

### Phase D — Android "Voices" screen
- `net/SpeakersClient.kt` (OkHttp + org.json, backend url+token from Settings): list / setName /
  merge / downloadSample.
- `ui/VoicesScreen.kt`: list speakers + sample utterances, name field + Save (PATCH), merge dropdown,
  "Play sample" (downloads bearer-protected audio to cache → `MediaPlayer`).
- `MainActivity.kt`: a `Screen` enum toggle + `BackHandler` (no nav framework added);
  `CaptureScreen.kt` gained an `onOpenVoices` button.

### Docs / ops kept current
- `AGENTS.md` updated (component rows, schema/invariants, the new migration + model-provisioning
  note, the runtime-sqlx-for-speakers note).
- `local_dev/partition_maintenance.sh` now also runs `ensure_/drop_speaker_segment_partitions`
  (the retention call is the **privacy purge** for raw voiceprints).
- Memory: `hushai-speaker-id-sentiment.md` (model choice, calibration, in-tx match/mint, the
  stale-migration dev-env gotcha).

---

## Verification evidence (all on the live stack)

- **Phase A:** synthetic positive/negative control clip → `SELECT DISTINCT sentiment` returns **both
  `positive` and `negative`**, labels track content. ✓ (AC-A)
- **Phase B speaker split:** interleaved 2-voice fixture (macOS `say` Daniel + Samantha) → **exactly
  2 distinct `speaker_id`** cleanly separated by voice; NULL on short/boundary segments; centroid
  cosine distance **0.84 > `SPEAKER_MATCH_THRESHOLD` 0.5**; centroids L2-normalized. ✓ (AC-B)
- **Cross-device:** same audio under a 2nd device → maps to the **same 2 IDs** (n_samples 6→12),
  not 4. ✓
- **Idempotency (strict):** reprocess one segment → `transcript_sentences` count, `speaker_id`,
  `speaker_segments` count (1), and `speakers.n_samples`+`centroid` (md5) all **byte-identical**. ✓
- **Backend endpoints:** `GET /v1/speakers` (with bounded sample utterances), `PATCH` name (persists),
  auth 401 / unknown-id 404, `POST .../merge` (repoints both tables, sums n, preserves name, deletes
  loser), `sample-audio` → valid playable 2.06s h264+aac mp4. ✓
- **RAG attribution:** `speaker_name="Bob"` → answer + sources **all Bob, zero cross-attribution**;
  Alice's content filtered to Bob → "I don't have information"; unknown name → empty sources (not
  error); `exhaustive:true` → `list_by_speaker` (not pruned); no filter → both speakers appear. ✓
- **Phase C:** forced 12-way over-split → recluster → back to **2**, human name survived on the
  canonical id, transcripts bulk-remapped. ✓
- **Phase D:** `./gradlew :app:assembleDebug` **green** (Kotlin compiled). On-device play/name/merge
  is the [HUMAN] step.
- **Regression:** full workspace `cargo test` green — backend 9, rag 6, viewer 5, worker 21, 0 fails.

---

## Current environment state

- Running: `ollama serve` (with `mxbai-embed-large` + `llama3.2:3b`), `hushai-backend` (:8080),
  `hushai-rag` (:8090). **Worker is stopped.** (Logs under the session scratchpad.)
- DB has leftover test data: device `cam-oversplit` (2 speakers, one named "Daniel") + speakers the
  worker minted while draining the real `android-019efbfe…` backlog (~3068 pending segments still
  there — the worker reprioritization used tiny `capture_start` only on test devices). Clean test
  devices with the same pattern as `tests/worker_db.rs::cleanup` if desired.
- Dev-env gotcha hit + fixed this session: the backend binary embeds migrations at compile time, so
  a new migration on disk needs a recompile (`touch hushai-backend/src/lib.rs && cargo build -p
  hushai-backend`) or startup fails "migration N … missing in the resolved migrations".

---

## What remains / next steps

1. **Commit** (not done — was waiting on your call). Plan was separate commits per phase A→B→C→D;
   could also squash. Note the model is gitignored (don't commit the 101 MB `.onnx`).
2. **[HUMAN] on-device Phase D:** open the Voices screen on the phone, play a sample to ID a voice,
   name it, merge a duplicate; confirm a later `/v1/rag/query` attributes correctly.
3. **Optional supporting unit tests** the ticket lists as "run last" (matcher reuse-vs-mint, etc.) —
   not added; behavior is already proven by the live E2E above. `worker_db.rs::cleanup` was extended
   to `DELETE FROM speakers`.
4. **Committed fast-follow (separate ticket):** rolling 8–10s contiguous-speech window assembly — the
   biggest remaining accuracy lever. Deferred: at-rest voiceprint encryption + consent; acoustic
   `emotion` model; backfilling pre-feature history.

## How to resume the stack

```bash
# Ollama (if not running): ollama serve  (needs mxbai-embed-large + llama3.2:3b)
cd "/Users/mf/Documents/Rust_Code/Rig AI Agent"
cargo run -p hushai-backend          # :8080  (loads .env: DEVICE_TOKEN=dev-secret-token)
SPEAKER_MODEL_PATH="$PWD/models/nemo_en_titanet_large.onnx" cargo run -p hushai-worker
cargo run -p hushai-rag              # :8090
# Android: cd hushai-android && JAVA_HOME=/opt/homebrew/opt/openjdk@17 \
#   ANDROID_HOME=~/Library/Android/sdk ./gradlew :app:assembleDebug
```
