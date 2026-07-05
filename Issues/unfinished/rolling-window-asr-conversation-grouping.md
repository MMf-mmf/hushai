**Title:** `[hushai-worker + hushai-backend + hushai-rag] - Rolling-window ASR analysis stage (decouple analysis from the 2s transport segment) + conversation grouping with multi-device fusion, surfaced in RAG`

- **Description**:

  Fix the root cause of "the transcript is chopped up too much" and give the stack a
  first-class notion of a **conversation** (across devices) for the advisor agents to analyze.
  Two linked pieces, both primarily in `hushai-worker`, plus an additive migration in
  `hushai-backend/migrations/` and a retrieval upgrade in `hushai-rag`.

  This is the **rolling-window assembly ticket** that `Issues/finished/speaker-identity-sentiment-rag-attribution.md`
  repeatedly defers to as "the committed immediate next ticket … the single biggest accuracy
  lever" (that file, lines 85-87, 383, 407-408, 553). It is a **prerequisite** for good
  diarization: once the worker's job unit is a window (this ticket), the speaker-ID work
  should run diarization on the window instead of the interim per-segment embedding. See
  **Coordination** below. **Build on the existing foundation — do not rebuild it.**

  ### The pipeline today (grounded)

  The worker processes **one ~2-second transport segment at a time, in isolation**:
  `hushai-worker/src/claim.rs:claim_one` leases the single oldest processable segment
  (`FOR UPDATE SKIP LOCKED` + lease, `ORDER BY g.capture_start_unix_nanos`) from
  `segment_transcription_status`; `process.rs:process_segment` runs
  `media::load_segment` → `media::extract_pcm` (ffmpeg decodes **only that segment's** blob to
  16 kHz mono f32) → `asr.rs:Transcriber::transcribe` (whisper-rs, greedy, **no cross-segment
  context**, segment-level timestamps only) → `chunk.rs:chunk_into_sentences` (splits on
  `.?!`, distributes time **proportionally to character count** — approximate, not real) →
  `embedder.embed` → `write_transcript` (one tx: `DELETE FROM transcript_sentences WHERE
  segment_id=$1`, batched multi-row INSERT at 8 bind params/row, mark
  `segment_transcription_status.status='done'`, commit — idempotent per segment).

  Android cuts a **fixed wall-clock 2-second** segment (`CaptureService.kt:284
  SEGMENT_DURATION_US=2_000_000L`; the feeder uses `--seg-seconds 2`). So a 6-second spoken
  sentence is transcribed as ~3 disjoint windows with mangled boundaries, and most 2s windows
  have no sentence terminator and are stored whole. **This is a pipeline problem, not a schema
  problem.**

  RAG (`hushai-rag/src/retrieve.rs:nearest` + `routes.rs:rag_query` + `llm.rs:build_prompt`)
  returns the top-k nearest **individual** sentences and feeds them to the local Ollama LLM as
  disconnected numbered snippets (`[i] (segment <uuid>, t=<ns>) <text>`). There is **no
  conversation/turn grouping and no multi-device fusion** — "analyze the whole conversation"
  is unsupported.

  The intake/storage layer is sound and stays unchanged: immutable content-addressed segments,
  `segments.sequence` (monotonic per `(session_id, stream_id)`), `segments.gap_before`
  (`0001_init.sql:62`), absolute `capture_start_unix_nanos` + `duration_nanos`, and the
  monthly RANGE-partitioned `transcript_sentences` with its HNSW + `(device_id,
  start_unix_nanos)` + `(segment_id)` indexes (`0003_scalability.sql`). Migrations on disk are
  `0001`–`0003`; **this ticket is `0004`** (the speaker ticket reserves `0005` and references a
  "0004 convention").

  ---

  ### Part A — Rolling-window analysis stage (the core fix)

  Stop treating the 2s **transport** segment as the unit of **analysis**. Assemble consecutive
  segments into a rolling window and run ASR on the window.

  **Window assembly** — new `hushai-worker/src/window.rs`, kept side-effect-free and
  unit-tested like `chunk.rs`. Group consecutive segments of the **same `(session_id,
  stream_id)`** audio stream, ordered by `sequence`, into a window with a target duration
  `WINDOW_TARGET_SECS` (default **30s** — whisper's native window; also fine for diarization,
  which the speaker ticket framed as 8-10s — configurable so it can be tuned there). Only
  audio-bearing streams: filter `media_type IN (AUDIO=1, MUXED=3)`, skipping `VIDEO=2` — this
  also clears the AGENTS.md "worker errors on video-only segments" fast-follow.

  **Close a window** when any holds: (a) accumulated duration ≥ `WINDOW_TARGET_SECS`; (b) the
  next segment in `sequence` has `gap_before=true` (a real capture discontinuity — never span
  it); (c) the stream has been idle longer than `WINDOW_IDLE_CLOSE_MS` (default ~3s) so a
  conversation's tail is transcribed promptly instead of waiting forever for a next segment.
  v1 windows are **contiguous and non-overlapping**; a sentence straddling a 30s boundary may
  split across two rows — vastly better than every 2s, and an explicit v1 limitation (small
  inter-window overlap + boundary dedup is a noted refinement, not v1).

  **Window = the durable job unit** (replaces per-segment claiming):
  - New table `transcription_windows` (the claimable queue; the analysis-stage analogue of
    `segment_transcription_status`). `window_id` is **deterministic** — a UUIDv5 over
    `(session_id, stream_id, first_sequence, last_sequence)` — so re-deriving the same span
    yields the same id and the write stays idempotent across crashes/replays.
  - A **window-builder** step scans `pending` segments per stream (driven by the existing
    `pg_notify('hushai_segment_ingested', …)` listener + poll backstop in `lib.rs:run`), forms
    closeable windows, and in **one tx** inserts the `transcription_windows` row (`pending`) and
    stamps each member segment's `segment_transcription_status` row with the `window_id`
    (new column) + status `windowed`. Idempotent via the deterministic `window_id` +
    `ON CONFLICT DO NOTHING`. This reuses `claim.rs:ensure_status_rows` (every segment still
    gets a status row first).
  - `claim.rs` gains `claim_window` mirroring `claim_one` exactly (`FOR UPDATE SKIP LOCKED`,
    lease re-claim of crashed `processing` windows, `max_attempts`, `ORDER BY
    start_unix_nanos`). `lib.rs:worker_loop` claims **windows** instead of segments.

  **Per-window pipeline** — `process.rs:process_window` (supersedes `process_segment`):
  - Batch-load the window's segments; `media.rs` decodes **each** to PCM (keeping the existing
    per-segment container logic — fMP4 init prepend vs. self-contained mp4, `media.rs:67-114`)
    and concatenates them in capture order into one window PCM buffer
    (`media.rs:extract_window_pcm`).
  - `asr.rs`: enable **word-level timestamps** via whisper-rs 0.16 `DtwParameters` /
    `DtwModelPreset` so sentence boundaries and per-sentence timing are **real**, not
    char-proportional. The window's absolute time anchor is the first segment's
    `capture_start_unix_nanos`.
  - `chunk.rs`: split into sentences using the word timings (real `start_unix_nanos`/
    `end_unix_nanos`), keeping the existing non-speech filtering.
  - Embed each sentence with the existing 1024-dim `embedder` (unchanged).

  **Sentence → segment attribution.** Each produced sentence is attributed to the source
  segment whose `[capture_start_unix_nanos, capture_start_unix_nanos + duration_nanos)`
  contains the sentence's **midpoint** — deterministic and exactly-once — so
  `transcript_sentences.segment_id` stays meaningful even though analysis spans many segments
  (preserves the `segment_id` citation/idempotency-anchor the speaker ticket relies on).

  **Idempotent window-keyed write.** Add a `window_id` column to `transcript_sentences`. The
  write tx does `DELETE FROM transcript_sentences WHERE window_id=$1`, batched INSERT (now
  carrying `window_id` + the per-row midpoint `segment_id`), then marks the window `done` —
  the same atomic delete-then-insert idempotency `write_transcript` uses today, re-keyed from
  segment to window.

  ### Part B — Conversation grouping + multi-device fusion

  Give the agents a persisted **conversation** to analyze, fused across cameras.

  - **New `conversations` table**: `conversation_id uuid PK`, `started_at_unix_nanos`,
    `ended_at_unix_nanos`, `device_ids text[]` (all participating devices), `summary text`
    (NULL-ready — see Out of scope), `embedding vector(1024)` + `embedding_model` +
    `embedding_dim` (NULL-ready), `created_at`, `updated_at`.
  - **New columns on `transcript_sentences`**: `conversation_id uuid`, `turn_id bigint`.
  - **Grouping** — new `hushai-worker/src/conversation.rs`, run as a step after windows are
    transcribed (on the same NOTIFY/poll cadence). Cluster sentences by absolute
    `start_unix_nanos`: a new conversation starts when there is a silence/time gap >
    `CONVERSATION_GAP_SECS` (default ~90s) across **all** participating streams.
  - **Multi-device fusion**: windows from **different `device_id`s** whose wall-clock time
    ranges overlap (or fall within the gap threshold) are merged into **one** conversation
    spanning both `device_ids` — the living-room-cam + kitchen-cam case. Uses absolute
    `start_unix_nanos` to align across devices (all clients send device wall-clock UTC ns).
  - **`turn_id`**: a contiguous run separated by a silence/gap. Fully meaningful once speaker
    labels exist (ticket `0005`); silence-based turns are written now.
  - **Idempotency / late data (stated honestly):** grouping re-derives assignments over a
    **sliding recent window**; a conversation older than `CONVERSATION_GAP_SECS` is
    **finalized** (boundaries frozen). The still-open tail is order-dependent and may have its
    `conversation_id` revised as later windows arrive — documented, not hidden.

  **RAG context-neighborhood expansion** (`hushai-rag`):
  - `retrieve.rs`: `Source` gains `conversation_id: Option<String>` (read with
    `try_get::<Option<…>>().unwrap_or_default()`, matching `retrieve.rs:124-127`). New
    `conversation_context(pool, conversation_id, around_unix_nanos, budget)` returns the
    conversation's sentences **ordered by `start_unix_nanos`** within a row/token budget
    centered on the hit (backed by the new `(conversation_id, start_unix_nanos)` index, on the
    same table as the HNSW index — never a JOIN, per the 0003 recall-cliff lesson).
  - `routes.rs:rag_query`: after `nearest()` + the existing distance prune, expand each hit to
    its conversation neighborhood, dedupe, and pass coherent ordered spans to the LLM. A hit
    with `conversation_id IS NULL` (pre-feature / ungrouped) falls back to today's
    single-sentence behavior. `QueryFilters`/`Filters` gain optional `conversation_id` and keep
    the existing time filters.
  - `llm.rs:build_prompt`: render context grouped by conversation, ordered, with device/time
    per line (and ready to add the speaker name once `0005` lands) instead of scattered
    snippets. Keep the existing answer-only-from-context preamble.

  ### Migration — new `hushai-backend/migrations/0004_window_and_conversation.sql` (additive)

  Auto-applied on worker/backend startup via the existing `sqlx::migrate!`. All changes are
  additive (no destructive migration), per the `0001_init.sql` philosophy.
  - `ALTER TABLE transcript_sentences ADD COLUMN window_id uuid, ADD COLUMN conversation_id
    uuid, ADD COLUMN turn_id bigint` — NULL-able; `ADD COLUMN … NULL` on the partitioned parent
    is metadata-only and propagates to all partitions. No backfill (pre-feature rows keep these
    NULL forever; RAG treats NULL as ungrouped).
  - `CREATE INDEX transcript_sentences_window_id_idx ON transcript_sentences (window_id)` —
    backs the idempotent delete-by-window.
  - `CREATE INDEX transcript_sentences_conversation_time_idx ON transcript_sentences
    (conversation_id, start_unix_nanos)` — backs neighborhood expansion + grouping reads.
  - `CREATE TABLE transcription_windows (window_id uuid PRIMARY KEY, session_id uuid NOT NULL,
    stream_id text NOT NULL, device_id text NOT NULL REFERENCES devices(device_id),
    first_sequence bigint NOT NULL, last_sequence bigint NOT NULL, start_unix_nanos bigint NOT
    NULL, end_unix_nanos bigint NOT NULL, status text NOT NULL DEFAULT 'pending', attempts int
    NOT NULL DEFAULT 0, last_error text, claimed_at timestamptz, updated_at timestamptz NOT NULL
    DEFAULT now())` + `CREATE INDEX … ON transcription_windows (status, claimed_at)` (the claim
    scan, mirroring `segment_transcription_status_claim_idx`).
  - `ALTER TABLE segment_transcription_status ADD COLUMN window_id uuid` (the segment→window
    assignment; `'windowed'` becomes a valid status value).
  - `CREATE TABLE conversations (...)` as described above.
  - **`ON DELETE CASCADE`** on FKs to `segments`/`devices` where applicable — this establishes
    the "0004 convention" the speaker ticket (`0005`) references.
  - Migration-safety header comment (matching house style): `ADD COLUMN … NULL` on a
    partitioned parent is metadata-only, but `CREATE INDEX` on a partitioned parent is **not**
    concurrent and locks per partition; on a production-sized corpus build the new indexes
    `CONCURRENTLY` per partition out-of-band. Fine for the dev corpus.

  ### Config (`hushai-worker/src/config.rs` + `.env.example`, via the existing `opt()/parse()` pattern)
  `WINDOW_TARGET_SECS` (30), `WINDOW_IDLE_CLOSE_MS` (~3000), `WINDOW_MAX_SECS` (hard cap, ~60),
  `CONVERSATION_GAP_SECS` (~90), and any whisper DTW knobs. Thread through `lib.rs:run`.

  ### Coordination with the speaker-ID + sentiment ticket (`0005`)
  `Issues/finished/speaker-identity-sentiment-rag-attribution.md` was written **before** windowing
  existed and uses **per-segment** ECAPA embeddings as an interim, explicitly naming this
  windowing work as its bigger-lever prerequisite. This ticket changes the worker's **job unit
  to the window**. When both land, diarization + speaker embedding should run on the **window
  PCM** (more voiced speech ⇒ far better embeddings) and `speaker_id`/`sentiment` attach to
  sentences within the window. **Do not rewrite the `0005` ticket here** — just keep the
  `segment_id` link intact (via midpoint attribution) so its per-segment bookkeeping still
  works, and leave a note that its diarization should migrate onto windows.

  ### Out of scope (later tickets)
  - **Per-conversation summarization** (filling `conversations.summary`/`embedding` and the
    existing `rolling_summaries` "book" idea) — the columns are NULL-ready; populating them is a
    follow-on.
  - Speaker identity, diarization, sentiment/emotion — owned by `0005`.
  - Inter-window overlap + boundary-dedup (refinement over v1 non-overlapping windows).
  - Any change to the ingest wire contract or segment cadence.

