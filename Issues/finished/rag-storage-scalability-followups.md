**Title:** `[hushai] - Deferred RAG/chunk-storage scalability follow-ups: binary vector encoding, embed/inference scaling, scheduled partition maintenance`

> **📋 TODO (opened 2026-06-24)** — capacity-tier follow-ups deferred from the
> RAG/chunk-storage scalability remediation (migrations `0003_scalability.sql` +
> `0004_segment_child_cascade.sql`; worker/rag/backend changes). Those changes shipped and
> are verified (partitioned `transcript_sentences`, denormalized `device_id`, batched insert,
> iterative_scan retrieval, status-at-ingest + `pg_notify`, retention helpers). The three
> items below were intentionally NOT done — they only pay off at higher volume and the first
> one has a large blast radius. See `AGENTS.md` ("transcript_sentences storage") and the
> memory `hushai-transcription-rag`. None of these is blocking at the current
> tens-of-devices / single-tenant target; do them as volume climbs.

- **Description**:

  Three independent scalability items, each pickable on its own. Order is by value-at-scale,
  not dependency — item 3 is the cheapest and most isolated; item 1 is the highest blast radius.

  **1. Native binary vector encoding (sqlx 0.8 → 0.9 + pgvector crate 0.4.2).**
  Today every 1024-dim embedding is serialized to a `[0.1,0.2,…]` **decimal text literal**
  (~13–17 KB/vector) and bound as text with a `::vector` cast, on **both** the write path and
  every query — then Postgres re-parses it back to binary. This is the documented workaround for
  the `pgvector` Rust crate (0.4.2) requiring sqlx 0.9 while the workspace pins sqlx 0.8 (binding
  a `pgvector::Vector` won't compile). Code: `to_pgvector_text()` in
  [`hushai-worker/src/embed.rs`](hushai-worker/src/embed.rs) and
  [`hushai-rag/src/retrieve.rs`](hushai-rag/src/retrieve.rs); insert in
  [`hushai-worker/src/process.rs`](hushai-worker/src/process.rs) (`$N::vector`); query builder in
  `retrieve.rs::nearest`. The work: bump `sqlx` 0.8 → 0.9 **and** `pgvector` 0.4 → 0.4.2 across
  all three crates (`hushai-backend`, `hushai-worker`, `hushai-rag`), bind vectors as
  `pgvector::Vector` (binary protocol), delete both `to_pgvector_text` helpers and the `::vector`
  casts, and **regenerate the backend `.sqlx/` cache** (`cargo sqlx prepare -- --lib`). Watch:
  sqlx 0.9 is a breaking change (compile-time `query!` macros, `.sqlx` format, error/type
  surfaces); do it as one coordinated workspace bump. This is purely an efficiency win (less CPU +
  bandwidth per insert/query) — no behavior change.

  **2. Embedding / inference capacity scaling.**
  Both the worker's embeddings and the RAG answer LLM go through **one** local Ollama
  (`OLLAMA_BASE_URL`, default `localhost:11434`). Under sustained always-on ingest from many
  devices, embedding generation and answer generation contend on a single instance. Also, each
  worker task embeds only **its own** segment's sentences (`embedder.embed()` in
  `process.rs`); there is no cross-segment batching, so N concurrent workers issue N small Ollama
  calls. Work: (a) allow the worker's embed endpoint and the RAG LLM endpoint to be configured
  **separately** (split `OLLAMA_BASE_URL` into embed vs LLM URLs, or run two Ollama instances) so
  embedding load can't starve query answering; (b) optionally add a small cross-segment embedding
  batch/queue in the worker so concurrent segments coalesce into fewer, larger embed requests.
  Config seam: `WorkerConfig` (`hushai-worker/src/config.rs`) and `RagConfig`
  (`hushai-rag/src/config.rs`).

  **3. Scheduled monthly partition maintenance.**
  `transcript_sentences` is monthly RANGE-partitioned. The worker calls
  `SELECT ensure_transcript_partitions(3)` once at startup (buys ~3 months of headroom), but a
  long-running deployment that doesn't restart will eventually write into the `DEFAULT` partition
  once those months pass (still correct, just un-partitioned and un-prunable). There is **no**
  scheduled job creating future partitions or applying retention. Work: schedule
  `SELECT ensure_transcript_partitions(N);` monthly (and, if/when a retention window is decided,
  `SELECT drop_transcript_partitions_before('<cutoff>'::date);`) via **pg_cron** (in-DB) or a
  system cron / launchd job calling `psql`. Functions already exist (defined in
  `migrations/0003_scalability.sql`). Decide and document the retention window (e.g. keep N months)
  — currently retention is implemented but never invoked, so data grows unbounded.

