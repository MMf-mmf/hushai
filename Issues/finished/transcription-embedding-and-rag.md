**Title:** `[hushai-worker + hushai-rag] - Transcribe & vectorize stored segments (durable, resumable worker), then serve a basic RAG chat endpoint (Rust + Rig)`

- **Description**:

  Build the **processing + retrieval layer** on top of the completed ingest backend
  (`Issues/initial-backend.md`, crate `hushai-backend/`). Two linked, sequential pieces of
  work, both in **Rust using the Rig framework**:

  **Part A — durable, resumable background processing worker (`hushai-worker`).** Read the
  audio/video **segments already stored** by `hushai-backend` (the `segments` table + the
  content-addressed media blobs on disk), **transcribe the audio to text**, and
  **vectorize/embed** that text into the **already-existing, currently-empty** pgvector
  table `transcript_sentences`. This does **not** need to happen instantly — the goal is to
  start ASAP and **run continuously**: drain the entire backlog of stored segments, then
  stay running and keep up with newly-ingested segments as they arrive. It must be
  **crash-safe, resumable, and idempotent** (re-running never duplicates rows or loses
  work).

  **Part B — basic RAG chat endpoint (`hushai-rag`).** Once embeddings exist, expose a
  simple HTTP endpoint the **frontend app** calls to perform a **RAG search**: embed the
  user's query, run a pgvector nearest-neighbor retrieval over the stored sentence
  embeddings, feed the retrieved context to a Rig LLM, and return a grounded answer **with
  its sources**. "Basic" = single-turn question→answer-with-citations; conversational memory
  is out of scope.

  **What already exists (build on it, do not rebuild):**
  - `hushai-backend` stores each segment's media as a `file://` content-addressed blob
    (`{BLOB_DIR}/blobs/ab/cd/<sha256>`) and the metadata row in `segments`
    (`segment_id`, `device_id`, `stream_id`, `session_id`, `sequence`, `media_type`,
    `codec`, `container`, `codec_init_data` bytea, `capture_start_unix_nanos`,
    `duration_nanos`, `content_sha256`, `byte_len`, `blob_uri`, `storage_backend`, …).
  - The DB **already has** the empty, vector-ready tables from the initial ticket:
    `transcript_sentences` (`id` PK, `segment_id` ref, `text`, `start_unix_nanos`,
    `end_unix_nanos`, `sentiment`, `emotion`, `embedding vector(1024)`, `embedding_model`,
    `embedding_dim`), plus `video_events`, `scene_objects`, `rolling_summaries`. **No HNSW
    indexes exist yet** — the initial ticket explicitly deferred vector indexing to *this*
    (the embedding-pipeline) ticket.
  - `hushai-backend` is a lib+bin; its `db`, `config`, `storage`, and `proto` modules are
    reusable. The sibling crate `Agent Ahithophel/` already uses `rig` 0.37 with a DeepSeek
    provider as a reference for wiring Rig.

  **Privacy-first / model choice (a project constraint):** prefer **local models** where
  feasible so captured audio/video never leaves the machine. Recommended default:
  - **ASR / transcription:** local **Whisper** (e.g. `whisper.cpp` via `whisper-rs`, or the
    `whisper`/`whisper.cpp` CLI) producing text **with timestamps**.
  - **Embeddings:** a **local 1024-dim** text model (e.g. `mxbai-embed-large` via Ollama, or
    a BGE-large via `fastembed`) driven through **Rig's embedding API**. The dimension is a
    **hard constraint: 1024**, to match `transcript_sentences.embedding vector(1024)`. Always
    write `embedding_model` + `embedding_dim` so future model generations can coexist.
  - **RAG answer LLM:** a **local** chat model preferred (e.g. via Rig's Ollama provider);
    the existing DeepSeek provider is an acceptable fallback. Keep the provider/model
    **config-driven** (env), not hard-coded.

  **Where the code lives:** turn the repo into a **Cargo workspace**. Keep `hushai-backend`
  (ingest) as-is and add:
  - `hushai-worker/` — a binary; the transcription+embedding worker. Depends on the
    `hushai-backend` lib (path dep) to reuse the DB pool, `Config`, and blob-path resolution
    rather than duplicating the schema.
  - `hushai-rag/` — a small Axum service (or a route module) exposing the RAG endpoint;
    also reuses the `hushai-backend` lib for DB access. *(Alternatively the RAG route may be
    mounted inside `hushai-backend`'s router — implementer's call — but keep the worker a
    separate process so heavy ASR/embedding load never blocks ingest.)*
  - New migrations live with `hushai-backend/migrations/` (see below).

  **Part A details — the worker:**
  - **Backlog discovery & claiming.** Add a small bookkeeping table (new migration
    `0002_*.sql`), e.g. `segment_transcription_status (segment_id uuid PK references
    segments, status text NOT NULL DEFAULT 'pending' /* pending|done|error */, attempts int
    NOT NULL DEFAULT 0, last_error text, claimed_at timestamptz, updated_at timestamptz
    DEFAULT now())`. Claim work with `SELECT … FOR UPDATE SKIP LOCKED` (or a claim
    timestamp + lease timeout) so multiple worker instances/threads never double-process and
    a crashed claim is re-leased. (A derived "segments with no `transcript_sentences` rows"
    query is acceptable for discovery, but a status row is required for retry/observability.)
  - **Per-segment pipeline:** resolve `blob_uri` → file path; reconstruct a decodable media
    fragment by concatenating the segment's `codec_init_data` (the fMP4 init) with the blob
    bytes; use **ffmpeg** to extract the audio track to 16 kHz mono PCM/WAV; run **Whisper**
    → text + per-utterance timestamps; split into sentences; compute each sentence's absolute
    `start_unix_nanos`/`end_unix_nanos` from `segments.capture_start_unix_nanos` + the
    in-segment offset; **embed** each sentence (1024-dim); write rows.
  - **Idempotent, atomic write per source segment:** in one transaction, `DELETE FROM
    transcript_sentences WHERE segment_id = $1`, insert the freshly-computed sentences, set
    status `done`; commit. Re-processing replaces, never duplicates. A segment whose audio
    has **no speech** is valid: insert zero sentences and still mark `done` (no crash, no
    re-loop).
  - **Run-until-done, then keep up:** process the backlog (oldest first, bounded
    concurrency), then **poll** for new `pending` segments on an interval (or `LISTEN/NOTIFY`)
    and process them as they arrive — the worker is a long-running service.
  - **Failure handling:** on transcription/embedding error, increment `attempts`, record
    `last_error`, set status `error` (with capped retry/backoff); the worker keeps going and
    surfaces failures in logs + the status table. `tracing` span per segment carrying
    `segment_id`/`device_id`.
  - **Vector indexing (this ticket's responsibility):** add the HNSW index the initial
    ticket deferred, e.g. migration `CREATE INDEX … ON transcript_sentences USING hnsw
    (embedding vector_cosine_ops);` (choose the distance op to match the embedding model;
    cosine for normalized embeddings — keep retrieval and index op consistent).

  **Part B details — the RAG endpoint:**
  - `POST /v1/rag/query`, JSON in: `{ "query": string, "top_k"?: int (default ~8),
    "filters"?: { "device_id"?, "after_unix_nanos"?, "before_unix_nanos"? } }`.
  - Pipeline: embed the query with the **same** 1024-dim model → pgvector NN search
    `SELECT id, segment_id, text, start_unix_nanos, embedding <=> $1 AS distance FROM
    transcript_sentences [WHERE filters] ORDER BY embedding <=> $1 LIMIT $k` → assemble the
    retrieved sentences as context → call the Rig LLM with a grounding instruction (answer
    only from the supplied context; say so when the context doesn't contain the answer) →
    return JSON `{ "answer": string, "sources": [ { "segment_id", "device_id", "text",
    "start_unix_nanos", "distance" } ] }`. Optionally use Rig's `rig-postgres` PgVector store,
    or a raw `sqlx` NN query (either is fine).
  - Keep it a single-turn endpoint. Reuse `hushai-backend`'s bearer-token seam for auth if
    trivially available; otherwise a simple token is fine for a first cut.

  **Out of scope (later tickets):** vision extraction into `video_events`/`scene_objects`,
  `rolling_summaries`, multi-turn chat memory / sessions, reranking, sentiment/emotion
  enrichment (leave those columns null for now), and any change to the ingest wire contract.

  **Pinned/likely crates:** `rig-core` 0.37+ (embeddings + completion; Ollama and/or
  DeepSeek providers), optionally `rig-postgres` (pgvector store), `sqlx` 0.8 (reuse), `axum`
  0.8 + `tokio` (RAG service), `whisper-rs` *or* shelling out to `ffmpeg` + `whisper.cpp`,
  `tracing`, `serde`/`serde_json`, `anyhow`/`thiserror`. ffmpeg + a Whisper model are
  external prerequisites (document them).

- **Acceptance Criteria**:
  - [ ] Repo builds as a **Cargo workspace**; `hushai-worker` and the RAG service both
        `cargo build` and `cargo run`; `hushai-backend` still builds and runs unchanged.
  - [ ] New migration(s) apply cleanly: the `segment_transcription_status` table exists and
        an **HNSW index exists on `transcript_sentences.embedding`** (`\d transcript_sentences`
        shows it).
  - [ ] Running the worker against a DB that already has ingested segments transcribes and
        embeds them: `transcript_sentences` gains rows, **each with non-null `text`, a
        1024-dim `embedding`, `embedding_model` set, and `embedding_dim = 1024`**.
  - [ ] Every processed segment is marked `done` in `segment_transcription_status`; a
        no-speech segment is also marked `done` with zero sentences (no crash, no infinite
        retry).
  - [ ] **Resumable & idempotent:** killing the worker mid-run and restarting it completes
        the backlog with **no duplicate** `transcript_sentences` for any `segment_id` and no
        lost segments; re-running after completion is a no-op (counts unchanged).
  - [ ] **Keep-up mode:** after the backlog is drained the worker keeps running; a newly
        ingested segment is picked up and transcribed within the poll interval **without
        restarting** the worker.
  - [ ] `POST /v1/rag/query` with a real question returns `200` and a JSON body with an
        `answer` grounded in the transcribed content **and** a non-empty `sources` array whose
        entries cite **real** `segment_id`s/timestamps present in `transcript_sentences`.
  - [ ] A query with no relevant stored content returns an answer that **declines/says it
        doesn't know** (does not fabricate) and `sources` is empty or clearly low-relevance.
  - [ ] No captured media leaves the machine when configured with local models (ASR +
        embeddings + LLM all local); provider/model selection is **config-driven**.

- **How to Test**:

  1. **(Real-world, mandatory — have data to process)** From the initial backend, ingest
     segments first. Bring up the DB and server (`createdb hushai`;
     `export DATABASE_URL=postgres://localhost/hushai`; `cd hushai-backend && sqlx migrate
     run && cargo run`), then feed audio **that actually contains speech** so transcription
     is meaningful: `cd local_dev && python3 feed_segments.py --device cam-A`
     (use a spoken clip via `--video <clip-with-speech>.mp4` if `IMG_7256.mp4` has no
     speech). Confirm `psql "$DATABASE_URL" -c "SELECT count(*) FROM segments;"` > 0.

  2. **(Real-world, mandatory — run the worker)** `cargo run -p hushai-worker`
     (env: `DATABASE_URL`, `BLOB_DIR`, ASR/embed/LLM model config). **Observe** the logs
     show it claiming and processing each segment, e.g. `processed segment <uuid>: N
     sentences embedded`, and that it transitions from backlog-draining to idle/poll. Then
     verify the real output in the DB:
     - `psql "$DATABASE_URL" -c "SELECT count(*) FROM transcript_sentences;"` → > 0.
     - `psql "$DATABASE_URL" -c "SELECT count(*) FROM transcript_sentences WHERE embedding IS NULL OR embedding_dim <> 1024;"` → **0**.
     - `psql "$DATABASE_URL" -c "SELECT segment_id, status FROM segment_transcription_status;"` → all `done` (or `error` with a recorded `last_error`).
     - Spot-check real text: `psql "$DATABASE_URL" -c "SELECT text FROM transcript_sentences LIMIT 5;"` → readable transcribed sentences.

  3. **(Real-world, mandatory — resumability/idempotency)** Re-run the worker unchanged →
     no new/duplicate rows (`SELECT count(*) FROM transcript_sentences;` unchanged; no
     `segment_id` has duplicated sentences). Then **kill the worker mid-run** during a fresh
     batch and restart it → it finishes the backlog with no duplicates and no skipped
     segments.

  4. **(Real-world, mandatory — keep-up)** With the worker still running, ingest one more
     segment (`python3 feed_segments.py --device cam-B --limit 1`). **Observe** the worker
     pick it up within the poll interval and that `transcript_sentences` /
     `segment_transcription_status` gain its rows — **without restarting** the worker.

  5. **(Real-world, mandatory — the RAG endpoint)** Start the RAG service
     (`cargo run -p hushai-rag`) and hit it for real:
     ```
     curl -s -X POST localhost:8090/v1/rag/query \
       -H 'content-type: application/json' \
       -d '{"query":"<a question about something said in the clip>","top_k":8}'
     ```
     **Observe** a `200` whose JSON `answer` is grounded in the actual transcript and whose
     `sources` array cites real `segment_id`s/timestamps. Cross-check one source against the
     DB: `psql "$DATABASE_URL" -c "SELECT text FROM transcript_sentences WHERE segment_id='<id from sources>';"`
     contains the cited content.

  6. **(Real-world — negative / no-hallucination)** `curl … -d '{"query":"something
     definitely not in the data, e.g. quarterly revenue of Acme Corp"}'` → the `answer`
     states it doesn't have relevant information and `sources` is empty or low-relevance
     (the model is not inventing an answer).

  7. **(Real-world — privacy)** With local models configured (local Whisper + local
     embeddings + local LLM), run steps 2 and 5 with network egress disabled (or monitor
     with `lsof`/a local proxy) and confirm processing + RAG still work and **no captured
     media or transcript leaves the host**.

  8. **(Real-world — retrieval quality sanity)** Independently of the LLM, confirm pgvector
     retrieval works: embed a known phrase from the clip and run
     `SELECT text, embedding <=> $1 AS d FROM transcript_sentences ORDER BY d LIMIT 5;` →
     the closest rows are topically relevant to the phrase.

  9. **(Supporting) Unit/integration tests** (`cargo test`): sentence-chunking + absolute
     timestamp math; the per-segment atomic delete-then-insert idempotency (re-process →
     same row count); embedding dimension == 1024 guard; the NN retrieval query; RAG prompt
     assembly + source extraction; worker claim/lease (no double-processing under two
     concurrent claimers). These back up the real runs above — they do not replace them.
