# Worker parallelism & scaling

**Status:** built 2026-06-29. `hushai-worker` compiles; the worker test suite is green (lib 55,
worker_db 5, vision_pipeline 9, ort_coexistence 1, delivery 3 — run serially, see §6). Measuring the
new saturation knee with the loadtest is the **operator step** (§6).

This is the technical reference for the work that made `hushai-worker` genuinely parallel across both
its lanes and right-sized to the box's CPU — "Phases 1–3" of the parallelization plan. AGENTS.md's
"Worker parallelism & scaling" section is the short orientation; this doc is the detail and the
rationale. It assumes familiarity with the worker pipeline (AGENTS.md `hushai-worker/` row,
`hushai-worker/src/{lib,process,claim}.rs`).

> **Deliberately NOT done (and why):** the stage-decoupled pipeline ("Approach B") was deferred — see
> §7. On a CPU box it's Whisper-bound just like this approach and adds a real lease-residency failure
> mode for no current gain. Don't build it until §7's triggers fire.

---

## 1. Why / what changed

The reported symptom was "audio is processed one at a time, and vision is probably similar." Reading
the code corrected and sharpened that:

- **Audio was never one-at-a-time.** It already spawned `WORKER_CONCURRENCY` (default **2**) parallel
  `worker_loop`s draining a `SELECT … FOR UPDATE SKIP LOCKED` queue. The problem was throughput, not
  serialism: each whisper call asked for **all** CPU cores (`asr.rs`), so two loops *oversubscribed*
  the box and netted far less than 2×. "Two at a time, fighting each other," not "one at a time."
- **Vision genuinely was one-at-a-time.** Exactly **one** `vision_worker_loop` was spawned
  (`lib.rs`), by an old comment's reasoning ("CPU-heavy + the mint advisory lock serializes anyway").

| | Before | After |
|---|---|---|
| Audio loops | `WORKER_CONCURRENCY` = 2, each grabbing all cores | `WORKER_CONCURRENCY` = 2 (unchanged default), each sized to a CPU thread budget |
| Vision loops | **1** (hard-coded) | `VISION_CONCURRENCY` = 2, sharing one `Arc`-cloned `VisionModels` |
| Whisper `n_threads` | `available_parallelism()` (all cores) per call | configured budget `cores / (audio+vision loops)` |
| ORT intra-op threads | ORT default (all cores) per session | same budget (CoreML nodes unaffected) |
| Scale-out | — | free: a 2nd worker host on the same DB drains the same SKIP-LOCKED queue |

Net effect: the box moves from "2 oversubscribed loops + 1 lonely vision loop" to "≈cores
genuinely-parallel, right-sized loops across both lanes." The defaults stay conservative (2 / 2);
operators raise them via the loadtest sweep (§6).

---

## 2. Verified thread-safety facts (the load-bearing rationale — do not re-litigate)

The whole approach rests on these, each checked against the actual code and the crate source. Future
work must not "fix" them back into mutexes or model duplication.

- **whisper-rs 0.14.4 is genuinely parallel.** `WhisperInnerContext` and `WhisperState` are
  `unsafe impl Send + Sync` (whisper_ctx.rs:469-470, whisper_state.rs:13-15). `Transcriber`
  (`asr.rs`) shares one `Arc<WhisperContext>` via `&self` and calls `ctx.create_state()` **per call**
  on `spawn_blocking` → each transcription has its own state and runs truly concurrently. There is
  **no** ASR mutex, and adding one would *serialize* it. The only real limiter is the per-call thread
  count (§3).
- **`ort` 2.0.0-rc.9 `Session` is thread-safe for concurrent inference.** `Session` is
  `unsafe impl Send + Sync` (ort session/mod.rs:518-521) and `run` takes `&self` (ONNX Runtime's
  `Run` is thread-safe). `VisionModels` holds `Arc<…>` model handles and is `#[derive(Clone)]`
  (`vision/write.rs`), so N vision loops share **one** set of sessions — no model duplication, no
  added mutex.
- **The speaker embedder IS a deliberate `Arc<Mutex<EmbeddingExtractor>>`** (`speaker.rs`): sherpa's
  `compute_speaker_embedding` takes `&mut self`, so TitaNet inference is serialized across loops. It's
  light and per-segment (num_threads 2) — left as-is. VAD (`VoiceDetector`) is per-call constructed →
  parallel-safe.
- **The speaker + face match/mint `pg_advisory_xact_lock`s are intentionally GLOBAL** (cross-device
  identity space). They hold **DB-only, short** work — embedding happens *outside* the lock
  (`process.rs`). **Never shard them per-device** — that breaks cross-device identity. More loops just
  means more contention on a short lock, which is correct behavior, not a bug.
