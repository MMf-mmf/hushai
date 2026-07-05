# hushai-loadtest — camera capacity benchmark

Answers **"how many cameras can this machine analyze in real time, and where does the per-camera
time go?"** It replays one clip as *N* synthetic cameras, ramps 1→N, and reports the **saturation
point** (largest N the worker keeps up with), the **bottleneck stage**, and a per-camera CPU/GPU/ANE
cost you can extrapolate to a 30-camera deployment. See also [`docs/hardware-sizing-30-cameras.md`](../docs/hardware-sizing-30-cameras.md).

## How it works (one paragraph)

The backend never decodes video, so 30 cameras is only ~45–126 Mbps of ingest — trivial. The cost is in
the **worker** (Whisper ASR + vision models). The harness fans one pre-split clip into *N* virtual
cameras using **identity-only, byte-identical replay**: it reuses the exact encoded segment bytes and
only re-tags the manifest with a fresh `device_id`/`session_id`/`segment_id` per replica + per emit
(`source_kind="loadtest_replica"`). Duplication is therefore ~free (an `Arc` clone + a tiny protobuf
encode + the POST), so the generator never perturbs what it measures. Cameras emit at **wall-clock
realtime cadence** (staggered, POST decoupled from the tick). The controller adds cameras one step at a
time (cumulative load), soaks at each N, scrapes the worker/backend Prometheus `/metrics` + the viewer
`/api/dashboard`, samples host load, and decides whether the worker is keeping up.

---

## Prerequisites — read these two first

1. **The whole stack must share ONE `DATABASE_URL`.** The backend (ingest), the worker (processing),
   and the viewer (`/api/dashboard`) must all point at the *same* Postgres database, or ingested
   segments are invisible to processing and you'll measure nothing (throughput reads 0, lag stays 0).
   The simplest guarantee is to bring everything up with `./local_dev/run_stack.sh`, which loads the
   same env for all services. (This bit us once — see Troubleshooting.)
2. **Start each run from a drained queue.** A pre-existing backlog makes `oldest_pending_age` huge and
   trips the hard-abort on step 1. `run_loadtest.sh` drains before each run; a bare `cargo run` does
   **not** — drain first or expect a misleading "didn't keep up at N=1".

Also required: `ffmpeg` on PATH (to pre-split the clip, once, cached under `local_dev/.feed_work/`),
the worker's models provisioned (Whisper at least; vision/plate models self-disable if absent), and
Ollama up if `SENTIMENT_ENABLED`/embeddings are on.

---

## Quick start (single ramp)

```bash
# 1) bring the stack up (backend :8080, worker :9100/metrics, viewer :8070), one consistent DB
./local_dev/run_stack.sh

# 2) ramp to 30 cameras, full pipeline
cargo run -p hushai-loadtest -- \
  --video IMG_7256.mp4 --max-cameras 30 --soak-secs 90 --profile everything
#   --no-powermetrics    # skip the sudo GPU/ANE sampler (CPU/RSS via `ps` still works)

# 3) remove the synthetic devices when done
cargo run -p hushai-loadtest -- --cleanup
```

Outputs land in `loadtest-out/run-<utc-ts>-<profile>/`:

| File | What |
|---|---|
| `report.md` | Headline saturation N, bottleneck stage, per-N table, hardware extrapolation |
| `summary_by_N.csv` | One row per camera count (steady-state aggregates) |
| `timeseries.csv` | Every sample (throughput, queue depth, lag, per-stage latency, CPU/GPU/ANE) |
| `run.json` | Machine-readable verdict + config + host |
| `live.json` | Rolling status the viewer dashboard panel reads |

---

## Profile sweep — attribute each subsystem's cost

`local_dev/run_loadtest.sh` restarts the worker with each profile's env (the worker reads all toggles
at startup — there is no runtime reconfig), drains, and runs a ramp per profile. The marginal cost of a
subsystem = the drop in saturation-N when you enable it.

```bash
./local_dev/run_loadtest.sh audio-only audio-sentiment audio-vision everything
./local_dev/run_loadtest.sh conc1 conc2 conc4 conc6        # audio-lane fan-out sweep
./local_dev/run_loadtest.sh visconc1 visconc2 visconc4     # vision-lane fan-out sweep
./local_dev/run_loadtest.sh --release --max 30 --soak 120 everything
```

| Profile | Sets | Isolates |
|---|---|---|
| `audio-only` | `VISION_ENABLED=false SENTIMENT_ENABLED=false` | the pure ASR+embed+speaker ceiling |
| `audio-sentiment` | `+ SENTIMENT_ENABLED=true` | the Ollama sentiment LLM cost |
| `audio-vision` | `VISION_ENABLED=true`, object/plate models pointed at a nonexistent path | the face lane (detect + ArcFace) |
| `everything` | vision + sentiment + plates + events all on | the full pipeline |
| `conc1/2/4/6` | `everything` + `WORKER_CONCURRENCY=N` | how saturation scales with **audio** parallelism |
| `visconc1/2/4` | `everything` + `WORKER_CONCURRENCY=2` + `VISION_CONCURRENCY=N` | how saturation scales with **vision** parallelism |

> The `conc*`/`visconc*` sweeps interact with the derived thread budget (next section): more loops ⇒
> fewer threads per call. Read the saturation N *and* the worker's "concurrency budget" log line together.

> The script stops the worker on exit, so after a sweep restart your stack worker
> (`./local_dev/run_stack.sh`) for normal use.

---

## The tuning levers (worker env)

These are what you sweep to move the saturation point. The worker logs a **"concurrency budget"** line
at startup — *check it first when reading a run.*