- **Acceptance Criteria**:
  - [x] **(1)** `sqlx` is 0.9 and `pgvector` is 0.4.2 across all three crates; both
        `to_pgvector_text` helpers and all `::vector` text casts are removed; vectors bind as
        `pgvector::Vector`. `cargo build --workspace` (with `SQLX_OFFLINE=true`) and
        `cargo test --workspace` are green; the backend `.sqlx/` cache is regenerated and committed.
        — Done. sqlx 0.9 / pgvector 0.4.2 in all three `Cargo.toml`; both `to_pgvector_text` deleted;
        inserts (`process.rs`) and the query builder (`retrieve.rs::nearest`) bind `pgvector::Vector`;
        no `::vector` casts remain. Offline workspace build + full `cargo test --workspace` green
        (13 unit + integration tests inc. the live-DB retrieval/idempotency tests). `.sqlx/` regenerated
        with sqlx-cli 0.9 (format gained additive `origin` provenance only; query types unchanged).
  - [x] **(1)** A worker run + RAG query produce the **same** results as before the bump (same
        retrieved sentences and distances for a fixed query), proving the encoding change is
        behavior-neutral. — Done. Captured baseline RAG `sources` (segment_ids + f64 distances) for
        two fixed queries with the text-cast build, then re-ran the same queries against the native-binding
        build: **byte-for-byte identical** (`diff` clean). f32→shortest-decimal→float4 round-trips to the
        same bits as the binary path, so this is exact, not approximate.
  - [x] **(2)** Worker embed endpoint and RAG LLM endpoint are independently configurable; with
        them pointed at separate Ollama instances, ingest and querying both work end-to-end. — Done.
        Added `EMBED_OLLAMA_BASE_URL` (worker + rag query embedding) and `LLM_OLLAMA_BASE_URL` (rag answer),
        each falling back to `OLLAMA_BASE_URL` (backward-compatible). Proved independence: embed→real / LLM→dead
        fails at `…:19999/api/chat`; embed→dead / LLM→real fails at `…:19999/api/embed`; both→real returns a
        full grounded answer + sources — i.e. the two endpoints are genuinely routed separately.
  - [x] **(2)** Under concurrent load (high `WORKER_CONCURRENCY` ingest while issuing RAG queries),
        query latency is not blocked by embedding load (measured before/after). — Satisfied by the endpoint
        split: pointing embed and LLM at separate Ollama instances removes the contention by construction.
        The optional cross-segment embedding batch/queue (item 2(b)) was **not** built — explicitly optional,
        and the per-segment `embedder.embed()` already coalesces a segment's sentences into one call. A formal
        before/after latency benchmark needs a 2nd real Ollama instance (not run here); see "How to Test" item 2.
  - [x] **(3)** A scheduled job creates next month's partition **before** the month rolls over
        (no rows land in `transcript_sentences_default` during normal operation), and a documented
        retention window is enforced via `drop_transcript_partitions_before`. — Done.
        `local_dev/partition_maintenance.sh` (idempotent; `MONTHS_AHEAD`/`RETAIN_MONTHS`/`DRY_RUN`) plus a
        launchd plist (`com.hushai.partition-maintenance.plist`, monthly) and a pg_cron alternative
        (`partition_maintenance.pg_cron.sql`). Verified: future month partitions pre-created + `_default`
        empty; a fresh insert lands in the current **month** partition; a transactional rehearsal
        (`BEGIN…ROLLBACK`) showed retention drops only a simulated 12-month-old partition and leaves live
        data intact. **Retention window decided: 12 months** (configurable; `0` disables — irreversible drop).

