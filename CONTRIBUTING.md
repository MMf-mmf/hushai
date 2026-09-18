# Contributing to Hushai

Hushai is a local-first system: captured media never leaves the machine, and every model
runs on your own hardware. Please keep that posture intact — a change that introduces an
outbound call to a third-party service will be rejected on principle, however convenient it
is.

## Getting a stack running

```bash
./local_dev/onboard.sh          # interactive first run: deps, Postgres, Ollama, models, .env, tokens
./local_dev/run_stack.sh        # thereafter: backend + worker + rag + viewer, Ctrl-C tears it all down
```

Architecture, invariants, and the non-obvious gotchas live in [AGENTS.md](AGENTS.md).
Per-component detail is in each crate's own `README.md`. The camera→backend wire contract
is [contracts/cameraToBackendContract.md](contracts/cameraToBackendContract.md).

## Before you push

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
SQLX_OFFLINE=true cargo build --workspace

createdb hushai_dev_test   # once
DATABASE_URL="postgres://$USER@localhost:5432/hushai_dev_test" \
  cargo test --workspace -- --test-threads=1
```

`--test-threads=1` is required, not a preference. The DB-backed tests share real tables, and the
alert-delivery loop claims a *batch* of pending rows — so two tests running at once steal each
other's fixtures and fail with things like `claimed at least our delivery`. Serially the suite is
**422 passed, 0 failed, 1 ignored**; in parallel it fails differently on different runs.

Point the tests at a **throwaway database**, never at the one your cameras record into — several
integration tests truncate the tables they exercise. Include the role in the URL
(`postgres://$USER@…`): without it libpq falls back to the OS user in a way that can fail with
`role "anonymous" does not exist`.

There is **no CI yet** — these checks are on you. Two notes on the test suite:

- DB-backed tests are gated on `DATABASE_URL` and skip cleanly without it. Vision tests are
  additionally gated on the ONNX weights being provisioned. Keep new model-dependent tests
  gated the same way, so the suite stays runnable on a machine without multi-gigabyte
  downloads.
- Worker tests must run serially (the `--test-threads=1` above) — see
  [docs/worker-parallelism-and-scaling.md](docs/worker-parallelism-and-scaling.md) §6.

Android:

```bash
cd hushai-android
JAVA_HOME=/opt/homebrew/opt/openjdk@17 ./gradlew :app:assembleDebug :app:testDebugUnitTest
```

Use the pinned `./gradlew` (Gradle 8.9) — a system Gradle 9.x breaks AGP 8.7.3.

Viewer UI changes should pass the headless sweep, which needs **real Google Chrome**
(Chromium has no H.264/AAC and cannot decode the HLS remux):

```bash
cd hushai-viewer/e2e && npm i && node run.mjs
```

It needs a stack with footage in it. If you have no camera, `./local_dev/build_demo.sh` builds a
`hushai_demo` database from public-domain stills and synthesized dialogue that exercises every
lane. The README's screenshots are regenerated from that same dataset — see
[docs/screenshots.md](docs/screenshots.md) if you change the UI enough to date them.

## Conventions

- **Rust edition 2024**, default `rustfmt`.
- `sqlx` queries build offline against the committed `.sqlx/` metadata. If you change a
  query, regenerate it:
  `cd hushai-backend && DATABASE_URL=… cargo sqlx prepare -- --lib`
  (needs `sqlx-cli` 0.9, installed **without** `--locked`).
- Migrations in `hushai-backend/migrations/` are **forward-only** and applied automatically
  at startup. Add a new numbered file; never edit one that has shipped. Index it in
  [hushai-backend/migrations/README.md](hushai-backend/migrations/README.md).
- Embedding dimension is fixed at **1024** to match `transcript_sentences.embedding
  vector(1024)`. Changing the embedding model means a migration.
- Code-review expectations are written down in [REVIEW.md](REVIEW.md).
- If your change makes [AGENTS.md](AGENTS.md) or [REVIEW.md](REVIEW.md) inaccurate, update
  them in the same commit.

## Pull requests

Branch off `main`, describe **what** and **why**, and include real end-to-end test steps —
exercise the actual endpoint, command, or UI, not only the unit tests. If the change touches
capture, identity, or retention, say what you observed on a live stack.

## Scope note

Perception quality work needs data we can't ship: identity thresholds are uncalibrated
against noisy real captures (see [SECURITY.md](SECURITY.md) "Known gaps"). If you have a
labelled real-world capture set and want to help calibrate, that is one of the most valuable
contributions available.