- **`claim_one` / `claim_one_vision` are `FOR UPDATE SKIP LOCKED`** (`claim.rs`) with a crash-re-lease
  on `claimed_at`. Safe for N concurrent loops **and** N worker processes/hosts — no double-processing.

---

## 3. The CPU thread budget (the change that makes fan-out a win, not a wash)

Every whisper/ORT call previously requested all cores. With M audio + V vision loops each doing that,
the box runs `(M+V)×cores` software threads over `cores` hardware threads → cache thrash, scheduler
overhead, near-zero speedup past ~2. The fix is one shared idea:

```
cores             = available_parallelism()            (fallback 4)
total_loops       = worker_concurrency + vision_concurrency      (≥ 1)
asr_threads       = clamp(cores / total_loops, 1, cores)         whisper n_threads per call
ort_intra_threads = clamp(cores / total_loops, 1, cores)         ORT intra-op per session
```

Computed once in `run()` (`lib.rs`) and logged at startup as **`"concurrency budget"`** (cores,
audio_loops, vision_loops, asr_threads, ort_intra_threads) — the first thing to read in a loadtest run.

Wiring:
- `WorkerConfig` (`config.rs`) gained `cores()`, `total_inference_loops()`, `asr_n_threads()`,
  `ort_intra_op_threads()`. The last two honor the explicit overrides (`ASR_THREADS` /
  `ORT_INTRA_THREADS`); `0`/unset ⇒ the derived budget.
- `Transcriber::new(path, n_threads)` (`asr.rs`) stores the count and uses it in `set_n_threads`
  (replacing the old `available_parallelism()` block). Per-call `create_state()` parallelism is
  untouched — we only shrink the thread pool each call *requests*.
- `model::load_session_with_threads(path, coreml, intra_threads)` (new, `vision/model.rs`) calls
  `SessionBuilder::with_intra_threads(n)` when `n > 0`. `load_session(path, coreml)` is kept as a
  threadless wrapper so the ~17 test callsites are untouched; the worker's 9 production callsites
  (`build_face_detector` / `build_vision_models` in `lib.rs`) use the threaded variant with the
  budget. `with_inter_threads` is left at default.

**CoreML caveat:** when `VISION_COREML=true`, the intra-op cap governs only the **CPU-fallback** nodes
— CoreML/ANE-offloaded nodes are unaffected. That's the safe direction (we never starve the accelerator).

---

## 4. Config knobs

All in `hushai-worker/.env.example` (worker config). Defaults are conservative; sweep to tune (§6).

| Env var | Default | Meaning |
|---|---|---|
| `WORKER_CONCURRENCY` | `2` | Concurrent **audio** pipelines (whisper/embed/speaker). Unchanged default. |
| `VISION_CONCURRENCY` | `2` | Concurrent **vision** pipelines (face/object/plate). Was effectively 1. |
| `ASR_THREADS` | `0` (auto) | Whisper `n_threads` per transcription. `0` ⇒ derived budget (§3). |
| `ORT_INTRA_THREADS` | `0` (auto) | ORT intra-op threads per vision session. `0` ⇒ derived budget. |

**Downstream resources to raise when you increase the concurrencies** (or they become the next choke):
- **DB pool** — `DB_MAX_CONNECTIONS` (workspace-root `.env`, default 16; backend `config.rs`) ≥
  `WORKER_CONCURRENCY + VISION_CONCURRENCY + ~3` (heartbeat/listener/delivery) + backend + rag
  headroom. Near core-many loops, ~32 (and match Postgres `max_connections`). Undersizing blocks on
  acquire (a `db_acquire_timeout` tuning signal), never corrupts.
- **Ollama** — on the Ollama server set `OLLAMA_NUM_PARALLEL ≥ max(WORKER_CONCURRENCY, 4)` so the
  now-parallel embed + sentiment requests don't serialize server-side; under load split
  `EMBED_OLLAMA_BASE_URL` / `LLM_OLLAMA_BASE_URL` onto separate instances. If the loadtest's dominant
  stage flips from `transcribe` to `embed`/`sentiment`, this is the gate that's binding.

---

## 5. Correctness invariants preserved

Fan-out parallelizes *across* segments; none of the per-segment guarantees changed.

- **No double-processing**: `claim_one*` SKIP-LOCKED + the atomic status UPDATE (`claim.rs`).
- **Idempotency**: `write_transcript` delete-then-insert + advisory-locked `assign_speaker` reading
  prior state before the DELETE (`process.rs`, `speaker_match.rs`) — reprocessing yields identical
  rows + identical `speaker_id`. Asserted by `tests/worker_db.rs`.
- **Global mint lock stays global** (cross-device identity); not sharded.
- **Vision segment stays atomic**: each segment is processed start-to-finish on one loop, so
  intra-segment frame ordering required by plate clustering (`vision/write.rs cluster_and_vote_plates`)
  is preserved.
