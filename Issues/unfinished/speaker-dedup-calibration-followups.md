**Title:** `[hushai-worker + hushai-backend] - Calibrate the speaker de-duplication thresholds on real noisy audio + finish the auto-heal ops wiring`

- **Description**:

  The speaker de-duplication overhaul (2026-06-26) fixed the root causes of "background static
  fragments one person into many `unknown speaker` rows" — real Silero VAD, multi-vector k-NN
  matching, mint-guard hysteresis, a self-healing centroid, raw-embedding recluster/duplicate
  detection, going-forward auto-merge, and a "Clean up voices" Android UI. See the
  **"Speaker de-duplication overhaul"** section of `AGENTS.md` and the memory
  `hushai-speaker-dedup-overhaul.md` for the full description of what shipped.

  That work is **built + verified** (workspace `cargo check` clean; worker lib + DB tests pass,
  incl. a live-DB `mint_guard_collapses_dupes_and_refuses_noise` test; backend tests pass; Android
  `assembleDebug` ok; the new `/v1/speakers/{recluster-deep,duplicates,merge-group}` endpoints
  exercised end-to-end against the live DB). What remains are **tuning + ops loose ends**, not
  missing functionality.

  ### Loose ends

  1. **Thresholds are uncalibrated starting guesses (the important one).** Every knob ships at a
     plausible default but was never tuned against real, noisy, multi-speaker capture — the only
     prior calibration used clean-room macOS `say` voices (cross-speaker cosine ≈ 0.84, within < 0.5).
     The knobs (all in `hushai-worker/.env.example`):
     - `SPEAKER_MATCH_THRESHOLD` (0.5), `SPEAKER_MINT_DISTANCE_FLOOR` (0.72) — the hysteresis band.
     - `SPEAKER_KNN_K` (15), `SPEAKER_KNN_NEIGHBOR_CEILING` (0.55), `SPEAKER_KNN_MIN_NEIGHBORS` (3).
     - `VAD_THRESHOLD` (0.5), `VAD_MIN_SILENCE_SECS` (0.3), `VAD_MIN_SPEECH_SECS` (0.25).
     - `SPEAKER_MINT_MIN_SPEECH_SECS` (1.2), `SPEAKER_MINT_MIN_SNR_DB` (10), `SPEAKER_MINT_MIN_VOICED_FRAC` (0.5).
     - `SPEAKER_AUTOHEAL_DISTANCE` (0.15), `SPEAKER_AUTOHEAL_MIN_LINKS` (2).
     Pick `SPEAKER_MATCH_THRESHOLD` ≈ the high percentile (p95) of clean within-speaker distance and
     `SPEAKER_MINT_DISTANCE_FLOOR` ≈ a low percentile (p5–p10) of cross-speaker distance (validate
     `match < floor`). Headline metrics to optimize: **identities-minted-per-true-speaker → ~1.0**
     while **per-segment attribution accuracy** does not collapse.

  2. **No scheduled deep-heal job (intentional, opt-in).** `POST /v1/speakers/recluster-deep` and the
     worker's going-forward `auto_merge_recent` both exist, but there is no `local_dev` launchd/cron
     wrapper (mirroring `partition_maintenance.{sh,plist,pg_cron.sql}`) to run a periodic whole-catalog
     deep-heal. Add one only if periodic catalog-wide cleanup is wanted; it must `curl` the backend
     endpoint (the heal logic holds the advisory lock + renormalizes in Rust — not pure SQL).

  3. **Existing duplicate backlog is not auto-collapsed (by design).** The user chose "fix going
     forward only," so `auto_merge_recent` is scoped to recently-active speakers. Pre-existing
     duplicates surface under the app's **"Clean up voices"** for manual one-tap merge, or run
     `recluster-deep` manually once over the whole catalog.

  4. **VAD detector is constructed per segment.** The sherpa-rs 0.6.8 wrapper exposes `clear()` but not
     the C-API `Reset` (LSTM recurrent-state reset), so a fresh `SileroVad` is built per `detect()` to
     avoid cross-clip state bleed. Cheap for a ~2 MB model; only pool/cache it if profiling shows the
     per-segment construction matters.

- **Acceptance Criteria**:

  - A calibration procedure is run over a real noisy multi-speaker corpus and the chosen
    `SPEAKER_*` / `VAD_*` env values are committed to `hushai-worker/.env.example` with the measured
    metrics (minted-per-true-speaker, attribution accuracy) noted.
  - (Optional) A scheduled deep-heal wrapper is added under `local_dev/` and documented, OR a note is
    added that periodic deep-heal is deliberately operator-triggered only.
  - The decisions in (3) and (4) are confirmed or revised; if revised, the code + `AGENTS.md` are updated.

- **How to Test** (real, end-to-end):

  1. With the stack running (`ollama serve`, backend `:8080`, worker, `models/silero_vad.onnx` present),
     ingest a clip of **one** speaker with heavy background static plus some silence/room-tone segments
     (e.g. via `local_dev/feed_segments.py`, or the Android client in a noisy room).
  2. `GET http://localhost:8080/v1/speakers` (bearer `DEVICE_TOKEN`) → confirm that one noisy speaker
     yields **one** voice, not many, and that pure-static/silence segments leave `speaker_id` NULL.
  3. Ingest two genuinely distinct speakers → confirm exactly two voices, correct attribution, no merge.
  4. In the Android app open **Voices → "Clean up voices"** → confirm any remaining over-splits appear
     as suggested groups and "Merge group"/"Merge all" collapses them; confirm a name-conflict group is
     NOT one-tap mergeable.
  5. Sweep the thresholds, re-run (2)–(3), and record minted-per-true-speaker + attribution accuracy.
