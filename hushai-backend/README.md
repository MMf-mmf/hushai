# hushai-backend

Durable, idempotent **segment-ingest server** for the Hushai data-intake subsystem.
It accepts audio/video **segments** from camera clients over HTTP and persists them
exactly-once, per the camera→backend contract v0.1.0
(`../contracts/cameraToBackendContract.md`).

This crate owns **ingest, the shared Postgres schema, and the admin/catalog HTTP API**.
Transcription, embeddings, and vision run in the sibling **worker** crate (which shares
this schema via a path dep); the backend stores what they produce and serves it back to
the viewer. The schema was designed vector-ready from the start and has grown by
forward-only migrations since (see `migrations/README.md`). For the workspace map, see
`../AGENTS.md`.

## Architecture: metadata in Postgres, media on a blob store

The actual audio/video bytes are **never** stored in Postgres. Each segment's media
`body` is written to the filesystem as a **content-addressed blob**
(`{BLOB_DIR}/blobs/ab/cd/<sha256>`), and the database row stores only **metadata** plus
a pointer:

- `blob_uri` (`file://…`) + `storage_backend` (`"file"`) → swap to `s3://` / MinIO later
  with **zero schema change**.
- The only binary columns on `segments` are `content_sha256` (32-byte digest) and
  `codec_init_data` (≤~1.4 KB fMP4 decoder init, i.e. SPS/PPS — small metadata from the
  manifest, not the media payload).

This is the standard "large media in object storage, metadata in the DB" pattern.

### Durable write path (the only path that returns `200`)

1. Stream `body` to a temp file while computing SHA-256 + byte length.
2. If digest/length ≠ manifest → delete temp, return `422` (never promote a bad blob).
3. `fsync` temp → atomic `rename` into the content-addressed path → `fsync` the
   destination directory (POSIX rename durability).
4. `INSERT … ON CONFLICT (segment_id) DO NOTHING` → commit.
5. Return `200`.

The blob is durable **before** the row commits, so a crash can only orphan a (GC-able)
blob — never commit a row pointing at missing bytes. Idempotency: `segment_id` is the
primary key; a re-POST with identical bytes returns `200` with no new write, and a
`segment_id` reused for **different** bytes returns `422` without overwriting anything.

## Endpoints

### Ingest — the durable write path (contract v0.1.0)

| Method | Path           | Notes |
|--------|----------------|-------|
| POST   | `/v1/segments` | `multipart/form-data`: `manifest` (protobuf) + `body` (opaque media). `Authorization: Bearer <token>`. |

Responses: `200` durable accept / idempotent retry · `400` malformed / missing part ·
`401` bad token · `409` `(session,stream,sequence)` reused by a new `segment_id` ·
`413` oversized body · `422` integrity mismatch or `segment_id` reused with different
bytes · `429` overloaded · `507` storage pressure / pool exhausted.