- **Single auto-merge runner**: the "worker 0 only" speaker auto-merge lives solely in the audio
  `worker_loop` (`lib.rs`); vision loops are identity-less. The `.max(1)` on the audio spawn keeps
  worker 0 alive even with a bad concurrency env.

---

## 6. How to verify & tune

**Find the saturation knee (operator step).** Bring the stack up (`./local_dev/run_stack.sh`) and run
the loadtest sweep — both lanes now have profiles in `local_dev/run_loadtest.sh`:

```bash
./local_dev/run_loadtest.sh conc1 conc2 conc4 conc6        # audio: saturation vs WORKER_CONCURRENCY
./local_dev/run_loadtest.sh visconc1 visconc2 visconc4     # vision: saturation vs VISION_CONCURRENCY
```

What proves the win (from `loadtest-out/run-*/report.md` + the metrics):
1. **Thread budget is a free win**: re-running `conc2` after the budget change holds the *same loop
   count* but yields the same-or-higher saturation `N`; host CPU% (`hushai-loadtest/src/sysload.rs`)
   approaches but no longer wildly exceeds `cores×100%`.
2. **Fan-out scales**: at `conc4`/`conc6` and `visconc2`/`visconc4`, saturation `N` climbs,
   `hushai_worker_queue_depth{lane}` drains at a higher camera count, and `oldest_pending_age` /
   `hushai_worker_capture_lag_seconds` stay flat (under `abort_lag_secs`).
3. **Bottleneck stays `transcribe`** in `hushai_worker_stage_seconds{lane,stage}` — if it flips to
   `embed`/`sentiment`, raise `OLLAMA_NUM_PARALLEL` (§4).

**Test suite:** `cargo test -p hushai-worker` with `DYLD_FALLBACK_LIBRARY_PATH=target/debug/deps:target/debug`
and `SQLX_OFFLINE=true`. Run it **`-- --test-threads=1`**: the two `tests/delivery.rs` tests share the
`alert_deliveries` outbox and the delivery loop claims in batches, so running them in parallel makes one
steal the other's rows (a pre-existing cross-test claim race, unrelated to this work). Serially all pass.

---

## 7. What's deferred + the path to 30 cameras

**Single-box CPU ceiling.** This work lifts the knee by a real multiple, but CPU Whisper runs ~2.5–5×
real-time per 2 s clip; ~30 cameras offer ~15 seg/s and need ~90 parallel ASR slots — **unreachable on
CPU cores alone** (`docs/hardware-sizing-30-cameras.md`). The levers past the ceiling, in order:

1. **GPU/Metal Whisper build** (the required next lever for ~30 cams on one box). whisper-rs exposes
   Metal/CUDA acceleration features. Once ASR is fast, re-run the loadtest — the bottleneck will shift
   (likely to Ollama embed/sentiment or the vision lane), which re-opens the Approach-B question.
2. **A 2nd worker host** — free via the SKIP-LOCKED queue: point another worker at the same DB. No code
   change.

**Approach B — stage-decoupled pipeline (deferred).** Decompose `process_segment` into
decode/ASR/embed/write stages connected by bounded channels, each with its own concurrency. It's the
right *eventual* architecture when ASR is on GPU and no single stage dominates — but on a CPU box it's
Whisper-bound like this approach, and it introduces a genuine new failure mode (lease residency: a
segment parked in a channel longer than `LEASE_TIMEOUT_SECS`=300 could be re-claimed → double-processed).
**Revisit only when** GPU/Metal Whisper lands *or* the loadtest shows a non-ASR knee on a multi-core box.

---

## Touched files (this work)

| File | Change |
|---|---|
| `hushai-worker/src/lib.rs` | Vision fan-out loop; `"concurrency budget"` startup log; threaded `load_session_with_threads` at the 9 vision callsites; `Transcriber::new(.., asr_n_threads())` |
| `hushai-worker/src/config.rs` | `vision_concurrency` / `asr_threads` / `ort_intra_threads` fields + `cores()` / `total_inference_loops()` / `asr_n_threads()` / `ort_intra_op_threads()` |
| `hushai-worker/src/asr.rs` | `Transcriber` gains a configurable `n_threads` |
| `hushai-worker/src/vision/model.rs` | new `load_session_with_threads`; `load_session` kept threadless for tests |
| `hushai-worker/.env.example` | new knobs + DB-pool / `OLLAMA_NUM_PARALLEL` guidance |
| `local_dev/run_loadtest.sh` | `visconc1/2/4` profiles |
| `AGENTS.md`, `docs/hardware-sizing-30-cameras.md` | orientation + sizing updates |

_Last updated 2026-06-29._
