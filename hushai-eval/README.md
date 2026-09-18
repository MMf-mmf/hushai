# hushai-eval — end-to-end regression harness

Inject **known** clips into the **live** pipeline, wait for processing to truly complete, query the
results, score them against ground truth, and emit a machine-readable **improvement / regression /
unchanged** verdict + an exit code. This is the inner loop an agent (or a human) drives:
*make a change → run the suite → read the verdict → iterate until the target metric improves with no
regressions.*

It is the **deterministic / file-injection** tier (Tier 1) — the regression backbone. The physical
"camera-at-screen" realism tier (Tier 2) is **built** too (`local_dev/physical_loopback.py`) and
reuses the same fixtures + scorers.

> **Agents: read `hushai-eval/RECURSIVE_TESTING.md`** — the full playbook (bring-up, the
> validate-a-change loop, the labeling flow, trust invariants, troubleshooting). This README is the
> quick reference.

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
DATABASE_URL=postgres://$USER@localhost:5432/hushai_test sqlx migrate run --source hushai-backend/migrations

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

Current corpus — synthetic (macOS `say`, `build_fixtures.sh`) + real public-domain clips
(`fetch_eval_clips.sh`); all ground truth human-verified:

| case | split | scores | notes |
|------|-------|--------|-------|
| `asr_short` | train (fast) | transcript | single TTS voice; WER ≈ 0.06 |
| `two_speakers` | train (full) | transcript, events | 2 TTS voices; diarization staged (currently merges to 1 voice — next target) |
| `jfk_moon` | train (full) | transcript, **speakers**, events | real JFK speech; WER 0.23; **speaker gate: mints exactly 1 voice** |
| `fdr_infamy` | train (full) | transcript, events | real 1941 archival audio; WER 0.56 (degraded-audio baseline) |
| `armstrong_step` | train (full) | transcript, events | real Moon-radio audio; WER 0.43 (noisy baseline) |
| `car_object` | train (full) | objects | PD Peugeot iOn photo, ken-burns'd; guards the RF-DETR COCO-91 decode (a COCO-80 mislabel turns `car`→`motorcycle`, F1=0) |
| `face_id` | train (full) | persons | PD Judith Resnik NASA portrait; SCRFD detect + ArcFace identity (`distinct_count: 1`) |
| `plate_ocr` | train (full) | plates | PD Auckland street plate `EMD774`; full ALPR: RF-DETR ROI → YOLOv9-t plate detect → fast-plate-ocr |
| `money_talk` | train (full) | transcript, sentiment, **chat** | 2-voice worry/reassure dialogue; RAG money-recall + worried-tone recall + no-hallucination decline |
| `clip_speaker_roster` | train (full) | transcript, **chat** | voice enrolled as Morgan (JFK window); deictic "who was speaking in this clip" → named answer + attributed citation, plus no-playback fallback + recency path |
| `repeat_visitor` | train (full) | transcript, **chat** | same voice on 2 cameras a day apart (multi-clip `Meta.injections[]`); RAG recall + routing + no-hallucination decline + a counting probe |
| `silence_no_speech` | holdout (full) | transcript, speakers | counter-fixture: must mint **0** speakers |

## The labeling loop (`probe`)

Turn any clip into a human-verified fixture:

```
cargo run -p hushai-eval -- probe --audio path/to/clip.wav [--case NAME] [--vision]
```

It resets the test DB, injects the clip, processes it, prints everything the pipeline heard
(transcript + sentiment + speakers + events), and writes a DRAFT `fixtures/staging/<case>/` with
`expected.json` PRE-FILLED from the pipeline's own output. You review the printout, correct the
ground truth, then promote it to `fixtures/train/` and `--update-baseline`. Real fixtures' media is
re-fetchable via `local_dev/fetch_eval_clips.sh`.

## Current coverage & known findings

- **Working & baselined:** transcript (WER + normalized similarity + window-phrase containment),
  events (type/subject/count + severity floor), the speaker-**count** counter-fixture, sentiment
  (allow-set accuracy, `money_talk`), and all three vision lanes — objects (label-set F1,
  `car_object`), persons/faces (count + named, `face_id`), plates (normalized string + read recall +
  named, `plate_ocr`).
- **RAG-CHAT ANSWER scoring:** the `chat` (alias `rag`) modality scores the live `/v1/rag/chat` answer
  itself, not just perception. Per question the harness POSTs to the running RAG, parses the SSE, and
  scores deterministic-first assertions that survive LLM wording — `must_contain` / `must_not_contain` /
  `expect_number` / `expect_routed_agent` (vs the SSE `routed_agent_id`) / `min_citations` /
  `citation_must_attribute` — plus Info-only `reference_answer` cosine + optional LLM-judge (never gate).
  Multi-clip `Meta.injections[]` stage one subject across days/cameras (`repeat_visitor`, `money_talk`,
  `clip_speaker_roster`; a question may also carry `playback` for deictic "in this clip" asks). See
  `RECURSIVE_TESTING.md` §6 (the PI-workflow layer + the harness-driven RAG wins) for the deep playbook.
- **Scorers wired, awaiting inputs:** diarization (Hungarian-mapped purity + named-match) — staged off
  `two_speakers.modalities` until the 2-voice merge is fixed; it scores as soon as the fixture lists
  the modality.
- **✅ Speaker-lane bug — found AND fixed by this harness (the first recursive-testing win).** The
  harness surfaced that the speaker lane minted **0 speakers** on every clip (TTS *and* real audio:
  a constant ~0.31s post-VAD speech). Root cause: `hushai-worker/src/speaker.rs::detect()` fed the
  whole buffer to sherpa's Silero VAD in one `accept_waveform` call, which emits a single ~0.31s
  segment regardless of input. Fix: feed 512-sample windows in a loop, draining segments (verified
  by the `vad_probe_real_speech` diagnostic: 0.31s → 4.41s on a 14s clip). Now JFK mints 1 voice;
  `jfk_moon` + `silence_no_speech` gate it.
- **Remaining diarization gap (next target):** `two_speakers` still merges its 2 distinct voices into
  1 (short 2s-turn TTS + the speaker-window aggregation crossing the turn boundary). Diarization is
  staged off `two_speakers.modalities`; the target (`distinct_count: 2`) is documented in its
  `expected.json`. Enable it once the merge is fixed — **do not** loosen mint/match thresholds to mask it.
- **✅ Vision lanes provisioned & guarded (2026-07-01):** objects decode COCO-91 (`car_object`),
  SCRFD is the default face detector + ArcFace identity (`face_id`), and ALPR runs end-to-end —
  RF-DETR vehicle ROI → YOLOv9-t plate detect → fast-plate-ocr → `EMD774` (`plate_ocr`). All three
  vision lanes now carry a Tier-1 guard; the decode/detector fixes are in `RECURSIVE_TESTING.md` §6.
- **Pending:** the physical camera tier (Tier 2, built) still needs a rig; per-deployment
  ASR-hallucination and speaker-gate calibration on real room audio remain open (both documented in
  `RECURSIVE_TESTING.md` §6 — do not blind-tune).
