**Title:** `[hushai-worker + hushai-backend + hushai-rag + hushai-android] - Speaker identity (anonymous-now, named-later, cross-device) + sentiment on the transcript pipeline, surfaced in RAG attribution`

- **Description**:

  Make the always-on capture→transcribe→embed→RAG stack *smarter* by deriving two new
  per-utterance signals and wiring them into RAG so it can attribute quotes to a **named
  person across devices**:

  1. A **speaker identity** (`speaker_id`) — anonymous now (a minted UUID), human-named later.
     This is the headline feature: it lets RAG answer **"what did I say"** / **"what did Bob
     say"** grounded in one person's utterances. The system never has names up front — it
     **mints an anonymous voice ID per distinct voice it hears**, and a human later attaches a
     name ("this voice is Bob") via the app; RAG then resolves the name → voice IDs.
  2. A **sentiment** label (`positive | neutral | negative`) per utterance, derived from
     transcript text via the in-stack local Ollama LLM. The existing `emotion` column stays a
     NULL-ready slot (acoustic prosody is a deliberate non-goal for this ticket).

  This is a sibling to `Issues/voice-assistant-wakeword-rag.md` (which does **on-device** owner
  voice *verification*). That ticket and this one share the "voice biometrics" idea but **do
  not overlap in code**: that one verifies the owner on the phone; this one discovers, clusters,
  and labels *every* voice **server-side** in the worker, for retrieval/attribution.

  **Resolved scope decisions (from the design review):** ship all four phases A–D; use
  **per-segment** speaker embeddings in this ticket with **rolling multi-segment windows
  committed as the immediate next ticket** (not part of this one); store voiceprints
  **server-side** with retention + deletion purge (at-rest encryption + consent deferred);
  speaker identity is **cross-device / global** (the same person on two devices is one ID).

  ---

  ### The pipeline today (grounded)

  `hushai-worker/src/process.rs:process_segment` is a straight chain:
  `media::load_segment` → `media::extract_pcm` (16 kHz mono f32 `Vec<f32>`) →
  `transcriber.transcribe(pcm)` (**note:** `pcm` is *moved* here at `process.rs:25`) →
  `chunk::chunk_into_sentences` → `embedder.embed` → `write_transcript`. `write_transcript`
  (`process.rs:54-128`) opens one tx, runs `DELETE FROM transcript_sentences WHERE segment_id=$1`
  as the **first** statement, then a chunked multi-row `INSERT` (8 bind params/row), then marks
  `segment_transcription_status.status='done'`, then commits. Re-processing replaces, never
  duplicates.

  RAG (`hushai-rag/src/retrieve.rs:nearest` + `routes.rs:rag_query` + `llm.rs:build_prompt`)
  embeds the query, raises `SET LOCAL hnsw.ef_search` (default `Filters.ef_search=100`,
  `.max(top_k)` at `retrieve.rs:83`), runs an HNSW cosine (`<=>`) NN search over
  `transcript_sentences`, applies an optional `AND ts.device_id = $` filter
  (`retrieve.rs:102-103`), prunes by `RAG_DISTANCE_THRESHOLD` (~0.6), and asks local Ollama to
  answer only from survivors. Nullable text columns are read with
  `try_get::<Option<String>>(...).unwrap_or_default()` (`retrieve.rs:124-127`).

  There is **no speaker concept anywhere** server-side today. Usefully,
  `transcript_sentences.sentiment` and `.emotion` **columns already exist**
  (`0003_scalability.sql:40-41`, carried through the partitioned recreate) but are never
  written — so sentiment needs **no migration**, only a worker write. RAG binds on `:8090`
  (`hushai-rag/src/config.rs:38`).

  ### The 2-second reality (why v1 is a prototype, stated honestly)

  Segments are a **fixed wall-clock 2-second window** cut on a timer with **zero** alignment to
  speaker turns (Android `CaptureService.kt:346 SEGMENT_DURATION_US=2_000_000L`; feeder
  `--seg-seconds 2`). A raw 2s timer-cut segment often holds **< 1s of voiced speech** (room
  tone, dead air, mid-turn cuts). ECAPA-TDNN speaker embeddings degrade sharply below ~3s and
  are poor under ~1.5s, where same-speaker cosine spread can *exceed* cross-speaker spread — so
  errors are **bidirectional** (both over-split *and* mis-merge). (NB: the bundled Android Vosk
  x-vector README's "1.5s" is **not** applicable prior art — that is 1.5s of *post-VAD* speech
  clustered globally with PLDA, none of which we have.)

  So v1's honest claim is **per-segment speaker labeling that is a usable prototype for "ask
  Bob," not forensic diarization.** We compute one acoustic embedding over the segment's voiced
  speech and stamp that one `speaker_id` onto every sentence chunked from the segment, behind
  three mandatory accuracy guards:

  - **VAD / speech-duration gate:** estimate voiced-speech duration in the 2s PCM (proxy: span
    of whisper utterance timestamps `t0..t1`, or a cheap energy/ZCR gate). If voiced speech
    `< SPEAKER_MIN_SPEECH_SECS` (~0.8s) or whisper returned zero sentences, **skip speaker work**:
    leave `speaker_id` NULL, write nothing speaker-related. Embedding sub-second speech is the
    dominant accuracy killer; a NULL beats minting noise.
  - **L2-normalization:** L2-normalize every ECAPA vector before any cosine; store centroids
    already-normalized.
  - **Multi-speaker refusal:** embed the first vs second ~1s half of the segment and compare
    intra-segment cosine spread to `SPEAKER_SPLIT_THRESHOLD`; if the halves look like two voices,
    leave `speaker_id` NULL and **do not** fold the blended embedding into any centroid (a
    confidently-wrong attribution is worse than NULL, and a blended vector poisons the centroid
    for all future matches).

  **Committed fast-follow (separate next ticket, NOT in this one):** rolling 8–10s
  contiguous-speech window assembly (`segments.sequence` + `gap_before` already exist per
  `0001_init.sql:52,62`) is the single biggest accuracy lever and is the explicit next ticket.

  ### Speaker-ID strategy: online global centroid match, serialized, idempotent

  A new `hushai-worker/src/speaker.rs` mirrors `asr.rs:Transcriber` at the **Rust inference
  layer** only: `SpeakerEmbedder { session: Arc<ort::Session> }`, constructed once in
  `lib.rs:run()` beside `Transcriber::new`/`Embedder::new` (`lib.rs:79-80`), cloned per worker
  task, inference on `tokio::task::spawn_blocking`. It produces one 192-d ECAPA-TDNN embedding
  (L2-normalized). In `process_segment`, **borrow `&pcm` before the move** at `process.rs:25`
  (clone or restructure), apply the VAD gate + multi-speaker check, then embed.

  Identity assignment is **online incremental, GLOBAL (cross-device), serialized, idempotent**,
  executed inside the `write_transcript` tx:

  - **Global serialization:** take `pg_advisory_xact_lock(<constant speaker-space key>)` at the
    top of the speaker step inside the tx. Because identity is cross-device there is one global
    speaker space, so a **single global lock** serializes match-or-mint. This also fixes the
    cold-start duplicate-mint race under `WORKER_CONCURRENCY=2` (`config.rs:37`). **Known
    tradeoff:** a global lock serializes *all* speaker assignment — fine at single-user/local
    volume, a scaling bottleneck to revisit later (it is the simple correct choice given
    cross-device global comparison).
  - **Match or mint:** `SELECT speaker_id, centroid, n_samples FROM speakers` (the whole small
    catalog — global, not per-device), cosine-compare the normalized embedding to each centroid.
    If best **distance ≤ `SPEAKER_MATCH_THRESHOLD`**, reuse that ID and update the running-mean
    centroid (`centroid=(centroid*n+emb)/(n+1)`, re-normalize, `n_samples++`); else **mint** a
    new `speaker_id` (UUIDv7), `INSERT` a `speakers` row seeded with this embedding. Minted IDs
    are anonymous (`display_name` NULL).
  - **Idempotency (critical):** *before* the transcript `DELETE`, read the segment's prior
    assignment from `speaker_segments` (the durable source of truth keyed by `segment_id` — NOT
    `transcript_sentences`, whose rows the DELETE erases). If a row exists, **reuse** that
    `speaker_id` and **skip** the running-mean update (centroid + `n_samples` unchanged).
    UPSERT the raw 192-d vector to `speaker_segments` by `segment_id` so reprocessing never
    appends duplicate raw vectors (which would silently bias a future recluster).

  **Threshold honesty:** `SPEAKER_MATCH_THRESHOLD` is **cosine DISTANCE** (`= 1 − similarity`).
  Short-utterance ECAPA same-speaker similarity often sits 0.5–0.7, so an over-tight default
  (e.g. 0.25 distance) would over-mint a new ID on nearly every segment (hundreds of IDs for two
  people — "2 distinct speakers" would pass for the *wrong* reason). v1 ships with a deliberately
  loose starting value and a loud `WARN "speaker matching uncalibrated"`; **empirical tuning
  (sweep distance, watch ID-count vs known-speaker-count on the fixture) is an in-scope
  deliverable**, not a follow-up.

  ### Sentiment strategy: lexical, via the in-stack Ollama LLM

  A new `hushai-worker/src/sentiment.rs` reuses the same rig Ollama client surface that
  `hushai-rag/src/llm.rs:42` already runs against the locked `rig-core 0.38.2`
  (`client.agent(model).preamble(...).build()` → `.prompt(...)`), built from `OLLAMA_BASE_URL`
  with `llama3.2:3b` (already running for RAG). **Granularity (pinned):** classify **per
  segment**, then denormalize the one label onto every sentence of that segment — same pattern
  as `device_id`. The LLM sees only ~2s of text, so one label per segment is the honest unit and
  it avoids the fragile "align N output lines to N sentence rows" parse. One batched prompt per
  segment ("classify the overall sentiment as exactly one of positive/neutral/negative"), strict
  parse (accept only the three labels, else NULL), wrapped in `SENTIMENT_TIMEOUT_MS` (~3000ms) so
  a slow LLM never stalls the always-on pipeline (on timeout → NULL). Writes the existing
  `sentiment` column (no migration). `emotion` stays NULL.

  ### Anonymous-ID-now / name-later / cross-device flow into RAG

  1. Worker mints anonymous `speaker_id` UUIDs and denormalizes them onto
     `transcript_sentences.speaker_id`. **Type contract (pinned):** `speakers.speaker_id` is
     `uuid`, but `transcript_sentences.speaker_id` is **`text`** (matching `device_id`, which is
     text per 0003). The worker writes `speaker_id.to_string()`; RAG resolves names → `Vec<Uuid>`
     then binds **stringified** IDs as `text[]` so `AND ts.speaker_id = ANY($1::text[])` cannot
     hit a uuid-vs-text runtime type error (these are unchecked `QueryBuilder` queries — no
     compile-time catch).
  2. A human assigns names via authenticated backend endpoints (`GET /v1/speakers`,
     `PATCH /v1/speakers/{id}`, `POST /v1/speakers/{id}/merge`) — and, in Phase D, via an Android
     "Voices" screen with audio-snippet playback so a human can identify who's who **by ear**.
     Because identity is **cross-device**, name resolution is **global** (no device scope).
  3. RAG resolves name → IDs and filters on the **denormalized** column (never a JOIN — mandatory
     per the 0003 recall-cliff lesson). `routes.rs:QueryFilters` and `retrieve.rs:Filters` gain
     `speaker_id: Option<Vec<String>>` and `speaker_name: Option<String>`; the new `Source` field
     is `speaker_id: Option<String>` via `try_get::<Option<String>>(...).unwrap_or_default()`;
     `llm.rs:build_prompt` includes the resolved `display_name` (or "unknown speaker") per context
     line for attribution. **Two retrieval paths:** the semantic `nearest()` gets
     `AND ts.speaker_id = ANY($::text[])` for *topical* "what did Bob say about X" (a selective
     speaker predicate on the HNSW iterative scan **raises recall-cliff risk** for a sparse
     speaker — bumping `ef_search` mitigates but does **not** eliminate it); a new **non-semantic**
     `list_by_speaker` (filter speaker + optional device + time, `ORDER BY start_unix_nanos`, no
     vector order, no distance prune, backed by a new btree index) is the path for **exhaustive
     "everything Bob said"** so it is never truncated by the cliff or the 0.6 prune.

  ### Data-history policy (v1 non-goal, stated)

  Migration 0005 adds `speaker_id` NULL to the partitioned parent. v1 does **not** backfill —
  pre-existing `transcript_sentences` rows keep `speaker_id=NULL` forever; RAG treats NULL as
  "unknown speaker" (never matches a name filter, never errors). Consequence stated plainly:
  "what did Bob say" silently **excludes all pre-feature history**. Backfilling (re-deriving PCM
  from `segments.blob_uri` and re-embedding) is a non-goal because it would feed the online
  matcher out of chronological order and perturb centroids. The existing single-camera corpus
  (`local_dev/captures/` device `019efbfe`, `.feed_work` sidecars cam-A…cam-keepup) is
  single-speaker and will yield ~1 `speaker_id` per voice if reprocessed — **expected, not a
  regression**.

  ### Privacy / voice-biometric posture (resolved: store + retention/purge)

  The 192-d ECAPA vectors in `speakers.centroid` and `speaker_segments.embedding` **are voice
  biometrics** — and this is the **first server-side persistence of voice biometrics** in the
  system (a deliberate posture change from `Issues/voice-assistant-wakeword-rag.md:21-27`, which
  keeps voice-ID on-device). v1 **stores them server-side** and must: add a `speaker_segments`
  retention helper (`drop_speaker_segment_partitions_before`, mirroring transcript retention);
  have device-deletion / person-deletion **purge** centroids + raw vectors; and acknowledge the
  cascade gap (deleting a `segments` row cascades its `speaker_segments` rows but does **not**
  recompute the running-mean `speakers.centroid`, so a deleted recording's voiceprint lingers in
  the centroid until a recluster). **At-rest encryption and consent are explicitly deferred** to
  a follow-up.

  ---

  ### Changes by component

  **Migration — new `hushai-backend/migrations/0005_speaker_identity.sql`** (auto-applied on
  worker/RAG startup via the existing `sqlx::migrate!("../hushai-backend/migrations")`):

  - `ALTER TABLE transcript_sentences ADD COLUMN speaker_id text` (denormalized like `device_id`;
    altering the partitioned parent covers all partitions). **`text`, not `uuid`**, to match the
    existing text `device_id` and avoid a uuid-vs-text `ANY()` runtime type error in RAG. NULL for
    not-yet-assigned / silent / multi-speaker / pre-feature rows. No backfill.
  - `CREATE INDEX transcript_sentences_speaker_time_idx ON transcript_sentences (speaker_id, start_unix_nanos)`
    — leads with `speaker_id` (not `device_id`) because identity is **cross-device**; this backs
    the non-semantic `list_by_speaker` path. (Optionally also `(device_id, speaker_id, start_unix_nanos)`
    if device-scoped speaker queries become common.) **It does not back the semantic speaker
    filter** — that query rides the HNSW index and applies the speaker predicate as a scan filter.
  - `CREATE TABLE speakers (speaker_id uuid PRIMARY KEY, centroid vector(192), n_samples bigint NOT NULL DEFAULT 0, display_name text NULL, first_seen_device_id text NULL REFERENCES devices(device_id), created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now())`.
    Global catalog (**not** device-scoped — cross-device); `first_seen_device_id` is metadata only.
    Not partitioned (low row count). `centroid` is **`vector(192)` — its own dim, NOT the 1024
    text-embedding space**; bind via `to_pgvector_text` + `::vector`, never the pgvector Rust type
    (pgvector 0.4 targets a different sqlx than the workspace's 0.8). Centroids stored
    L2-normalized.
  - `CREATE INDEX speakers_name_idx ON speakers (lower(display_name))` — **global** name→ID
    resolution (no device scope, because cross-device).
  - `CREATE TABLE speaker_segments (id bigserial, segment_id uuid NOT NULL REFERENCES segments(segment_id) ON DELETE CASCADE, device_id text NOT NULL, speaker_id uuid NULL, start_unix_nanos bigint, end_unix_nanos bigint, embedding vector(192), created_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (id, created_at)) RANGE PARTITION BY (created_at)`.
    One row per processed speech segment holding the raw 192-d embedding (the authoritative
    per-segment `speaker_id` = idempotency source of truth + the substrate for Phase C recluster).
    `ON DELETE CASCADE` matches the 0004 convention.
  - Enforce **one row per `segment_id`** in `speaker_segments`: since it's partitioned by
    `created_at` a plain `UNIQUE(segment_id)` isn't possible, so `DELETE FROM speaker_segments
    WHERE segment_id=$1` then INSERT inside the write tx (state this in the file).
  - `CREATE INDEX speaker_segments_segment_id_idx ON speaker_segments (segment_id)` (parent index
    propagates) so the `ON DELETE CASCADE` and the reprocess lookup don't scan every monthly
    partition (mirrors `transcript_sentences_segment_id_idx`).
  - `ensure_speaker_segment_partitions(N)` + `drop_speaker_segment_partitions_before(...)` helpers
    mirroring the transcript partition/retention helpers; call `ensure_speaker_segment_partitions(3)`
    from `lib.rs:run()` next to `ensure_transcript_partitions(3)` (`lib.rs:66`). The drop helper is
    the privacy retention mechanism for raw voiceprints.
  - **No sentiment/emotion DDL** — those columns already exist (`0003:40-41`).
  - **Migration-safety header comment:** `ADD COLUMN ... NULL` on a partitioned parent is
    metadata-only, but `CREATE INDEX` on a partitioned parent is **not** concurrent and locks per
    partition; on a production-sized corpus the speaker/time index should be built out-of-band
    `CONCURRENTLY` per partition before deploy. Fine for the dev corpus.

  **`hushai-worker`:**

  - New `speaker.rs`: `SpeakerEmbedder { session: Arc<ort::Session> }`; `::new(model_path)` loads
    the ECAPA ONNX, inference returns a 192-d vector, then L2-normalize. **Feature-extraction fork
    must be resolved before coding** (see Model decisions): prefer an export whose ONNX graph
    accepts raw `[1,N]` 16 kHz waveform (fbank baked in); if not, a hand-rolled Kaldi-compatible
    mel-filterbank Rust pre-step is its own sub-task with a test asserting Rust-computed features
    match a reference vector from the export's training front-end.
  - New `vad.rs` (or fns in `speaker.rs`): the VAD/speech-duration gate + the multi-speaker
    first-half/second-half refusal check described above.
  - New `speaker_match.rs` (or fns in `speaker.rs`): inside the `write_transcript` tx — take the
    **global** `pg_advisory_xact_lock`; read prior assignment from `speaker_segments WHERE
    segment_id=$1` **before** the transcript DELETE (reuse + skip update if present); else fetch
    the global centroid set, L2-cosine compare, reuse-or-mint, update running-mean centroid /
    `n_samples`, `INSERT speakers` (UUIDv7) on mint; UPSERT `speaker_segments` by `segment_id`.
    Reuse `embed::to_pgvector_text` for `vector(192)` binds. `WARN` once while
    `SPEAKER_MATCH_THRESHOLD` is at the uncalibrated default.
  - New `sentiment.rs`: `SentimentClassifier` over `OLLAMA_BASE_URL` via the rig Ollama client,
    one batched per-segment prompt, strict 3-label parse → NULL otherwise, `SENTIMENT_TIMEOUT_MS`
    guard, gated by `SENTIMENT_ENABLED`.
  - `chunk.rs`: extend `Sentence` with `speaker_id: Option<String>` and `sentiment:
    Option<String>` (`emotion` stays `None`); `chunk_into_sentences` keeps producing text/timestamps,
    `process_segment` fills the segment-level `speaker_id` + `sentiment` onto all sentences.
  - `process.rs`: borrow `&pcm` before the `transcribe(pcm)` move (`process.rs:25`); run VAD +
    multi-speaker + ECAPA embed; run timeout-guarded sentiment; do the advisory-locked match +
    `speaker_segments` UPSERT **inside** `write_transcript`'s tx; extend the INSERT to 11 bind
    params/row (`speaker_id`, `sentiment`, `emotion`) — `11*ROWS_PER_INSERT(1000)=11000`, well
    under 65535. Read prior `speaker_id` before the DELETE.
  - `config.rs` + `.env.example`: `SPEAKER_MODEL_PATH` (default `./models/ecapa-tdnn.onnx`, beside
    `ggml-base.en.bin`), `SPEAKER_MATCH_THRESHOLD` (cosine distance; ship loose, tune in-ticket),
    `SPEAKER_MIN_SPEECH_SECS` (~0.8), `SPEAKER_SPLIT_THRESHOLD`, `SENTIMENT_MODEL` (`llama3.2:3b`),
    `SENTIMENT_ENABLED` (true), `SENTIMENT_TIMEOUT_MS` (~3000), via the existing `opt()/parse()`
    pattern; thread through `lib.rs:run()`.
  - `lib.rs:run()`: construct `SpeakerEmbedder` + `SentimentClassifier` once (beside
    `Transcriber::new`/`Embedder::new`, `lib.rs:79-80`), clone into each `worker_loop` task; add
    `ensure_speaker_segment_partitions(3)` next to `ensure_transcript_partitions(3)`.
  - `Cargo.toml`: add `ort` + `ndarray` as **direct** deps pinned to the already-locked
    `ort 2.0.0-rc.9` / `ndarray 0.16.1`, and **explicitly pick the `ort` feature**: `download-binaries`
    (dev — one-time build-time fetch of `libonnxruntime`, cached) **or** no-download + vendored lib
    via `ORT_LIB_LOCATION`/pkg-config (air-gapped). These crates are in `Cargo.lock` only
    transitively under rig's currently-**unused** `fastembed` feature and are **not compiled
    today** (proof: the `ort-sys` lock entry's only dep is `pkg-config`); adding `ort` as a direct
    dep runs `ort-sys`' `build.rs` for the first time. **This is a first-class provisioning task,
    not a footnote** (see Model decisions).

  **`hushai-rag`:**

  - `retrieve.rs:Filters` gains `speaker_id: Option<Vec<String>>`; `nearest()` appends `AND
    ts.speaker_id = ANY($::text[])` (stringified), adds `ts.speaker_id` to the SELECT + `Source`
    (`Option<String>`, `unwrap_or_default()`), and raises effective `ef_search` beyond `.max(top_k)`
    when a speaker filter is present (**documented as mitigation, not elimination**, of the sparse-
    speaker recall cliff).
  - `retrieve.rs`: new `list_by_speaker(pool, speaker_ids, device_id?, time-range?, limit)` —
    `WHERE speaker_id = ANY($::text[]) [AND device_id=$] [AND time]`, `ORDER BY start_unix_nanos`,
    no vector order, no distance prune — backed by `transcript_sentences_speaker_time_idx`. The
    path for exhaustive/attribution "what did Bob say."
  - `routes.rs:QueryFilters` gains `speaker_id: Option<Vec<String>>` and `speaker_name:
    Option<String>`. Resolve `speaker_name` → `Vec<Uuid>` **globally** via `speakers` (cross-device).
    **Pin the failure-mode contract:** unknown name → empty sources (not error, not unfiltered);
    ambiguous name → union all IDs; `speaker_name` + `speaker_id` both given → `speaker_id` wins;
    NULL `speaker_id` rows never match a name filter. Route exhaustive attribution → `list_by_speaker`;
    topical speaker queries → `nearest` with the `ANY` filter.
  - `llm.rs:build_prompt`: include the resolved `display_name` (or "unknown speaker") per context
    line so the model attributes quotes ("Bob said: …") and doesn't invent attributions for
    NULL-speaker rows.
  - `state.rs` (or new `speakers.rs`): the global name→ID resolution query (small SELECT against
    `speakers`). Migrations auto-apply on RAG startup, so the column/tables exist.

  **`hushai-backend`** (new derived server-side surface — no proto/contract change):

  - New module `speakers.rs` (register in `lib.rs`), handlers following `ingest.rs:post_segment`
    shape (`async fn(State(AppState), ...) -> Result<Json<...>, IngestError>`).
  - `GET /v1/speakers` — list discovered speakers (`speaker_id`, `display_name`, `n_samples`, a few
    **bounded** sample utterances via a `LATERAL`/correlated `LIMIT 3` per `speaker_id` over the
    partitioned `transcript_sentences` using `transcript_sentences_speaker_time_idx`, not an
    unbounded scan). First JSON-returning read endpoint in the crate. (Cross-device, so device
    filter is optional, not required.)
  - `PATCH /v1/speakers/{id}` — set `display_name` (idempotent upsert).
  - `POST /v1/speakers/{id}/merge {into: <other_id>}` — manual merge of two IDs for the same person
    (over-splitting is **guaranteed** by the conservative matcher, so humans hit duplicate IDs
    immediately): bulk `UPDATE transcript_sentences SET speaker_id=<canonical> WHERE
    speaker_id=<losing>` (all partitions, denormalized text column), repoint `speaker_segments`,
    combine centroids weighted by `n_samples` (re-normalize) + sum `n_samples` on the survivor,
    preserve the survivor's `display_name`, `DELETE` the losing `speakers` row. (Splitting one ID
    into two is the Phase C recluster's job, not a manual endpoint.)
  - `GET /v1/speakers/{id}/sample-audio` (**Phase D enabler**) — return a short audio snippet for a
    speaker (decode a stored speech segment for that `speaker_id` from `segments.blob_uri`) so a
    human can identify the voice **by ear** when naming. 2s sample *text* alone is too weak.
  - `routes.rs`: register speaker routes on an **authenticated** sub-router mirroring ingest
    (`auth::require_bearer`); keep them out of the unauthenticated health merge.
  - `db.rs`: `list_speakers`, `set_speaker_name`, `merge_speakers`. **Build-ordering trap:** `db.rs`
    uses compile-time `sqlx::query!` macros with a committed `.sqlx` cache — new queries against the
    new `speakers` table **fail to compile** unless the dev DB has 0005 applied at build time and
    `cargo sqlx prepare` is re-run, **or** the new handlers use **runtime** `sqlx::query`/`query_as`
    (no macro). State which path is taken; **runtime queries are the lower-friction choice** for
    these new endpoints.

  **`hushai-android`** (Phase D — the end-user naming UI):

  - New "Voices" screen mirroring `ui/CaptureScreen.kt`: list speakers from `GET /v1/speakers`
    (ID, sample utterances, count), **play the sample-audio snippet** to ID the voice by ear, type a
    name and `PATCH`, and a merge action. New JSON net client mirroring `net/Uploader.kt` over the
    existing OkHttp 4.12.0, reusing url/token from the `config/Settings.kt` DataStore. Cross-device,
    so the screen lists the global speaker catalog (device filter optional).

  ### Model decisions

  - **Speaker embedding — ECAPA-TDNN (192-dim)** (e.g. SpeechBrain `spkrec-ecapa-voxceleb` or
    3D-Speaker exported to ONNX, ~20 MB) via the `ort` crate (ONNX Runtime 2.0.0-rc.9). Why: 16 kHz
    mono = the pipeline's exact PCM (no resample); 192-d is cheap to centroid-match; runs offline at
    **runtime** from Rust on `spawn_blocking` with `Arc<Session>`. **Honesty correction:** the Rust
    inference code mirrors `asr.rs:Transcriber`, but the **native-lib story is categorically
    different** — `whisper-rs-sys` vendors + cmake-builds whisper.cpp from source (zero network at
    build, zero external binary at runtime), whereas `ort-sys` does **not** vendor `onnxruntime`; it
    either downloads a prebuilt lib (`download-binaries`) or needs a pre-installed lib
    (`pkg-config`/`ORT_LIB_LOCATION`). There is **no `libonnxruntime` and no `ecapa-tdnn.onnx` on this
    machine today**, so the speaker path **does not build out of the box** — provisioning is a
    first-class task. **Acceptance tension:** "no cloud calls" is **runtime**-offline; the easy dev
    path (`download-binaries`) makes a one-time **build-time** network fetch — strict build-time-offline
    needs a vendored lib + `ORT_LIB_LOCATION`. *Rejected:* (a) the bundled Android Vosk x-vector model
    (Kaldi nnet3 @ 8 kHz MFCC, no ONNX/Rust loader) — Android-only dead end server-side; (b)
    whisper.cpp tinydiarize (marks turn boundaries, emits no embeddings, different ggml model);
    (c) sherpa-onnx/sherpa-rs (turnkey but a second large native dep — reconsider only if the
    fbank+ECAPA path proves fiddly); (d) pyannote (Python — violates Rust-only/offline).
  - **Feature-extraction fork (resolve before coding `speaker.rs`):** most published ECAPA exports
    take Fbank features, not raw waveform. If the chosen export bakes fbank into the graph (accepts
    raw `[1,N]` 16 kHz waveform), `speaker.rs` is simple; if not, the "small Rust pre-step" is a
    from-scratch Kaldi-compatible mel-filterbank that must match the model's **training** front-end
    exactly or embeddings are garbage — a multi-day sub-task and the most likely place a latent Python
    dependency hides. **Pick a specific export, state whether fbank is baked in, scope the Rust
    pre-step + reference-vector test if needed, and verify zero Python at inference.**
  - **Sentiment — lexical via the in-stack rig Ollama API** (`llama3.2:3b`, already running), one
    batched per-segment prompt. Zero new model assets, fully offline, reuses the client surface
    already compiled in `llm.rs:42`. The **de-risked Phase A** ship. *Rejected:* an acoustic emotion
    ONNX model competes for `spawn_blocking` threads on a CPU-only 2s cadence — `emotion` stays a
    NULL-ready slot. **Known limitation:** lexical sentiment loses sarcasm/tone and will mostly emit
    "neutral" on flat camera-setup speech — hence the positive/negative control fixture in How to Test.
  - **Identity logic — online global running-mean centroid matching in the worker**, state in the
    Postgres `speakers` table, serialized via a global `pg_advisory_xact_lock` inside the write tx,
    VAD-gated + L2-normalized + multi-speaker-refused, raw per-segment vectors persisted to
    `speaker_segments` for the Phase C recluster. **Explicitly acknowledged:** greedy online first-
    match is **order-dependent** and SKIP-LOCKED claim ordering is nondeterministic, so minted
    `speaker_id`s are **not reproducible run-to-run** (documented, not hidden). Phase C reclustering
    heals over-splits of cleanly-separated IDs but **cannot** recover blended/sub-second embeddings,
    and without PLDA/score-norm it is the same raw-cosine metric at larger scope — necessary, not
    sufficient. The committed rolling-window next ticket is the bigger accuracy lever.

  ### Phasing (all in scope; recommend separate PRs per phase)

  - **Phase A — sentiment** (smallest; no migration, no ONNX): extend `chunk.rs:Sentence`, add
    `sentiment.rs` (timeout-guarded, per-segment), write the existing `sentiment` column. Ships value
    immediately, de-risks rig-completion-in-worker with zero native-lib risk; must **not** be gated on
    speaker-ID. Optionally expose `sentiment` in RAG `Source`.
  - **Phase B — speaker-ID MVP** (the bulk + the risky part): **prerequisite checklist first** (pin
    the ECAPA `.onnx` + sha + host; resolve the fbank-baked-vs-Rust-prestep fork; pick the `ort`
    feature + provision `libonnxruntime` for dev *and* air-gapped). Then migration 0005, `speaker.rs`
    (ort/ECAPA + VAD + multi-speaker refusal + L2-norm), the global-advisory-locked online match +
    `speaker_segments` UPSERT, RAG speaker filter + `list_by_speaker` + `Source` + prompt attribution +
    name-resolution failure-mode contract, backend `GET/PATCH/merge /v1/speakers` (runtime sqlx), plus
    the empirical `SPEAKER_MATCH_THRESHOLD` tuning loop. ~500+ lines across worker+backend+rag+migration
    touching high-stakes surfaces (auth on new endpoints, a partitioned-parent migration, the HNSW
    recall-cliff path) → warrants a deeper review.
  - **Phase C — accuracy heal:** periodic batch agglomerative reclustering over `speaker_segments` per
    the global space (admin endpoint or pg_cron) that merges over-split anonymous IDs, recomputes
    centroids, bulk-UPDATEs `transcript_sentences.speaker_id` via a remap **preserving any human-named
    canonical ID**. Honestly bounded (cannot fix blended/sub-second embeddings).
  - **Phase D — Android "Voices" screen + `GET /v1/speakers/{id}/sample-audio`:** the end-user naming
    UI with audio-snippet playback so a human IDs voices by ear, names them, and merges duplicates.

  **Committed fast-follow (separate ticket):** rolling 8–10s contiguous-speech window assembly for
  materially better diarization.

- **Acceptance Criteria**:
  - [ ] **(A)** Ingesting any speech recording + running the worker yields `transcript_sentences` rows
        with non-NULL `sentiment` in {positive, neutral, negative} for speech segments; NULL on
        unparseable/timeout. With the positive/negative control fixture, **more than one** distinct
        non-NULL label appears (a constant classifier fails this).
  - [ ] **(B)** Ingesting the committed interleaved ≥2-speaker fixture + running the worker yields rows
        with non-NULL `speaker_id` for clean single-speaker segments, **at least 2 distinct
        `speaker_id`** for that recording, and the distinct-ID count is within a **sane band** of the
        true speaker count (NOT hundreds — guards against an over-tight threshold passing for the wrong
        reason). Objective check: cosine distance between the two minted centroids exceeds
        `SPEAKER_MATCH_THRESHOLD`.
  - [ ] Segments that are silent / non-speech / below `SPEAKER_MIN_SPEECH_SECS` / flagged multi-speaker
        write `speaker_id=NULL`, do **not** mint a `speakers` row or fold a centroid, and do not crash
        the worker.
  - [ ] A `speakers` row exists per minted `speaker_id` with `n_samples>0`, `centroid` non-NULL and
        L2-normalized, `display_name` initially NULL, `first_seen_device_id` set.
  - [ ] `GET /v1/speakers` (Bearer) returns discovered `speaker_id`s with **bounded** sample utterances;
        `PATCH /v1/speakers/{id} {"display_name":"Bob"}` returns 200 and persists (re-GET shows it);
        `POST /v1/speakers/{id}/merge {"into":<other>}` repoints `transcript_sentences` +
        `speaker_segments`, combines centroid/`n_samples`, preserves the survivor's name, deletes the
        losing row.
  - [ ] **Cross-device:** the same voice captured under **two different `device_id`s** is matched to
        **one** `speaker_id` (not split by device), and a name set once resolves across both devices.
  - [ ] `POST /v1/rag/query` (`:8090`) with `filters.speaker_name="Bob"` returns an answer + sources where
        **every** source's `speaker_id` resolves to Bob and the answer reflects only Bob's utterances;
        asking for the **other** speaker's distinctive content filtered to Bob returns a no-information
        decline or only Bob's sources (no cross-attribution). Exhaustive "everything Bob said" routes
        through `list_by_speaker` and is **not** truncated by the 0.6 prune. Unknown `speaker_name` →
        empty sources (not an error, not unfiltered).
  - [ ] **Idempotency (strict):** reprocessing a segment leaves `transcript_sentences` row count
        unchanged, the segment's `speaker_id` unchanged, `speaker_segments` count for that segment
        unchanged (still 1), **and** `speakers.n_samples` + `centroid` byte-for-byte unchanged. No
        duplicate `speakers` row minted.
  - [ ] **Cold-start:** ingesting a brand-new voice's first segments under `WORKER_CONCURRENCY>=2` does
        **not** mint duplicate `speaker_id`s for the same first-seen voice (global advisory lock
        serializes mint).
  - [ ] **(C)** Running the batch recluster over a deliberately over-split set merges duplicate anonymous
        IDs, recomputes centroids, bulk-remaps `transcript_sentences.speaker_id`, and **preserves any
        human-assigned name** on the surviving canonical ID.
  - [ ] **(D)** The Android "Voices" screen lists discovered speakers, **plays a sample-audio snippet**
        per ID (`GET /v1/speakers/{id}/sample-audio`), lets a human name a voice (PATCH) and merge
        duplicates, reusing url/token from the Settings DataStore; `./gradlew :app:assembleDebug` green.
  - [ ] **Build/link:** `cargo build -p hushai-worker` **links `libonnxruntime`** on the target, and
        `SpeakerEmbedder::new` loads `ecapa-tdnn.onnx` and returns a 192-d vector for a 2s PCM fixture
        (separate from a generic "worker builds" that would pass while `ort` is unwired). Worker, RAG,
        backend all build + start; migration 0005 applies cleanly on startup; existing tests pass.
  - [ ] **Runtime offline:** no cloud calls; speaker model loads from `SPEAKER_MODEL_PATH`, sentiment +
        RAG use local Ollama. (Build-time offline holds only on the vendored-`libonnxruntime` path; the
        `download-binaries` dev path fetches the native lib once at build — stated, not hidden.)
  - [ ] **Privacy:** `drop_speaker_segment_partitions_before(...)` retention helper exists and works;
        device/person deletion purges centroids + raw vectors; the cascade gap (deleting audio doesn't
        scrub the running-mean centroid until a recluster) is documented.
  - [ ] **Pre-feature history:** existing single-camera corpus rows keep `speaker_id=NULL`; a name filter
        never matches NULL rows; "what did Bob say" correctly excludes pre-feature history (documented,
        not a bug). Reprocessing a single-camera capture yields ~1 `speaker_id` — expected.

- **How to Test** (real end-to-end on the live local stack; human-in-the-loop steps marked
  **[HUMAN]**; I'll drive every DB/curl/adb/build step myself):

  1. **(Phase A — sentiment, no speaker fixture)** **[HUMAN]** record a tiny control clip with one
     clearly **positive** line ("This is wonderful, I love it!") and one clearly **negative** line
     ("This is terrible, I hate this."), then mux with a black video track so the MUXED feeder works:
     `ffmpeg -f lavfi -i color=c=black:s=320x240:r=15 -i control.wav -shortest -c:v libx264 -c:a aac .../scratchpad/control.mp4`.
  2. Bring up the stack: Postgres (pgvector ≥0.8) + local Ollama with `mxbai-embed-large` + `llama3.2:3b`
     pulled; `cargo run -p hushai-backend` (loads `.env`, `DEVICE_TOKEN=dev-secret-token`). Feed:
     `python3 local_dev/feed_segments.py --device cam-sentiment --video .../control.mp4 --seg-seconds 2 --url http://localhost:8080/v1/segments --token dev-secret-token`.
     Run `cargo run -p hushai-worker`. **Observable:**
     `SELECT DISTINCT sentiment FROM transcript_sentences WHERE device_id='cam-sentiment'` → **both a
     positive and a negative label present** (not all neutral/NULL).
  3. **(Phase B build-prep — before `cargo run`)** provision per the prerequisite checklist: place the
     pinned ECAPA `.onnx` at `./models/ecapa-tdnn.onnx` (record its sha256), set `SPEAKER_MODEL_PATH`,
     and provision `libonnxruntime` (dev: enable `ort` `download-binaries` and accept the one-time
     build-time fetch; air-gapped: vendor the lib + set `ORT_LIB_LOCATION`). Apply migration 0005 (start
     backend once, or `sqlx migrate`), then either `cargo sqlx prepare` for the backend (and touch a
     `.rs`) **or** confirm the new handlers use runtime sqlx. **Observable:** `cargo build -p hushai-worker`
     links and `SpeakerEmbedder` returns a 192-d vector for a 2s fixture.
  4. **(Phase B fixture)** **[HUMAN — and ears]** create a committed two-speaker fixture with
     **interleaved** turns (A,B,A,B) using two **genuinely distinct** voices (two real people, or two
     clearly different TTS engines — same-engine TTS can collapse to one ID), ensure it has an audio
     track, mux to mp4, commit under `local_dev/captures/` (or `models/test-assets`), document expected
     speaker count = 2.
  5. Feed it as real 2s segments:
     `python3 local_dev/feed_segments.py --device cam-twovoices --video .../twovoices.mp4 --seg-seconds 2 --url http://localhost:8080/v1/segments --token dev-secret-token`
     (confirm HTTP 200s + pending status rows). Run the worker; wait until done count == segment count.
     **Observable:** `SELECT speaker_id, sentiment, left(text,40) FROM transcript_sentences WHERE
     device_id='cam-twovoices' ORDER BY start_unix_nanos` → non-NULL `speaker_id` on clean segments
     (NULL on silent/multi-speaker), **~2 distinct IDs (NOT hundreds)**, and **[HUMAN ears]** eyeball
     that IDs roughly track the two real speakers' turns (per-segment labeling is coarse; expect
     boundary errors — a documented v1 limitation, not a failure).
  6. **Objective separation (not just eyeballing):** confirm `count(DISTINCT speaker_id)` is a small band
     around 2; compute cosine distance between the two centroids in `speakers` and confirm it exceeds
     `SPEAKER_MATCH_THRESHOLD`. **Run the threshold tuning sweep here:** vary `SPEAKER_MATCH_THRESHOLD`,
     observe distinct-ID count vs the known count of 2, record the calibrated value.
  7. **Cross-device check:** re-feed the **same** `twovoices.mp4` under a **second** device id
     (`--device cam-twovoices-2`), run the worker. **Observable:** the two voices map to the **same two
     `speaker_id`s** as device 1 (global match), not four new IDs.
  8. **Catalog + naming:** `GET http://localhost:8080/v1/speakers` (Bearer dev-secret-token) → the
     discovered rows with sample utterances. **[HUMAN ears]** export sample audio per ID to decide which
     ID is "Bob" — via the new `GET /v1/speakers/{id}/sample-audio`, or
     `./local_dev/export_capture.sh cam-twovoices-muxed` (**pass the `<device>-muxed` stream id
     explicitly** — `feed_segments` writes `stream_id='cam-twovoices-muxed'`; the script defaults to
     `cam0-video` and would otherwise find nothing). Name it:
     `curl -X PATCH -H 'Authorization: Bearer dev-secret-token' -H 'Content-Type: application/json' -d '{"display_name":"Bob"}' http://localhost:8080/v1/speakers/<bob-uuid>`;
     re-GET → `display_name="Bob"`. If the matcher over-split Bob, exercise merge:
     `curl -X POST .../v1/speakers/<loser-uuid>/merge -d '{"into":"<bob-uuid>"}'` → `transcript_sentences`
     repointed + one fewer `speakers` row.
  9. **(RAG attribution)** `cargo run -p hushai-rag` (`:8090`). Exhaustive:
     `curl -X POST http://localhost:8090/v1/rag/query -H 'Content-Type: application/json' -d '{"query":"what did Bob say?","filters":{"speaker_name":"Bob"}}'`.
     **Observable:** every source's `speaker_id` resolves to Bob; **[HUMAN ears]** the cited text matches
     what Bob actually said, not the other speaker. (Because identity is cross-device, this works without
     a `device_id` filter and spans both fed devices.)
  10. **Negative / scope checks:** query a fact only the **other** speaker said, filtered to Bob → no-info
      decline or only Bob's sources (no cross-attribution). Unknown name → empty sources (not an error).
      No speaker filter → both speakers' content can appear.
  11. **Idempotency (strict):** `UPDATE segment_transcription_status SET status='pending' WHERE
      segment_id=$x`; let the worker re-claim. **Observable:** `transcript_sentences` count unchanged,
      that segment's `speaker_id` unchanged, `speaker_segments` still exactly 1 row for `$x`, **and**
      `speakers.n_samples` + `centroid` unchanged for the affected ID.
  12. **(Phase C recluster)** drive the matcher to over-split (a tight threshold) so one voice gets ≥2
      IDs, name one of them, run the recluster. **Observable:** the duplicate anonymous IDs merge,
      `transcript_sentences.speaker_id` is bulk-remapped, and the human-assigned name survives on the
      canonical ID.
  13. **(Phase D — Android)** **[HUMAN on device]** open the new "Voices" screen, **play the sample-audio
      snippet** for an unknown ID, type a name and save (PATCH), and merge two duplicate IDs. **Observable:**
      the name persists (visible on re-open and via `GET /v1/speakers`), and a subsequent
      `/v1/rag/query` with that name attributes correctly. `./gradlew :app:assembleDebug` green.
  14. **Single-camera regression sanity:** reprocessing an existing single-camera capture (e.g. device
      `019efbfe` from `local_dev/captures`) yields ~1 `speaker_id` → **expected** (single-speaker), not a
      regression.
  15. **(Supporting — run last, never the whole proof)** worker `chunk.rs` tests for the extended
      `Sentence`; a `worker_db.rs`-style live-DB test asserting `write_transcript` persists
      `speaker_id`/`sentiment`, the matcher reuses-vs-mints by threshold (two near + one far 192-d
      vectors), the uuid→text `ANY` filter matches a stringified id, reprocess leaves centroid/`n_samples`
      byte-identical, and **extend the test `cleanup()` (`worker_db.rs:92-101`) to also `DELETE FROM
      speakers ...`** (speaker_segments cascades from segments, but `speakers` has no FK to segments and
      would leak stale centroids that poison the next test's match-or-mint); a `hushai-rag`
      `retrieve.rs`-style test asserting the speaker filter restricts results, global name→ID resolution
      works, and a speaker-scoped semantic query over a dense speaker returns a full `top_k` (recall-cliff
      guard); a backend integration test for `GET/PATCH/merge /v1/speakers` behind auth. All live-DB tests
      gate on `DATABASE_URL` and clean up by `device_id`.

  ---

  **Deferred (explicitly out of scope for this ticket):** rolling 8–10s window assembly (committed as the
  immediate next ticket); at-rest encryption of voiceprints + consent flow (privacy follow-up); acoustic
  `emotion` model (the `emotion` column stays NULL-ready); backfilling pre-feature history.
