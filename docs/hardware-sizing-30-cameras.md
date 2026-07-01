# Hardware sizing for 30 cameras

How much computer + network do you need to **capture and analyze up to 30 cameras at once** with Hushai?
This guide gives the industry-standard numbers, explains where the real bottleneck is, and shows how to
fill in the exact per-camera cost on *your* hardware with the bundled load-test harness.

> TL;DR — **The cable is not the limit; compute is.** 30 cameras is only ~45–126 Mbps of ingest
> (trivial for one gigabit link), but the AI worker (transcription + vision) is what actually caps the
> camera count. Measure it with `hushai-loadtest`, then size CPU/GPU from the measured per-camera cost.

---

## 1. The network / "all over a cable"

Hushai ingest is plain HTTPS `POST /v1/segments` of ~2 s media chunks. The backend never decodes video —
it stores opaque, content-addressed bytes — so ingest is cheap and bandwidth-bound only.

| Camera | Codec | Per-camera bitrate | ×30 cameras |
|---|---|--:|--:|
| 1080p @ 25–30 fps | H.265 | 1.5–3.0 Mbps | **45–90 Mbps** |
| 1080p @ 25–30 fps | H.264 | 2.5–5.0 Mbps | 75–150 Mbps |
| 4 MP @ 25–30 fps | H.265 | 2.2–4.2 Mbps | **66–126 Mbps** |
| 4 MP @ 25–30 fps | H.264 | 3.2–8.0 Mbps | 96–240 Mbps |

Add the usual ~20% headroom for motion spikes/keyframes and you are still comfortably **under ~200 Mbps**
for 30 H.265 cameras — well inside a single **1 GbE** uplink.

**Cabling recommendation**
- Wired **PoE** cameras into a managed PoE switch; one **gigabit** uplink from the switch to the host is
  enough for 30 cameras at 1080p/4 MP H.265. (For >16 cameras, dedicate that gigabit uplink.)
- Only consider **2.5/10 GbE** if you go 4K@30 across many cameras (then per-camera bitrate ~8–16 Mbps).
- Prefer **H.265** end-to-end: ~30–50% less bandwidth and ~20–40% more retention per TB than H.264.

Conclusion: for 30 cameras the network is a solved problem. Budget the money for **compute and disks**.

---

## 2. The real constraint: the AI worker

Every ~2 s segment is analyzed by `hushai-worker`. Per segment the pipeline does, roughly:

- **Audio lane** (`WORKER_CONCURRENCY` parallel tasks, default 2): ffmpeg decode → **Whisper ASR**
  (the dominant cost, ~2.5–5× real-time per 2 s clip on a CPU build) → sentiment (Ollama) → speaker
  VAD + TitaNet embedding → text embedding → DB write.
- **Vision lane** (`VISION_CONCURRENCY` parallel tasks, default 2): ffmpeg frame decode → face detect
  (SCRFD/YuNet) + ArcFace embed (+ optional GFPGAN/Real-ESRGAN restore) → RF-DETR objects + CLIP →
  plate detect + OCR → DB write.

Both lanes fan out over a SKIP-LOCKED queue, so raising `WORKER_CONCURRENCY` / `VISION_CONCURRENCY`
parallelizes immediately (and a 2nd worker host against the same DB scales out for free). The catch:
each Whisper/ORT call otherwise grabs **all** cores, so N parallel loops oversubscribe the box. The
worker sizes a **per-call thread budget** (`ASR_THREADS` / `ORT_INTRA_THREADS`, `0`=auto =
`cores / (audio+vision loops)`) so fan-out is a real win — logged at startup as "concurrency budget".

Because Whisper alone runs several× real-time, the system still **saturates below 30 cameras at stock
settings** — the offered rate (30 cams × one 2 s segment / 2 s = 15 segments/s) far exceeds what a
handful of CPU Whisper workers drain. Fan-out + the thread budget move the knee up by a real multiple
(from "2 oversubscribed loops" to "≈cores genuinely-parallel right-sized loops"), but 30 cameras on
one box needs a GPU/Metal Whisper build (§3). This is the number the benchmark measures.

Industry reference points for *decode + light AI* (your pipeline is heavier, so expect lower density):

- **NVIDIA NVDEC** hardware decode: ~16–32 simultaneous 1080p streams per GPU (codec/VRAM dependent).
- **NVIDIA DeepStream** with a *lightweight* detector: ~16–32 streams per T4-class GPU; frame-skip +
  tracking can roughly double that. Hushai's multi-model pipeline (ASR + faces + objects + plates) is
  far heavier per stream, so treat these as optimistic ceilings, not targets.