| Env | Default | Effect |
|---|---|---|
| `WORKER_CONCURRENCY` | 2 | parallel **audio** processing loops |
| `VISION_CONCURRENCY` | 2 | parallel **vision** processing loops |
| `ASR_THREADS` | 0 → derived | Whisper `n_threads`; `0`/unset ⇒ `clamp(cores / (WORKER_CONCURRENCY + VISION_CONCURRENCY), 1, cores)` |
| `ORT_INTRA_THREADS` | 0 → derived | ONNX intra-op threads; same derived budget |
| `FRAMES_PER_SEGMENT` | 2 | vision frames sampled per ~2 s segment |
| `SENTIMENT_ENABLED` / `VISION_ENABLED` / `PLATE_ENABLED` / `EVENTS_ENABLED` | true | toggle stages |

**The key tradeoff:** raising `WORKER_CONCURRENCY` runs more segments in parallel but *shrinks* the
derived per-call thread budget (`cores / (audio+vision loops)`), so each Whisper/ONNX call gets slower.
There's an empirical sweet spot per machine — that's exactly what the `conc*` sweep finds.

---

## Reading the result

- **Saturation point** = the largest N where the worker keeps up with real time. A step "keeps up" iff,
  over the soak's steady-state tail: `oldest_pending_age` slope ≤ 0.05 s/s (flat) **and** sustained
  throughput ≥ 95% of offered **and** mean lag ≤ 3× segment duration **and** no errors.
- **Bottleneck stage** = the `hushai_worker_stage_seconds{stage}` with the largest mean at saturation.
- **Per-camera cost / 30-cam projection** in `report.md` is only meaningful in the keeping-up (linear)
  regime; above saturation the limit is the queue, not compute.
- **Live dashboard panel:** start the viewer with `VIEWER_LOADTEST_LIVE_JSON=local_dev/logs/loadtest-live.json`
  and run via `run_loadtest.sh` (it writes that file). The System dashboard then charts camera-count vs
  load with a marker at the saturation knee. (Without it, the CSV/`report.md` are still produced.)

The new metrics the worker exposes (consumed here, also useful in Grafana): `hushai_worker_stage_seconds{lane,stage}`,
`hushai_worker_segment_seconds{lane}`, `hushai_worker_capture_lag_seconds{lane}` (histograms).

---

## CLI reference

`--video` (clip to replay) · `--url` (backend ingest, default `http://localhost:8080/v1/segments`) ·
`--token` (or `$DEVICE_TOKEN`) · `--worker-metrics` / `--backend-metrics` / `--dashboard` ·
`--max-cameras` (30) · `--step` (1) · `--seg-seconds` (2) · `--soak-secs` (150) · `--sample-secs` (5) ·
`--abort-lag-secs` (300) · `--out-dir` (`loadtest-out`) · `--profile` (run label) · `--work-dir` ·
`--worker-pid-file` (`local_dev/logs/worker.pid`, for CPU/RSS attribution) · `--no-powermetrics` ·
`--live-json` · `--cleanup` · `--insecure`. Full list: `cargo run -p hushai-loadtest -- --help`.

---

## Cleanup

`cargo run -p hushai-loadtest -- --cleanup` deletes every `source_kind="loadtest_replica"` device via
the backend API — one `DELETE /v1/devices/{id}` per device, which cascades correctly even for
fully-analyzed devices: the delete NULLs the `first_seen_device_id` back-refs on speakers/persons/plates,
then tears down the device's segments → derived child tables → streams → sessions → events in a single
transaction (`hushai-backend/src/devices.rs`). No manual SQL cleanup is needed.

---

## Troubleshooting

- **Every step reads `throughput=0`, `lag=0`, queue depth flat, but ingest is accepted** → the backend
  and worker/viewer are on **different databases**. Confirm one `DATABASE_URL` everywhere
  (`ps eww <backend_pid> | tr ' ' '\n' | grep DATABASE_URL`). Bring up via `run_stack.sh`.
- **"didn't keep up at N=1" with a huge `mean lag`** → the queue wasn't drained before the run. Drain
  first (or use `run_loadtest.sh`, which does).
- **GPU%/ANE% are `N/A`** → `powermetrics` needs root; the harness calls it with `sudo -n` (fails fast,
  no prompt) and falls back to `ps` (CPU%/RSS only). Add a NOPASSWD sudoers entry for
  `/usr/bin/powermetrics` to capture GPU/ANE, or accept `--no-powermetrics`.
- **Per-stage latency columns read `N/A`** → the worker predates the `hushai_worker_stage_seconds`
  histograms; rebuild + restart the worker.

---

## Findings — first run (2026-06-29, Apple M3 Pro, 12 cores, default config)

Full pipeline (audio + faces; objects/plates not provisioned), `WORKER_CONCURRENCY=2`, CPU Whisper:

- **Saturation = 1 camera** in real time (fails at 2; at N≥2 the backlog runs away — queue 11→194,
  capture-lag 13 s→114 s over the ramp).
- **Bottleneck = `audio/transcribe`** (CPU Whisper, ~1.4–3.4 s per 2 s segment under the derived
  3-thread budget).
- **≈ 2 CPU cores per camera** → ~60 cores to do 30 cameras ⇒ **not feasible on CPU**; reaching 30
  needs a **GPU/Metal-accelerated Whisper** build and/or a higher `WORKER_CONCURRENCY` (re-measure the
  thread-budget tradeoff with the `conc*` sweep).

Two stack bugs this run surfaced (independent of the harness): a split-brain `DATABASE_URL` (backend on
`hushai_test`, worker/viewer on `hushai`), and `DELETE /v1/devices/{id}` returning 500 on populated
devices (an FK-cascade gap) — the latter since fixed: the delete now NULLs the identity back-refs, then
tears down segments → child tables → streams → sessions → events in one transaction
(`hushai-backend/src/devices.rs`).