- **Acceptance Criteria**:
  - [ ] Repo builds; `hushai-worker`, `hushai-backend`, `hushai-rag` all `cargo build`/`cargo
        run`; migration `0004` applies cleanly on startup (`\d transcript_sentences` shows
        `window_id`/`conversation_id`/`turn_id`; `transcription_windows` + `conversations`
        exist).
  - [ ] Running the worker over ingested audio produces `transcript_sentences` rows that are
        **whole, terminated sentences** with plausible per-sentence `start/end_unix_nanos` —
        **not** 2s fragments. A spoken sentence that crosses 2s transport boundaries appears as
        **one** row (or, at worst, splits only at a ~30s window boundary), demonstrably better
        than the pre-change per-segment output.
  - [ ] Every closed window has a `transcription_windows` row that ends `done` (or `error` with
        `last_error`); each member segment's `segment_transcription_status.window_id` is set and
        status `windowed`. A silent/no-speech window is `done` with zero sentences (no crash, no
        retry loop).
  - [ ] **Per-window idempotency:** re-running the worker (and **killing it mid-window** then
        restarting) completes with **no duplicate** `transcript_sentences` and **no lost**
        windows; the deterministic `window_id` is identical across runs; re-processing is a
        no-op on row counts.
  - [ ] **Attribution:** for a sampled sentence, the `segments` row for its `segment_id` has a
        `[capture_start, capture_start+duration)` interval that contains the sentence's midpoint.
  - [ ] `conversations` is populated; each row's `device_ids` lists every participating device;
        `transcript_sentences.conversation_id` is set for grouped rows.
  - [ ] **Multi-device fusion:** the same event captured under **two different `device_id`s**
        with overlapping wall-clock timestamps is grouped into **one** `conversations` row whose
        `device_ids` contains **both** devices (not two separate conversations).
  - [ ] **RAG:** `POST /v1/rag/query` returns an answer grounded on a **coherent, ordered
        conversation span** (sources share a `conversation_id` and read in time order), not
        isolated snippets; a hit whose `conversation_id` is NULL still works (single-sentence
        fallback). Optional `filters.conversation_id` restricts results to that conversation.
  - [ ] **Runtime offline:** ASR (local whisper), embeddings + RAG LLM (local Ollama) — no
        egress; provider/model selection stays config-driven.
  - [ ] Existing behavior preserved: `hushai-backend` ingest unchanged; the contract/proto
        unchanged; `transcript_sentences` partitioning + HNSW retrieval still work.