### Three deployment tiers

| Tier | Decode | Inference | Realistic 30-cam posture |
|---|---|---|---|
| **CPU-only edge** | ffmpeg (CPU) | CPU ONNX + CPU Whisper | Lowest density. Likely needs several boxes or heavy stage-trimming (audio-only, low `FRAMES_PER_SEGMENT`) to approach 30. Cheapest per box. |
| **Apple Silicon** | VideoToolbox/ffmpeg | CoreML (ANE/GPU) + Metal Whisper | Unified memory; ANE accelerates the vision models. Good mid density on one machine. *This is the tier the bundled benchmark profiles to.* |
| **Linux + NVIDIA GPU** | **NVDEC** (offload decode off CPU) | CUDA/TensorRT inference + GPU-accelerated Whisper | Highest density; best path to 30 cameras on one host. Size GPU count from measured per-camera GPU%. |

The right tier and box count come from the **measured per-camera cost** (§3), not a guess.

---

## 3. Measure it: the `hushai-loadtest` harness

> **Full operational guide:** [`hushai-loadtest/README.md`](../hushai-loadtest/README.md) — prerequisites,
> profiles, tuning knobs, output interpretation, troubleshooting, and cleanup. This section is the summary.

`hushai-loadtest` (workspace crate) replays one clip as **N synthetic cameras** (identity-only,
byte-identical fan-out — the fastest possible duplication, so the generator never perturbs the
measurement), ramps **1 → 30 cumulatively**, holds a soak at each step, and records:

- worker throughput (`hushai_segments_processed_total`), per-lane **queue depth**, and
  **`oldest_pending_age_secs`** (the realtime-lag signal);
- **per-stage latency** (`hushai_worker_stage_seconds{lane,stage}`) — decode, ASR, each vision model,
  DB write — so you see *where the time goes*;
- **host load** on macOS — worker CPU%/RSS (`ps`) and, with sudo, GPU/ANE residency (`powermetrics`).

It declares the **saturation point** = the largest N where the worker keeps up with real time
(flat `oldest_pending_age`, throughput ≥ ~95% of offered, bounded lag, no errors), names the
**bottleneck stage**, and extrapolates a per-camera CPU/GPU/ANE cost.

### Run it

```bash
# 1) bring the stack up
./local_dev/run_stack.sh

# 2) single profile (full pipeline), ramp to 30
cargo run -p hushai-loadtest -- --video IMG_7256.mp4 --max-cameras 30 --profile everything
#    (add --no-powermetrics to skip the sudo GPU/ANE sampler)

# 3) attribute per-subsystem cost across profiles
./local_dev/run_loadtest.sh audio-only audio-sentiment audio-vision everything
./local_dev/run_loadtest.sh conc1 conc2 conc4 conc6        # audio-lane: saturation vs WORKER_CONCURRENCY
./local_dev/run_loadtest.sh visconc1 visconc2 visconc4     # vision-lane: saturation vs VISION_CONCURRENCY

# 4) clean up synthetic devices when done
cargo run -p hushai-loadtest -- --cleanup
```

Outputs land in `loadtest-out/run-<ts>-<profile>/`: `report.md` (headline saturation N + bottleneck +
extrapolation), `summary_by_N.csv`, `timeseries.csv`, `run.json`. For the **live dashboard panel**, start
the stack with `VIEWER_LOADTEST_LIVE_JSON=local_dev/logs/loadtest-live.json` exported and use
`./local_dev/run_loadtest.sh` (which writes that file) — the viewer's dashboard then charts
camera-count vs load with a marker at the saturation knee.

### From measurement to a 30-camera spec

In the **keeping-up (linear) regime**, the harness reports per-camera cost; project it:

- `cores_for_30 ≈ 30 × (per_camera_cpu_pct / 100)` CPU cores of headroom.
- `WORKER_CONCURRENCY` needed ≈ `30 × (single-segment audio processing time / segment duration)`
  (e.g. one 2 s segment taking 6 s of ASR ⇒ ~`30 × 6/2 = 90` parallel audio slots — clearly a job for a
  GPU-accelerated Whisper build, not 90 CPU cores). The `conc*` profiles measure how saturation N rises
  with `WORKER_CONCURRENCY` so you can find the knee.
  with `WORKER_CONCURRENCY` (`visconc*` does the same for `VISION_CONCURRENCY`) so you can find the knee.
