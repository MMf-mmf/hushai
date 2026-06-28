**Title:** `[hushai-backend] - Initial backend: durable, idempotent segment-ingest server (video+audio) with a vector-ready database`

- **Description**:

  Stand up the **first version of the Hushai data-intake backend**: a durable Rust + Axum HTTP
  server that accepts audio/video **segments** from one or more camera clients and persists them
  correctly and idempotently. This is the foundation the whole intake system builds on; everything
  downstream (transcription, embeddings, vision) is a later ticket and is **explicitly out of scope
  here** — except that the database must be created **now** with vector/embedding capability so we
  never have to migrate the schema when local models arrive.

  The server's wire boundary is fixed by `contracts/cameraToBackendContract.md` (v0.1.0) — that
  document wins on the endpoint, message shape, and response guarantees. This ticket defines the
  backend internals the contract intentionally leaves open (database, storage layout, durability),
  and must uphold the contract's §6 guarantees and the §7 source-agnostic invariant.

  **Where the code lives:** a new `hushai-backend/` crate at the repo root (Cargo project), separate
  from `Agent Ahithophel/` (the unrelated rig/deepseek agent). The proto, `build.rs`, and DB
  migrations live with it.

  **The endpoint (from the contract):**
  - `POST /v1/segments`, `multipart/form-data` with two parts: `manifest` (serialized protobuf
    `hushai.v1.SegmentManifest`) and `body` (opaque, codec-tagged media bytes).
  - `Authorization: Bearer <device-token>` on every request.
  - Responses: `200` = durably accepted; `200` again on duplicate `segment_id` (idempotent);
    `401` bad/expired token; `422` `content_sha256`/`byte_len` mismatch; `429`/`507` backend
    busy / storage pressure.

  **Proto:** author `hushai-backend/proto/hushai/v1/segment.proto` **verbatim** from contract §4
  (this same file is later compiled by the Android app via Wire — do not change the wire shape).
  Compile it with `prost-build` in `build.rs`. Keep `segment_id`/`session_id` as proto `bytes` and
  decode the 16 bytes into `uuid::Uuid` in Rust.

  **Storage model (backend-internal):**
  - Media `body` bytes go to the **local filesystem**, content-addressed by `content_sha256` with a
    two-level shard (`blobs/ab/cd/<sha256>`); the DB row stores metadata + a `blob_uri`
    (`file://…`) and `storage_backend` so we can swap to S3/MinIO later with no schema change.
  - Durable-accept write path (the only path that may return `200`):
    1. Stream the `body` to a temp file while incrementally computing SHA-256 + byte count.
    2. If digest/length ≠ manifest → delete temp, return `422` (never promote a bad blob).
    3. `fsync` the temp file → atomic `rename` into the content-addressed path → **`fsync` the
       destination directory** (POSIX rename durability).
    4. `INSERT … ON CONFLICT (segment_id) DO NOTHING RETURNING …`, commit.
    5. Return `200`.
  - Because the blob is written and durable **before** the row commits, a crash can only orphan a
    blob (GC-able), never commit a row pointing at missing bytes.

  **Idempotency / the duplicate race:** `segment_id` is the primary key. On `ON CONFLICT DO NOTHING`
  returning 0 rows, `SELECT` the existing row and compare its `content_sha256` to the incoming one:
  equal → `200` (legitimate retry, no new write); **different** → a `segment_id` reused for different
  bytes (client bug / contract violation) → `422`, never silently overwrite. This closes the one
  place an idempotency key can corrupt data.

  **Durability under load / honest backpressure (contract §6 requires real `429`/`507`, not blind
  accept):**
  - `tokio` multi-thread runtime + Axum handle many simultaneous uploaders out of the box.
  - `DefaultBodyLimit` / `RequestBodyLimitLayer` sized for ~2s segments (configurable, e.g. 32 MB);
    oversized → reject before writing anything.
  - Global concurrency cap (`tower::limit` + `load_shed`, or a `Semaphore`) → shed → `429`.
  - Free-disk watermark on the blob volume (and DB-pool exhaustion) → `507`.
  - `tower_http::timeout`; graceful shutdown via `axum::serve(...).with_graceful_shutdown(...)`
    wired to `ctrl_c` + SIGTERM so a SIGTERM mid-write can't strand a client.
  - `GET /healthz` (liveness) and `GET /readyz` (DB pool + blob volume writable + free space) — the
    readiness check is also the `507` signal source.

  **Database (Postgres + pgvector, via `sqlx`; runs locally via docker compose). Create the FULL
  schema now — including empty-but-ready tables — so no migration is needed when models arrive:**
  - `CREATE EXTENSION IF NOT EXISTS vector;`
  - **Written in phase 1:** `devices` (device_id PK, source_kind [stored, never branched on],
    first/last_seen, attrs jsonb); `sessions` (session_id uuid PK, device_id, started_at);
    `streams` (composite PK `(session_id, stream_id)`, media_type, codec, container,
    codec_init_data bytea); `segments` (segment_id uuid PK; device_id, stream_id, session_id;
    sequence bigint; media_type, codec, container, codec_init_data; capture_start_unix_nanos,
    monotonic_start_nanos, duration_nanos [all bigint]; content_sha256 bytea, byte_len bigint;
    gap_before bool; blob_uri text, storage_backend text; attrs jsonb; received_at;
    `UNIQUE (session_id, stream_id, sequence)` for gap detection).
  - **Empty-but-ready (no writes in phase 1, created so we never migrate):** `transcript_sentences`
    (segment_id ref, text, start/end_unix_nanos, sentiment, emotion, `embedding vector(1024)`,
    embedding_model, embedding_dim); `video_events` (scene_label, vibe, disposition,
    `embedding vector(1024)`); `scene_objects` (object_label, person_id, action, bbox jsonb,
    `embedding vector(512)`); `rolling_summaries` (window start/end, granularity, summary_text,
    `embedding vector(1024)`).
  - **Embedding dims:** reserve `vector(1024)` for text/summary, `vector(512)` for face/object;
    always store `embedding_model` + `embedding_dim` so multiple model generations coexist; a future
    model needing a different dim gets an additive new column, not a migration. (HNSW caps at 2000
    dims for `vector`; use `halfvec` later if ever needed.) **No HNSW/IVFFlat indexes in phase 1** —
    the tables are empty; indexing ships with the embedding pipeline ticket.
  - `uint64` proto fields (sequence, *_nanos, byte_len) → Postgres `bigint` (i64) with a documented
    cast at the decode boundary.
  - `sqlx` offline mode: commit `.sqlx/` (`cargo sqlx prepare`) so clean/CI builds don't need a live
    DB.

  **Server behavior notes baked into the contract:**
  - Store every segment opaquely regardless of `media_type` (MUXED/AUDIO/VIDEO) — **do not demux on
    the server**. Support a device emitting multiple concurrent streams (composite stream PK).
  - **Never branch server logic on `source_kind`** (§7). Add a cheap CI/grep test asserting this.
  - Do not assume multipart part order — read fields by name into a small state machine; missing
    `manifest` or `body` → `400`. Validate sizes: `segment_id`/`session_id` 16 bytes,
    `content_sha256` 32 bytes, or `400`/`422`.
  - Auth seam: validate `Bearer <token>` against a config allowlist / `device_tokens` table → `401`
    on miss; keep token→device_id resolution in middleware so real issuance drops in later.
  - `tracing` span per request carrying `segment_id`/`device_id`/`stream_id`/`sequence`.

  **Pinned crates (folded into `hushai-backend/Cargo.toml`):** `axum` 0.8, `tokio` 1 (full),
  `tower` 0.5, `tower-http` 0.6, `sqlx` 0.8 (runtime-tokio, tls-rustls, postgres, uuid, chrono,
  macros, migrate), `pgvector` 0.4 (sqlx feature), `prost` 0.14 + `prost-build` 0.14 (build-dep),
  `uuid` 1 (v7), `sha2` 0.10, `hex` 0.4, `serde`/`serde_json` 1, `chrono` 0.4, `tracing` +
  `tracing-subscriber` 0.3, `thiserror` 2 (typed `IngestError` → status codes), `anyhow` 1 (edge
  only), `dotenvy` 0.15.