- **How to Test** (real end-to-end on the live local stack; human-in-the-loop steps marked
  **[HUMAN]**; every DB/curl/build step is driven directly):

  1. **(Bring up the stack)** Postgres (pgvector ≥0.8) on `localhost:5432` DB `hushai`; local
     Ollama with `mxbai-embed-large` + `llama3.2:3b` pulled; whisper model at
     `models/ggml-base.en.bin`. `cd hushai-backend && SQLX_OFFLINE=true cargo run` (applies
     `0004`). Confirm the new schema: `psql "$DATABASE_URL" -c "\d transcript_sentences"` shows
     `window_id`, `conversation_id`, `turn_id`; `\dt` shows `transcription_windows` +
     `conversations`.

  2. **([HUMAN] make a multi-sentence speech clip)** Record/obtain a clip with several full
     sentences spoken continuously over **>6 seconds** (so it crosses multiple 2s transport
     segments), e.g. *"I think we should reconsider the kitchen renovation. The budget is
     already tight this quarter. Let's talk to the contractor on Monday."* Mux with a black
     video track so the MUXED feeder works:
     `ffmpeg -f lavfi -i color=c=black:s=320x240:r=15 -i speech.wav -shortest -c:v libx264 -c:a aac .../scratchpad/speech.mp4`.

  3. **(Feed as real 2s segments + run the worker)**
     `python3 local_dev/feed_segments.py --device cam-A --video .../speech.mp4 --seg-seconds 2 --url http://localhost:8080/v1/segments --token dev-secret-token`
     (confirm HTTP 200s). Then `SQLX_OFFLINE=true cargo run -p hushai-worker`. **Observe** logs
     building/claiming **windows** (not per-segment), then verify the real output:
     - `psql "$DATABASE_URL" -c "SELECT left(text,80), start_unix_nanos, end_unix_nanos FROM transcript_sentences WHERE device_id='cam-A' ORDER BY start_unix_nanos;"`
       → **whole, terminated sentences** spanning the clip (each full sentence is one row), not
       ~2s fragments. Compare mentally to the old behavior (a fragment every 2s).
     - `psql "$DATABASE_URL" -c "SELECT status, count(*) FROM transcription_windows GROUP BY status;"` → all `done`.
     - `psql "$DATABASE_URL" -c "SELECT window_id, count(*) FROM transcript_sentences WHERE device_id='cam-A' GROUP BY window_id;"` → sentences grouped under window ids.

  4. **(Attribution spot-check)** Pick a sentence row; confirm its `segment_id`'s segment
     interval contains the sentence midpoint:
     `SELECT s.capture_start_unix_nanos, s.capture_start_unix_nanos + s.duration_nanos AS seg_end, (t.start_unix_nanos + t.end_unix_nanos)/2 AS mid FROM transcript_sentences t JOIN segments s USING (segment_id) WHERE t.id = <id>;`
     → `capture_start ≤ mid < seg_end`.

  5. **(Idempotency — strict)** Re-run the worker unchanged → `SELECT count(*) FROM
     transcript_sentences;` unchanged, no `window_id` duplicated. Then re-queue a window
     (`UPDATE transcription_windows SET status='pending' WHERE window_id=<id>;`) and **kill the
     worker mid-process**, restart → the window finishes with the **same** `window_id`, no
     duplicate sentences, none lost.

  6. **(Multi-device fusion — the headline)** Feed the **same** `speech.mp4` under a **second**
     device at an overlapping wall-clock time:
     `python3 local_dev/feed_segments.py --device cam-B --video .../speech.mp4 --seg-seconds 2 ...`
     (the feeder stamps current wall-clock, so cam-A and cam-B overlap). Run the worker.
     **Observe:** `psql "$DATABASE_URL" -c "SELECT conversation_id, device_ids, started_at_unix_nanos, ended_at_unix_nanos FROM conversations ORDER BY started_at_unix_nanos;"`
     → a conversation whose `device_ids` contains **both** `cam-A` and `cam-B` (one fused
     conversation, not two). Sentences from both devices share that `conversation_id`.

  7. **(RAG conversation grounding)** `SQLX_OFFLINE=true cargo run -p hushai-rag` (`:8090`).
     `curl -s localhost:8090/v1/rag/query -H 'content-type: application/json' -d '{"query":"what did they say about the renovation?"}' | jq`.
     **Observe:** a `200` whose `answer` reflects the **coherent conversation** (e.g. ties the
     renovation to the budget and the Monday contractor call), and whose `sources` share a
     `conversation_id` and read in time order — not three unrelated snippets. **[HUMAN]** sanity-
     check the answer reads like it understood the exchange, not isolated lines.

  8. **(Negative / fallback)** A query about content not present → the model declines (existing
     no-hallucination behavior intact). A pre-feature `transcript_sentences` row (NULL
     `conversation_id`) is still retrievable and grounds as a single sentence.

  9. **(Privacy)** Re-run steps 3 and 7 with network egress blocked (or `lsof`/local-proxy
     monitoring) and confirm transcription + grouping + RAG still work and **nothing leaves the
     host**.

  10. **(Supporting unit/integration tests — `cargo test`, run last, never the whole proof)**
      `window.rs`: window-close on duration / on `gap_before` / on idle timeout; deterministic
      `window_id` stability. `chunk.rs`: sentence split from word-level timings; midpoint→segment
      attribution. `conversation.rs`: gap-based boundary; **multi-device overlapping-time merge
      into one conversation**; idempotent re-grouping. A `worker_db.rs`-style live-DB test:
      `process_window` persists `window_id`/`conversation_id` and reprocessing leaves counts
      unchanged. `hushai-rag` `retrieve.rs`: `conversation_context` returns ordered neighbors;
      neighborhood expansion in `build_prompt`. All live-DB tests gate on `DATABASE_URL` and
      clean up by `device_id`.

  ---

  **Migration numbering:** this is `0004`; `Issues/finished/speaker-identity-sentiment-rag-attribution.md`
  is `0005` and builds on it. Keep `AGENTS.md` (the `transcript_sentences` storage section + the
  "video-only segments" fast-follow note) and `REVIEW.md` current in the same change set.