- If the **vision** lane saturates first while CPU has headroom → the deployment needs GPU/ANE
  acceleration. If **audio** (CPU Whisper) saturates first → more cores or a GPU Whisper build.
- If ffmpeg **decode** shows up as a dominant stage → offload it (NVDEC / VideoToolbox).

**Don't let a shared resource cap the fan-out.** When you raise the concurrencies, size these to match
or the dominant stage will silently shift off Whisper onto a queue you didn't tune:
- **DB pool:** `DB_MAX_CONNECTIONS` (default 16) ≥ `WORKER_CONCURRENCY + VISION_CONCURRENCY + ~3`
  (heartbeat/listener/delivery) + backend + rag headroom. Near core-many loops, ~32 (and match
  Postgres `max_connections`). Undersizing blocks on acquire (a `db_acquire_timeout` tuning signal),
  never corrupts.
- **Ollama:** set `OLLAMA_NUM_PARALLEL` ≥ `max(WORKER_CONCURRENCY, 4)` so the now-parallel embed +
  sentiment requests don't serialize server-side; under load split `EMBED_OLLAMA_BASE_URL` and
  `LLM_OLLAMA_BASE_URL` onto separate instances. If the report's dominant stage flips from `transcribe`
  to `embed`/`sentiment`, this is the gate that's binding.
- **CPU thread budget:** leave `ASR_THREADS`/`ORT_INTRA_THREADS` at `0` (auto) unless profiling shows
  loops sitting idle on I/O — then pin them. The startup "concurrency budget" log shows the derived values.

---

## 4. Storage

Continuous recording dominates disk sizing; analytics metadata is negligible by comparison.

| Resolution / codec | Per camera / day | ×30 cameras / day | ×30 / 30 days |
|---|--:|--:|--:|
| 1080p H.265 | ~5 GB | ~150 GB | **~4.4 TB** |
| 1080p H.264 | ~10 GB | ~300 GB | ~8.8 TB |
| 4K H.265 | ~15–20 GB | ~450–600 GB | ~13–18 TB |

- Use **surveillance-rated drives** (WD Purple / Seagate SkyHawk) tuned for 24/7 multi-stream writes.
- **Avoid SMR** drives — they cannot sustain continuous write and start dropping data within months.
- Motion-only recording can roughly halve these. RAID/redundancy is on top of capacity.

> Note: the load-test's identity-only replay reuses **one** stored blob (content-addressing dedups
> identical bytes), so it does **not** exercise real disk-write volume. Size storage from this table
> (`30 × byte_rate × retention`), not from the harness.

---

## 5. Sources

- IP-camera bitrate / bandwidth: Reolink, SCW, Hikvision recommended-bitrate tables, cctvcalculator.net.
- NVIDIA NVDEC decode density: NVIDIA Video Codec SDK docs; NVIDIA developer forums.
- DeepStream stream density with AI: NVIDIA DeepStream SDK docs + technical blog.
- Frigate hardware guidance (CPU/Coral/GPU sizing patterns): docs.frigate.video.
- Surveillance storage sizing + surveillance-drive guidance: NVR storage calculators (CCTV Security Pros,
  Security Camera King), WD Purple / Seagate SkyHawk product guidance.
- Video-analytics benchmarking methodology (throughput/latency/dropped-frame metrics): video-analytics
  inference-pipeline benchmarking literature.

## 6. Measured baseline (fill in per machine)

First run — **Apple M3 Pro (12 cores), default config** (`WORKER_CONCURRENCY=2`, `VISION_CONCURRENCY=2`,
derived per-call thread budget = `cores/(audio+vision loops)` ≈ 3 threads each), full audio + face
pipeline on **CPU Whisper**:

| Metric | Value |
|---|---|
| Saturation (real-time) | **1 camera** (fails at 2; backlog runs away beyond) |
| Bottleneck stage | `audio/transcribe` (~1.4–3.4 s per 2 s segment) |
| Per-camera cost | ≈ **2 CPU cores** |
| 30-camera projection | ≈ 60 cores ⇒ **needs GPU/Metal Whisper and/or tuned concurrency, not CPU** |

The audio (CPU Whisper) lane saturates first, so the lever for 30 cameras is a **GPU/Metal-accelerated
Whisper** build, then re-measuring the `WORKER_CONCURRENCY`/`VISION_CONCURRENCY` thread-budget tradeoff
with the `conc*`/`visconc*` sweeps.

---

_Last updated 2026-06-29. Re-run `hushai-loadtest` per target machine and update §6 + the §2 tier rows._