- **Acceptance Criteria**:
  - [ ] New `hushai-backend/` crate builds (`cargo build`) and runs (`cargo run`), binding a
        configurable address/port; `GET /healthz` returns `200` and `GET /readyz` returns `200` when
        Postgres + blob volume are healthy.
  - [ ] `hushai-backend/proto/hushai/v1/segment.proto` matches contract §4 verbatim and compiles via
        `build.rs`/`prost-build`.
  - [ ] `POST /v1/segments` accepts a conforming multipart (`manifest` protobuf + `body`) with a
        valid Bearer token and returns `200` only after the body is fsync'd to a content-addressed
        path **and** the metadata row is committed.
  - [ ] Re-POSTing the same `segment_id` returns `200` and creates **no** duplicate row and **no**
        second blob (exactly-once at rest).
  - [ ] Re-using a `segment_id` for **different** body bytes returns `422` (or `409`) and does not
        overwrite the stored blob/row.
  - [ ] Body whose bytes don't match `content_sha256`/`byte_len` → `422`; missing/expired token →
        `401`; missing `manifest` or `body` part → `400`.
  - [ ] Two feeders running **concurrently** with different `device_id`s both fully ingest; the
        server stays up; all rows present for both; logs show interleaved per-segment spans.
  - [ ] Postgres has the `vector` extension enabled and **all** tables exist — including the
        empty-but-ready `transcript_sentences` / `video_events` / `scene_objects` /
        `rolling_summaries` with their `vector(…)` columns — proving the DB is embedding-ready with
        no future migration.
  - [ ] A nearest-neighbor query (`<->`) against a manually-inserted dummy embedding row succeeds,
        proving pgvector is genuinely usable (the dummy row is then deleted).
  - [ ] No server code branches on `source_kind` (enforced by a CI/grep check).
  - [ ] Oversized body is rejected before any write; graceful shutdown (Ctrl-C/SIGTERM) finishes
        in-flight commits without stranding a `200`.

