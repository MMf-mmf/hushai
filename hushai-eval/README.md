# hushai-eval — end-to-end regression harness

Inject **known** clips into the **live** pipeline, wait for processing to truly complete, query the
results, score them against ground truth, and emit a machine-readable **improvement / regression /
unchanged** verdict + an exit code. This is the inner loop an agent (or a human) drives:
*make a change → run the suite → read the verdict → iterate until the target metric improves with no
regressions.*

It is the **deterministic / file-injection** tier (Tier 1) — the regression backbone. A future
physical "camera-at-screen" realism tier (Tier 2) reuses the same fixtures + scorers; see the plan
at `~/.claude/plans/getting-recursive-testing-in-resilient-micali.md`.

## The contract (what the agent loop consumes)

```
hushai-eval run --tier {fast|full} [--case ID] [--fixtures train|holdout|all] \
                [--update-baseline [--force]] [--json]
  → stdout: human report, or (with --json) the full SuiteResult
  → exit:   0 = all pass/improved · 1 = regression or absolute-floor breach · 2 = inconclusive/infra
```

`--json` emits `{verdict, exit_code, config_hash, manifest, cases[]}` where each case has per-metric
`{key, value, baseline, delta, classification, floor_ok, detail}`.

## One-time setup

```bash
# 1. Test DB (isolated from the dev `hushai` DB — the harness TRUNCATEs everything each run).
createdb hushai_test
DATABASE_URL=postgres://mf@localhost:5432/hushai_test sqlx migrate run --source hushai-backend/migrations

# 2. Generate the fixture media (gitignored; ground truth in meta/expected is committed).
./local_dev/build_fixtures.sh
```

## Running

Bring up the stack against the test DB with the determinism profile (`local_dev/eval.env`), then run:

```bash
# Stack (test DB + determinism lockdown). Either `./local_dev/run_stack.sh --test-db`,
# or manually (backend from hushai-backend/, worker from repo root):
#   backend: DATABASE_URL=…/hushai_test ./target/debug/hushai-backend
#   worker : DATABASE_URL=…/hushai_test WORKER_CONCURRENCY=1 SPEAKER_AUTOHEAL_ENABLED=false \
#            SPEAKER_BACKFILL_ON_START=false SPEAKER_REPROCESS_REJECTS_ON_START=false \
#            DYLD_FALLBACK_LIBRARY_PATH=target/debug/deps:target/debug:/usr/local/lib:/usr/lib \
#            ./target/debug/hushai-worker

# Establish baselines (first time, or after an intentional pipeline change):
cargo run -p hushai-eval -- run --tier full --fixtures all --update-baseline

# Inner loop (fast, train split only):
cargo run -p hushai-eval -- run --tier fast --json

# Full gate before declaring a task done (includes the sealed holdout split):
cargo run -p hushai-eval -- run --tier full --fixtures all
```

## The agent loop (`/loop`-able)

```
1. baseline once:   hushai-eval run --tier full --fixtures all --update-baseline
2. LOOP (fast):     change code → rebuild only the touched crate
                    → hushai-eval run --tier fast --json
                    → exit 0 & no "regression" classifications → candidate done; else iterate
3. full gate:       hushai-eval run --tier full --fixtures all   (must be exit 0; holdout included)
4. (later) physical realism gate before "done"
```

The agent's stop condition is **machine-checked**: target metric improved AND no metric regressed,
on train AND the sealed holdout. The exit code lets a `/loop` or a Stop-hook drive iteration without
re-reading prose.

## The seven trust invariants (enforced)

1. **Clean pinned state** — `hushai_test` DB; `TRUNCATE … RESTART IDENTITY CASCADE` of all
   result/catalog/status/`segments` tables before every case (`src/reset.rs`).
2. **Determinism lockdown** — `WORKER_CONCURRENCY=1`, auto-merge/backfill/reprocess off, CPU EP
   (`local_dev/eval.env`).
3. **Fixture-pinned timestamps** — the injector stamps a fixed `capture_start_unix_nanos`
   (`feed_segments.py --capture-start-ns`); no wall-clock-relative output is scored.
4. **Config-hash gate** — every run fingerprints models + Ollama digests + ORT + EP + knobs into a
   `config_hash`; baselines are keyed by it, so a model/knob change mints a new lineage instead of a
   bogus regression (`src/manifest.rs`).
5. **Quiescent completion** — wait until every injected segment is terminal in both lane status
   tables, then until event counts stop changing; timeout → **inconclusive (exit 2)**, never scored
   (`src/poll.rs`).
6. **Assignment-invariant scoring** — identity metrics use optimal label assignment + denormalized
   names, never minted UUIDs; float metrics use calibrated tolerance bands (`src/score.rs`,
   `src/baseline.rs`).
7. **Sealed holdout + counter-fixtures** — `fixtures/holdout/` is scored only in the full gate; it
   includes adversarial cases (e.g. silence that must mint **0** speakers).

## Fixtures

`fixtures/<split>/<case>/` with `media.*` (gitignored), `meta.json` (how to inject + which
modalities to score), `expected.json` (ground truth; every modality key optional; time windows are
ns offsets from `base_capture_unix_nanos`), and optional `refs/` enrollment assets. Regenerate media
with `./local_dev/build_fixtures.sh`.

Current corpus (auto-generated, construction-known ground truth):

| case | split | scores | notes |
|------|-------|--------|-------|
| `asr_short` | train (fast) | transcript | single TTS voice; WER ≈ 0.06 |
| `two_speakers` | train (full) | transcript, events | two voices; multi-utterance ASR + speech events |
| `silence_no_speech` | holdout (full) | transcript, speakers | counter-fixture: must mint **0** speakers |

## Current coverage & known findings

- **Working & baselined:** transcript (WER + normalized similarity + window-phrase containment),
  events (type/subject/count + severity floor), and the speaker-**count** counter-fixture.
- **Scorers wired, awaiting inputs:** diarization (Hungarian-mapped purity + named-match), sentiment
  (allow-set accuracy), persons/faces (count + named), objects (label-set F1), plates (normalized
  string + read recall + named). They score as soon as a fixture lists the modality and the lane is
  enabled with weights present.
- **⚠ Speaker-lane finding:** on this machine the speaker lane currently mints **0 speakers** on
  every available clip — TTS **and** real audio (`IMG_7256.mp4`): ~0.31s post-VAD speech, all
  `quality='marginal'`, while Whisper (its own VAD) transcribes the same audio fine. This is a real
  characteristic the harness surfaced; **do not** loosen the mint gates to mask it. Diarization
  scoring is staged off `two_speakers.modalities` until it's investigated. To re-enable, add
  `"speakers"` back to that fixture's `modalities`.
- **Pending (needs weights / your help):** object detection + ALPR fixtures require the RF-DETR /
  CLIP / plate-detector / plate-OCR weights (Phase 0 provisioning). Face fixtures need source
  stills. The physical camera tier needs a rig.