> The contract §6 enumerates `200/401/422/429/507`; the server additionally uses standard
> HTTP `400` (malformed/undecodable request), `409` (permanent ordering conflict — re-send
> can't help), and `413` (oversized) for cases §6 leaves open. These never weaken a §6
> guarantee: a `200` still means durably accepted, and no non-`200` ever claims acceptance.

### Health & observability (unauthenticated, like the health probes)

| Method | Path       | Notes |
|--------|------------|-------|
| GET    | `/healthz` | Liveness. |
| GET    | `/readyz`  | DB reachable + blob volume writable with free-space headroom. |
| GET    | `/metrics` | Prometheus scrape target. |

### Admin / catalog API (bearer-authed, proxied through the viewer)

Beyond ingest, the backend hosts the read/admin surface the **viewer** proxies — all
**bearer-authed** (the same token store as ingest) and reached through the viewer, not
called directly by cameras. One row per resource group below; see `src/routes.rs` for the
exact routes and `../AGENTS.md` for what each does.

| Resource | Paths | What |
|----------|-------|------|
| Speakers | `GET /v1/speakers`; `PATCH /v1/speakers/{id}` + `/merge`·`/archive`·`/unarchive`·`/owner`·`/unowner`·`/sample-audio`; `/v1/speakers/recluster`·`/recluster-deep`·`/duplicates`·`/merge-group`·`/unattributed` (+`/name`, `/sample-audio`) | Voice-print catalog: rename, merge, archive, owner-tag, recluster/dedup. |
| Persons  | `GET /v1/persons`; `PATCH /v1/persons/{id}` + `/merge`·`/archive`·`/unarchive`·`/owner`·`/unowner`·`/sample-face` | Face catalog (visual sibling of speakers). |
| Plates   | `GET /v1/plates`; `/v1/plates/search`; `PATCH /v1/plates/{id}` + `/merge`·`/archive`·`/unarchive`·`/sample-crop` | License-plate (ALPR) catalog. |
| Devices  | `GET /v1/devices`; `PATCH`/`DELETE /v1/devices/{id}`; `/{id}/usage`; `PUT /{id}/retention`; `DELETE /{id}/footage`; `/{id}/footage/bulk-delete` | Device management + footage deletion + retention policy. (Footage **export** lives in the viewer, not here.) |
| Events & alerts | `GET /v1/events`; `/v1/events/feed` (+ `/{delivery_id}/ack`); `/v1/alert-rules` (+ `/{rule_id}`) | Materialized event feed, in-app notification feed + ack, alert-rule CRUD. |
| Watchlist | `/v1/watchlist` (+ `/{watch_id}`) | "Of interest" list. |
| Audit    | `GET /v1/audit` | Append-only audit-log read surface. |

## Database

Postgres + pgvector. **Docker isn't required for local dev** — a Homebrew
`postgresql@16` + `pgvector` on `localhost:5432` works:

```bash
createdb hushai
export DATABASE_URL=postgres://localhost/hushai
sqlx migrate run                 # applies migrations/*.sql in order (see migrations/README.md)
```

`docker-compose.yml` (image `pgvector/pgvector:pg16`, host port **5433**) is the
canonical/CI artifact:

```bash
docker compose up -d db
export DATABASE_URL=postgres://hushai:hushai@localhost:5433/hushai
sqlx migrate run
```

### sqlx offline mode

Query metadata is committed under `.sqlx/` (via `cargo sqlx prepare`), so clean/CI
builds need no live DB:

```bash
SQLX_OFFLINE=true cargo build
```

Regenerate after changing any SQL: `cargo sqlx prepare` (needs the DB up + migrated).

## Run

Configuration is read from the environment (see `.env.example`; `.env` is auto-loaded):

```bash
cp .env.example .env            # set DATABASE_URL, BLOB_DIR, DEVICE_TOKEN
cargo run
curl -w '%{http_code}\n' localhost:8080/healthz   # 200
```

> To bring up the **whole stack** (backend + worker + rag + viewer + infra) with one
> command, run `../local_dev/run_stack.sh` (see AGENTS.md "Run the full stack locally").

## Feed the real video

`../local_dev/feed_segments.py` splits a video file into conforming MUXED ~2 s fMP4
segments (ffmpeg) and POSTs them. Build a public-domain demo clip first with
`../local_dev/build_demo.sh`, or pass `--video` an absolute path to your own:

```bash
cd ../local_dev
V="$PWD/.demo_work/clips/front_door.mp4"                # --video must be an ABSOLUTE path
python3 feed_segments.py --device cam-A --video "$V"              # 10/10 accepted
python3 feed_segments.py --device cam-A --video "$V"              # re-run: all idempotent 200s
python3 feed_segments.py --device cam-B --video "$V" &
python3 feed_segments.py --device cam-C --video "$V" & wait
# negative paths:
python3 feed_segments.py --device cam-A --video "$V" --bad-token    # 401
python3 feed_segments.py --device cam-A --video "$V" --corrupt-body # 422
python3 feed_segments.py --device cam-A --video "$V" --conflict     # 422 (id reuse, new bytes)
```

## Tests

```bash
cargo test                      # unit + source_kind §7 guard; integration runs if DATABASE_URL is set
```

- Unit: `IngestError`→status mapping, proto decode boundary (16/32-byte validation,
  `u64`→`i64`), `shard_path`.
- `tests/source_kind_invariant.rs`: CI guard asserting no server logic branches on
  `source_kind` (contract §7).
- `tests/integration.rs` (DB-gated): happy `200`, idempotent retry, `segment_id`-reuse
  `422`, integrity `422`, `401`, missing-part `400`.

---

# Project status & build log

**Ticket:** `Issues/initial-backend.md` — *Initial backend: durable, idempotent
segment-ingest server (video+audio) with a vector-ready database.*
**Status:** ✅ **Complete and verified end-to-end.** All acceptance criteria pass against
a real video clip and a live Postgres 16 + pgvector. Built on macOS arm64
with `cargo`/`rustc` 1.96 (edition 2024).

## What was built (file map)

```
hushai-backend/
  Cargo.toml                      pinned deps + correct feature flags (see "Crate notes")
  build.rs                        prost-build compiles the proto at build time
  docker-compose.yml              pgvector/pgvector:pg16, host port 5433 (CI/canonical artifact)
  .env.example / .env             config; .env is gitignored, also read by sqlx macros at compile time
  proto/hushai/v1/segment.proto   VERBATIM copy of contract §4 (SegmentManifest + MediaType)
  migrations/0001_init.sql        full vector-ready schema: 8 tables, vector(1024)/vector(512), no indexes yet
  .sqlx/                          committed offline query metadata (cargo sqlx prepare) — clean/CI builds need no DB
  src/
    lib.rs       boot (dotenv, tracing, build_state, serve) + graceful shutdown (Ctrl-C + SIGTERM)
    main.rs      thin #[tokio::main] entrypoint -> hushai_backend::run()
    config.rs    Config::from_env() with documented defaults
    error.rs     IngestError (thiserror) -> HTTP status mapping (400/401/413/422/429/507/500)
    state.rs     AppState { pool, blob_root, tokens, limiter, config } (cheap Arc clone)
    proto.rs     prost include + DecodedManifest (the single bytes->Uuid, u64->i64 decode boundary)
    auth.rs      TokenStore + require_bearer middleware (token->DeviceIdentity seam) -> 401
    storage.rs   durable write path: stream+hash to temp, fsync->rename->fsync-dir, RAII temp guard, free-space
    db.rs        idempotency transaction: devices/sessions/streams upserts + segments ON CONFLICT, conflict detection
    ingest.rs    POST /v1/segments: multipart state machine -> integrity gate -> promote -> commit
    routes.rs    router wiring, per-route body limit + auth, healthz/readyz, trace + timeout layers
  tests/
    integration.rs            DB-gated HTTP end-to-end cases
    source_kind_invariant.rs  contract §7 CI guard
local_dev/
  feed_segments.py            ffmpeg HLS-fMP4 splitter + protobuf manifest + multipart POST feeder
  segment_pb2.py              generated Python protobuf binding (protoc)
```

## Key design decisions & rationale

- **Media on disk, metadata in Postgres.** Bodies are content-addressed blobs on the
  filesystem; the DB stores metadata + `blob_uri`/`storage_backend`. Verified: `segments`
  table is ~336 KB for 110 rows while media lives on disk. An S3/MinIO backend is a future
  drop-in with **no schema change**.
- **Durable-before-commit ordering.** A `200` is returned only after the blob is fsync'd
  *and* the row is committed, so a crash can at worst orphan a GC-able blob — never commit a
  row pointing at missing bytes. Empirically confirmed under a SIGTERM-mid-burst test
  (48/48 received-200s had a committed row + present blob; 0 stranded).
- **Idempotency via the `segment_id` PK.** `INSERT … ON CONFLICT (segment_id) DO NOTHING`,
  then on a conflict re-read and compare `content_sha256`: equal → `200` (no new write),
  different → `422` (never overwrite). A reused `(session_id, stream_id, sequence)` with a
  new `segment_id` is caught as a unique violation → `422`.
- **Backpressure via `tokio::sync::Semaphore`** (the contract allows "tower::limit +
  load_shed *or* a Semaphore"). Chosen over the tower `load_shed` → `HandleErrorLayer`
  stack because it produces `429` directly through typed Rust and is less brittle across
  axum 0.8 / tower 0.5. Disk watermark / pool exhaustion → `507`.
- **Source-agnostic (§7).** `source_kind` is stored and logged but never branched on. A CI
  test (`tests/source_kind_invariant.rs`) fails the build if any server line uses
  `source_kind` in a conditional/comparison.
- **Postgres provisioning.** Docker is not installed on the dev machine, so local
  development and all mandatory tests run against a Homebrew `postgresql@16` + `pgvector`
  on `:5432`. `docker-compose.yml` is shipped as the canonical/CI artifact (host port 5433
  so it coexists with a local Postgres). *(User-confirmed approach.)*
- **`BLOB_DIR` is the volume root** (e.g. `./data`); media lands under
  `{BLOB_DIR}/blobs/ab/cd/<sha256>` with in-progress uploads in `{BLOB_DIR}/tmp/` on the
  same filesystem (so `rename` is atomic).

## Verification log (observed results)

All run against the real server + real Postgres + a real ~20 s video clip (10 MUXED ~2 s
fMP4 segments):

| Check | Result |
|-------|--------|
| `cargo build` / `cargo run` / `SQLX_OFFLINE=true cargo build` | clean, no warnings |
| `cargo test` | 7 unit + 1 integration + §7 guard — all pass |
| `healthz` / `readyz` | `200` / `200` |
| Feed the clip (`cam-A`) | **10/10** accepted; blobs match digest+length; gapless (`count == max(seq)+1`) |
| Idempotent re-run | all `200`; rows + blobs **unchanged** (exactly-once) |
| Reuse `segment_id`, different bytes | **422**; stored `content_sha256` unchanged |
| Corrupt body / bad token / missing part / oversized | **422** / **401** / **400** / **413**; nothing written |
| Two+ concurrent cameras (`cam-B`, `cam-C`, plus 8× `cam-D*`) | all ingest fully; interleaved per-segment spans; cross-device blob dedup |
| All 8 tables + `vector` extension + NN `<->` query | present; dummy `vector(1024)` insert → NN match → delete |
| Graceful shutdown (SIGTERM mid-burst) | 48/48 received-200s durable; **0 stranded**; restart + re-feed idempotent |

Final DB state at hand-off: 11 devices (`cam-A`/`cam-B`/`cam-C`/`cam-D0..7`), 10 segments
each (110 total), each timeline gapless.

## Where things stand / known limitations

- **Embeddings and vector search are live** (no longer deferred). The worker writes
  transcript, speaker-voiceprint, face, and object embeddings into partitioned pgvector
  tables, each backed by a per-partition **HNSW** (`vector_cosine_ops`) index — transcript
  (`0003`), speaker (`0007`), person + objects (`0009`). See `migrations/README.md` for the
  full table/index list.
- **Auth is a bearer-token allowlist.** Either a single `DEVICE_TOKEN` (back-compat) or a
  per-device `DEVICE_TOKENS` map of `label:token` entries (so a lost device can be revoked
  individually — drop its entry + restart). Tokens are compared in **constant time**
  (`subtle::ConstantTimeEq`). The middleware resolves a token into a `DeviceIdentity`
  extension — the seam where a real `device_tokens` table / per-device issuance drops in
  without touching the handlers. See `src/auth.rs`.
- **Orphan-blob GC is implemented** (`storage::reclaim_blobs`). After a footage/device
  delete has *committed*, the reclaimer re-checks each content hash against the live DB and
  unlinks only blobs no longer referenced by any segment row (skipping files newer than a
  10-minute grace window, so an in-flight promote is never mistaken for an orphan). A crash
  between write and commit still only orphans a GC-able blob — reclaimed by a later pass.
- **Storage backend is local files only.** `storage_backend="file"`; an S3/MinIO backend is
  a future additive change (no schema migration needed).
- **Docker not installed locally.** Tests ran on Homebrew Postgres; `docker compose up -d
  db` is the documented CI/portable path but was not exercised on this machine.
- **`duration_nanos` in the feeder is nominal** (segment length, last segment shorter than
  the nominal 2 s is not measured); the server stores it opaquely, so this does not affect
  correctness.

## Reproduce the full verification

```bash
# 1. DB
createdb hushai && export DATABASE_URL=postgres://localhost/hushai
(cd hushai-backend && sqlx migrate run)

# 2. Server
cd hushai-backend && cp .env.example .env   # ensure DATABASE_URL/BLOB_DIR/DEVICE_TOKEN
cargo run &                                  # listens on :8080

# 3. Feed + checks
cd ../local_dev
V="$PWD/.demo_work/clips/front_door.mp4"               # ./build_demo.sh makes this
python3 feed_segments.py --device cam-A --video "$V"   # 10/10 -> 200
python3 feed_segments.py --device cam-A --video "$V"   # idempotent: counts unchanged
psql "$DATABASE_URL" -c "SELECT device_id,count(*),max(sequence)+1 FROM segments GROUP BY 1;"
cargo test                                             # back-up unit/integration suite
```

## Adversarial code review

An automated multi-agent review ran four dimension reviewers (durability,
idempotency/concurrency, contract conformance, robustness/security); each finding was then
cross-checked by 3 independent skeptics and kept only if ≥2 confirmed it. Result: **6
confirmed, 2 rejected** (rejected: "413 not in §6" — by design, see above; "bearer compare
not constant-time" — refuted, tokens aren't secrets-at-rest here). Disposition:

| # | Sev | Finding | Action |
|---|-----|---------|--------|
| 1 | **High** | `promote()` fsync'd only the leaf shard dir, not the newly-created `blobs/<ab>` and `blobs/<ab>/<cd>` dirents in their parents → a crash on a first-write-to-new-shard could lose a blob after a `200`. | **Fixed.** `ensure_layout` now fsyncs the blob root; `promote` fsyncs the full chain `blobs/<ab>/<cd>` → `blobs/<ab>` → `blobs` after the rename. |
| 2 | Medium | `SequenceConflict` returned `422`, telling a conforming client to "re-send" a segment that can never succeed. | **Fixed.** Now returns `409 Conflict` (permanent ordering error, not a re-send case); `422` is reserved for integrity mismatch. |
| 6 | Low | The `manifest` part was buffered up to the full 32 MB body limit (×64 concurrency = memory amplification). | **Fixed.** `manifest` is now read with a tight 1 MiB cap; oversized → `400`. |
| 3 | Low | A rejected `segment_id`-reuse / sequence-conflict leaves a freshly-promoted orphan blob; no GC. | **Accepted (by design).** Orphans are content-addressed and GC-able; a background sweep ships with a later ticket (inline deletion would race content-addressed sharing). |
| 4 | Low | `400` for malformed/undecodable requests is outside §6's enumerated set. | **Accepted.** `400` is the honest code for unrecoverable client malformation; documented above as a §6-compatible superset. |
| 5 | Low | Same `segment_id` resent with different bytes → `422` (whose §6 remedy is "re-send"). | **Accepted.** The issue explicitly specifies `422` (or `409`) for this case and the narrative maps it to `422`; kept `422` to match the ticket and the verified tests. |

All fixes were re-verified: `cargo test` green; live server returns `200` on normal feed and
`409` on a `(session,stream,sequence)` reuse with a new `segment_id`.

## Crate notes (versions that needed care)

- `prost`/`prost-build` `0.14` exist (0.14.0/0.14.2 are yanked; cargo resolves 0.14.4).
- `tower-http` needs features `["trace","timeout","limit"]`; `TimeoutLayer::new` is
  deprecated → use `with_status_code`. (The direct `tower` dep was dropped — the layers in
  use come from `tower-http`; backpressure is a `tokio::sync::Semaphore`, not a tower layer.)
- `sqlx` needs the `json` feature for `serde_json::Value` ↔ `jsonb`.
- Python 3.13 has no `uuid.uuid7()`; the feeder hand-rolls an RFC-9562 UUIDv7.