- **How to Test**:

  1. **(Real-world, mandatory) Bring up the database.** From `hushai-backend/`, start Postgres +
     pgvector: `docker compose up -d db`. Confirm the extension and schema:
     `psql "$DATABASE_URL" -c "SELECT extname FROM pg_extension WHERE extname='vector';"` → one row;
     `psql "$DATABASE_URL" -c "\dt"` → lists `devices, sessions, streams, segments,
     transcript_sentences, video_events, scene_objects, rolling_summaries`.

  2. **(Real-world, mandatory) Run the real server.** `cargo run` (env: `DATABASE_URL`,
     `BLOB_DIR`, `BIND_ADDR`, a dev `DEVICE_TOKEN`). Then hit it for real:
     `curl -s -w '\n%{http_code}\n' localhost:8080/healthz` → `200`;
     `curl -s -w '\n%{http_code}\n' localhost:8080/readyz` → `200`.

  3. **(Real-world, mandatory) Feed the real video.** Run the local feeder
     (`local_dev/feed_segments.py`) against `IMG_7256.mp4`: it uses `ffmpeg` to split the file into
     conforming **MUXED** ~2s fMP4 segments (each starting on a keyframe; the fMP4 init segment's
     bytes become `codec_init_data`), and for each segment mints a UUIDv7 `segment_id` (once), a
     per-run `session_id`, increments `sequence`, computes `content_sha256` + `byte_len`, sets dual
     clocks (`capture_start_unix_nanos`, independent `monotonic_start_nanos`), `duration_nanos`,
     `source_kind="file_replay"`, serializes the `SegmentManifest`, and `POST`s multipart with the
     Bearer token. **Observe:** every POST returns `200`; the feeder reports e.g. `9/9 segments
     accepted`.

  4. **(Real-world, mandatory) Verify it actually stored correctly — observable results:**
     - Row count == segments fed:
       `psql "$DATABASE_URL" -c "SELECT count(*) FROM segments;"` → equals the feeder's count.
     - Every blob exists and matches its digest: for each `blob_uri`, the file exists and
       `shasum -a 256 <file>` == the row's `content_sha256` (the feeder prints a pass/fail summary).
     - Timeline is gapless: `SELECT session_id, stream_id, count(*), max(sequence)+1 FROM segments
       GROUP BY 1,2;` → `count(*)` == `max(sequence)+1` (no gaps, no dups).

  5. **(Real-world, mandatory — idempotency / exactly-once) Re-run the same feeder unchanged**
     (same `session_id`/`segment_id`s). **Observe:** all POSTs return `200`, and the `segments`
     count and on-disk blob count are **unchanged** — proving exactly-once at rest.

  6. **(Real-world, mandatory — multiple intakes) Run two feeders concurrently**, each with a
     distinct `device_id` and `session_id` (simulating two cameras), e.g.
     `python feed_segments.py --device cam-A & python feed_segments.py --device cam-B & wait`.
     **Observe:** both complete with all `200`s, the server stays up, and
     `SELECT device_id, count(*) FROM segments GROUP BY 1;` shows both devices' full segment counts.
     Tail the server logs and confirm interleaved per-segment `tracing` spans.

  7. **(Real-world, negative paths):**
     - Corrupt one segment body (flip a byte) before POST → server returns **`422`**; no row/blob
       written for it.
     - POST with a bad/empty Bearer token → **`401`**; no data stored.
     - Re-POST an accepted `segment_id` with **different** bytes → **`422`/`409`**; original blob/row
       untouched (verify the stored `content_sha256` is unchanged).

  8. **(Real-world — vector readiness)** Manually insert one dummy `transcript_sentences` row with a
     `vector(1024)` value and run a nearest-neighbor query:
     `SELECT id FROM transcript_sentences ORDER BY embedding <-> '[…]'::vector LIMIT 1;` → returns
     the row, proving pgvector is usable today. Delete the dummy row afterward.

  9. **(Real-world — durability/shutdown)** While a feeder is mid-run, send the server SIGTERM
     (Ctrl-C). **Observe:** in-flight requests either finish with a committed `200` or get no `200`
     (client will retry) — never a `200` with a missing blob/row. Restart the server and re-run the
     feeder: previously-accepted segments return idempotent `200`s with no duplication.

  10. **(Supporting) Unit/integration tests** (`cargo test`): SHA-256/byte_len validation → 422
      mapping; `ON CONFLICT` duplicate → 200 with sha-compare branch (equal vs different);
      status-code mapping for 401/422/429/507; multipart part-order state machine; a grep/lint test
      asserting no `source_kind` branch. These back up the real run above — they do not replace it.