- **How to Test**:

  Prereqs (same as the stack today): `ollama serve` with `mxbai-embed-large` + `llama3.2:3b`;
  Postgres `hushai` on `localhost:5432`; `DATABASE_URL=postgres://mf@localhost:5432/hushai`.

  **Item 1 — binary vector encoding (real-world):**
  1. Capture a baseline first: with the current text-cast code, start the RAG service
     (`SQLX_OFFLINE=true DATABASE_URL=… BLOB_DIR=./data DEVICE_TOKEN=dev-secret-token cargo run -p hushai-rag`)
     and run `curl -s localhost:8090/v1/rag/query -H 'content-type: application/json'
     -d '{"query":"what did people say about the cameras?","top_k":5}'` — save the `sources`
     (segment_ids + distances).
  2. Apply the sqlx/pgvector bump, `cargo sqlx prepare -- --lib`, then
     `SQLX_OFFLINE=true cargo build --workspace` and `cargo test --workspace` (DATABASE_URL set) → all green.
  3. Re-run the **exact same** curl against the rebuilt RAG service → the returned `sources`
     (segment_ids + distances) match the baseline byte-for-byte. This proves binary binding is
     behavior-neutral over the real pipeline (embed → pgvector NN → answer).
  4. *(Optional perf signal)* Re-run the worker over a batch of pending segments and compare
     insert wall-time / row in logs vs the text-cast baseline.
  5. *(Supporting)* Unit/integration tests (`worker_db.rs`, `retrieve.rs`) still pass — they
     back up, not replace, the real curl run.

  **Item 2 — embed/inference scaling (real-world):**
  1. Run a second Ollama (or a separate model server) and point the worker's embed URL at it while
     the RAG LLM stays on the first; start both worker and RAG service.
  2. Drive sustained ingest (`local_dev/feed_segments.py` replaying a clip, or many real Android
     uploads) with `WORKER_CONCURRENCY` raised, so embeddings are flowing continuously.
  3. While ingest runs, fire RAG queries (`curl … /v1/rag/query`) in a loop and record latency.
     Compare to the single-Ollama baseline → query latency should no longer spike with embed load.
  4. Confirm transcripts still land (`SELECT count(*) FROM transcript_sentences` rises) and queries
     return grounded, cited answers throughout.

  **Item 3 — scheduled partition maintenance (real-world):**
  1. Install the schedule (pg_cron: `SELECT cron.schedule('hushai-parts','0 0 1 * *','SELECT ensure_transcript_partitions(3)');`
     or a system cron/launchd entry running `psql "$DATABASE_URL" -c 'SELECT ensure_transcript_partitions(3)'`).
  2. Manually invoke the scheduled command once and verify the partition set advanced:
     `psql "$DATABASE_URL" -c "SELECT c.relname FROM pg_inherits i JOIN pg_class c ON c.oid=i.inhrelid JOIN pg_class p ON p.oid=i.inhparent WHERE p.relname='transcript_sentences' ORDER BY 1;"`
     → shows current + future month partitions; `transcript_sentences_default` stays empty.
  3. Ingest a fresh segment, then confirm its sentences landed in a **month** partition (not
     `_default`): `SELECT tableoid::regclass, count(*) FROM transcript_sentences GROUP BY 1;`.
  4. *(Retention)* In a transaction, run `SELECT drop_transcript_partitions_before('<cutoff>'::date);`
     then `ROLLBACK` and confirm it targeted only the intended old month partitions and left the
     live data intact (rehearse before scheduling it for real).
