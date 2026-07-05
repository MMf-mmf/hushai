# REVIEW.md — code-review conventions for the Hushai workspace

How to review changes in this repo, the output format to produce, and the security-sensitive /
extra-scrutiny surfaces that warrant a deeper pass. Pairs with [`AGENTS.md`](AGENTS.md) (which
explains how the system works). Keep this current when a change moves a flagged path or adds a new
high-stakes surface — in the **same** change.

## Output format

Produce findings in this shape:

1. **Summary line** — one sentence: what the change does + your overall verdict.
2. **Findings table**, most-severe first:

   | Severity | Location (`file:line`) | Finding | Confidence | Rule |
   |----------|------------------------|---------|------------|------|

   - **Severity:** Critical / Important / Minor / Nit.
   - **Confidence:** High / Medium / Low (state it; don't present a guess as a fact).
   - **Rule:** the convention/invariant it violates (cite the AGENTS.md section or the pattern below).
3. **Details** — a short paragraph per non-trivial finding: the concrete failure scenario
   (inputs/state → wrong output/crash), and the suggested fix.
4. **What I didn't check** — scope you did not cover (e.g. "did not run the vision model-gated tests
   — models unprovisioned", "did not exercise the Android build").
5. **Verdict** — Approve / Approve-with-nits / Request-changes, one line.

Report only findings you can defend. Prefer fewer high-confidence findings over a long speculative
list. If you make a claim about behavior, name the code path that proves it.

## Effort & when to fan out

Default to a **single careful pass**. Reserve the multi-agent (multiple finders + adversarial
verifier) treatment for a large diff (~500+ lines across multiple concerns) that **also** touches
one of the flagged surfaces below **and** the author asked for a thorough review. Otherwise: read the
diff, read the directly-changed files + their immediate dependencies, form findings in one pass.

## High-stakes surfaces (extra scrutiny)

A change touching any of these deserves a closer look; a large change touching them justifies fan-out.

- **Auth & tokens** — `hushai-backend/src/auth.rs` (`DEVICE_TOKENS` HashMap, constant-time `subtle`
  compare), `hushai-rag` `RAG_TOKEN`, `hushai-viewer/src/auth.rs` (IP allowlist + argon2 password +
  HMAC session cookie). Check: constant-time comparison preserved; no token/secret logged; the
  viewer stays the only IP-gated plane (backend/rag see only `127.0.0.1` via the proxy).
- **Schema migrations** — `hushai-backend/migrations/`. Forward-only (never edit a shipped file);
  partition/HNSW/`UNIQUE`-constraint changes; FK `ON DELETE` semantics. Update the migrations index
  ([`migrations/README.md`](hushai-backend/migrations/README.md)) in the same change.
- **Identity match/mint invariants** — `speaker_match.rs`, `vision/face_match.rs`,
  `vision/plates/plate_match.rs`. The `pg_advisory_xact_lock`s are **GLOBAL** (cross-device) with
  **distinct keys per catalog** — never shard per device, never reuse a key. Match/mint runs inside
  the write txn; delete-by-segment stays idempotent; the mint-guard hysteresis + quality gate must
  not be loosened to mask a calibration problem (see AGENTS.md "Known gaps").
- **Deletion & blob GC** — `hushai-backend/src/devices.rs` + `storage::reclaim_blobs`. Content-addressed
  blobs can be shared: reclaim must re-check each hash against the live DB AFTER commit. A crash may
  orphan a GC-able blob but must never dangle a row. NULL the NO-ACTION FKs under the identity
  advisory locks before deleting.
- **Webhook delivery / SSRF** — `hushai-worker/src/delivery.rs`. Cloud-metadata IPs blocked ALWAYS;
  redirects disabled; at-least-once with a stable idempotency key; optional HMAC signing. Don't widen
  the SSRF posture without intent (`ALERT_WEBHOOK_ALLOW_PRIVATE`).
- **Metrics cardinality** — `hushai-backend/src/observe.rs`. NEVER use device/client free text as a
  metric label (the registry never evicts → series explosion). New labels must be a bounded allowlist.
- **Ingest / gating correctness** — `db.rs::persist_segment`, `hints.rs`, the worker efficiency gates.
  Media must ALWAYS be stored regardless of gating; gates must **fail open**; `skipped` stays terminal
  + invisible to backfill reconcile.
- **The contract boundary** — `contracts/cameraToBackendContract.md` + `proto/hushai/v1/segment.proto`.
  Behavior keys off `container` / `media_type`, never `source_kind` (unvalidated client free text).

## Must-follow patterns

- **sqlx offline builds:** build with `SQLX_OFFLINE=true`. If you add/alter a backend `db.rs`
  `query!` macro, re-`cargo sqlx prepare -- --lib` (sqlx-cli 0.9) and commit `.sqlx/`. Runtime queries
  (worker/rag, backend `speakers.rs`/`persons.rs`/etc.) intentionally need no `.sqlx`.
- **Errors:** backend uses `thiserror` (`error.rs` `IngestError`); worker/rag use `anyhow`. Match the
  crate's existing style — don't reintroduce `thiserror` in worker/rag (removed as unused).
- **Logging:** go through the shared `hushai-backend/src/logging.rs`; do not add per-crate
  `tracing_subscriber::fmt()`. Never log secrets.
- **ORT/DYLD:** vision code must keep `ort` on `load-dynamic` (coexists with sherpa's bundled ONNX
  Runtime). Any new launch path for the worker/rag must set `DYLD_FALLBACK_LIBRARY_PATH`
  (`LD_LIBRARY_PATH` on Linux) — see AGENTS.md.
- **Viewer:** vanilla ES modules, no build step; no `alert()`/`confirm()`/`innerHTML` sinks (render
  user text via `textContent`). New proxied backend paths must be added to `proxy.rs::is_backend_path`.
- **Docs:** if a change makes `AGENTS.md`, `REVIEW.md`, a crate README, or the migrations index
  inaccurate, fix it in the same change.

## Verification expectation

Every non-trivial change should be exercised end-to-end, not just typechecked — bring up the stack
(`local_dev/run_stack.sh`) and drive the affected flow, or run the relevant `hushai-eval` tier
against `hushai_test`. State in "What I didn't check" anything you couldn't exercise (e.g.
model-gated vision tests, the Android build, physical-camera tiers).
