**Title:** `[hushai-rag + hushai-android] - Replace the robotic on-device assistant voice with a natural neural voice synthesized on the backend`

**Status: DONE (2026-06-26) — built + verified on the backend (audible) and the app installs/runs on the S8. One human-only step remains: physically say the wake word into the phone to hear it play through the phone speaker (see "Where we left off").**

- **Context / why**:

  The voice assistant spoke answers with Android's built-in `android.speech.tts.TextToSpeech`
  (only `Locale.US` set — no voice/pitch/rate), which sounded old and robotic. Goal: the **best
  modern, natural, fully-local voice**. Per the user's direction — *"the phone does no AI
  processing; it's a gateway for video capture and audio relay"* — synthesis runs on the **backend**
  (this Apple-Silicon Mac, unconstrained vs. the 2017 Galaxy S8), and the phone just plays the audio.
  Voice character chosen: **neutral male**. This is **Phase 1**; Phase 2 (move wake-word/STT/
  speaker-ID server-side too) is `Issues/unfinished/backend-wakeword-stt-speakerid-phone-as-gateway.md`.

- **What was built**:

  **Engine + model.** Kokoro-82M (Apache-2.0, top-tier naturalness) via the **official
  `sherpa-onnx` Rust crate v1.13** (the maintained successor to the now-deprecated `sherpa-rs`). Its
  build script auto-downloads a prebuilt onnxruntime and links it statically — builds clean on macOS
  arm64, no cmake/system lib. Model fetched (not committed; ~330 MB) by
  `local_dev/fetch_tts_model.sh` → `models/kokoro-en-v0_19/` (`model.onnx`, `voices.bin`,
  `tokens.txt`, `espeak-ng-data/`). 11 speakers, 24 kHz output.

  **Backend (`hushai-rag`, new `POST /v1/tts` → `audio/wav`):**
  - `src/tts.rs` — `Tts` wrapper around `OfflineTts` (Kokoro config); `synthesize_wav(text)` →
    `generate_with_config` → 16-bit mono WAV via `hound`. Loaded once into `AppState` (warm), behind
    `Arc`. Unit tests for WAV header + i16 clamping.
  - `src/routes.rs` — `tts_synthesize` handler (auth reuse via extracted `check_auth`, empty/too-long
    guards, `spawn_blocking` for the CPU-bound synth, returns `audio/wav` bytes). 503 when TTS off.
  - `src/config.rs` — `RAG_TTS_ENABLED` (default on), `RAG_TTS_DIR` (default `models/kokoro-en-v0_19`),
    `RAG_TTS_SID` (default **6 = am_michael**), `RAG_TTS_SPEED` (1.0), `RAG_TTS_THREADS` (2).
  - `src/state.rs` / `src/lib.rs` — `tts: Option<Arc<Tts>>` in `AppState`, loaded at startup
    (non-fatal if model missing → logs + serves without spoken answers), route registered.
  - `examples/tts_audition.rs` — writes one WAV per speaker id to `/tmp/kokoro_sid<N>.wav` so you can
    pick the voice (`cargo run -p hushai-rag --example tts_audition -- "text" 5 6 9 10`).
  - `Cargo.toml` — added `sherpa-onnx = "1.13"`, `hound = "3.5"`.

  **Android (`hushai-android`) — fetch the WAV, play it, drop on-device TTS:**
  - `net/TtsClient.kt` — `POST {ragUrl}/v1/tts {text}` → WAV bytes (or null on failure). Reuses
    `Http.rag` + bearer rule, like `RagClient`.
  - `assistant/WavPcm.kt` — pure RIFF/WAVE parser (unit-tested, `WavPcmTest.kt`).
  - `assistant/AudioPlayer.kt` — plays the WAV via `AudioTrack` (`MODE_STREAM`, `USAGE_ASSISTANT`),
    blocks until drained.
  - `assistant/VoiceAssistant.kt` — removed `TextToSpeech`/`UtteranceProgressListener`/`initTts`;
    `speak()` now runs fetch+play on a dedicated `hushai-va-speak` thread and sets `pendingResume`
    when done. State machine, mic-suppression during THINKING/SPEAKING, and watchdog preserved
    (`SPEAK_TIMEOUT_NANOS` bumped 30 s → 60 s).
  - `capture/CaptureService.kt` — `buildAssistant()` constructs a `TtsClient` and passes it in.
  - No new permissions (INTERNET already present; `AudioTrack` needs none).

- **Verification done**:
  - Backend: `cargo test -p hushai-rag` green; live `POST /v1/tts` → HTTP 200 `audio/wav`, valid
    24 kHz mono WAV, empty-text → 400, and the WAV **played audibly** on the Mac via `afplay`.
  - Android: builds with the pinned toolchain; unit tests green (the WAV-parser test caught a real
    truncation bug, fixed); APK installs on the physical Galaxy S8 and the app launches without
    crashing, reaching `voice assistant ready (wake='computer' enrolled=true)`.
  - Coexists with the concurrent **speaker-attribution** work that also landed in `hushai-rag`
    (`speakers` module, `speaker_id`/`speaker_name` filters, 3-arg `llm.answer`) — combined crate
    builds + tests pass.

- **Where we left off (the one remaining step)**:

  Hearing the new voice *through the phone* needs a human to physically say **"computer, <question>"**
  into the S8 mic (can't be automated) **in the enrolled owner's voice** (the device is
  `enrolled=true`), with `ollama serve` running so the RAG answer is produced. Everything up to that
  point is verified. To do it:
  ```bash
  # backend stack
  ollama serve &                                   # needed for the RAG answer (not for TTS)
  RAG_TTS_DIR=models/kokoro-en-v0_19 cargo run -p hushai-rag   # :8090 ; loads Kokoro at startup
  # (Postgres on :5432 must be up; migrations auto-run)
  ADB="$HOME/Library/Android/sdk/platform-tools/adb"
  "$ADB" reverse tcp:8090 tcp:8090 && "$ADB" reverse tcp:8080 tcp:8080
  # On the phone: enable "Voice assistant", then say:  computer, <a question answerable from history>
  ```
  NOTE: the long-running `cargo run` server gets **reaped** as a background task in this env — run it
  in a foreground terminal for the test.

- **Voice selection**: default `RAG_TTS_SID=6` (am_michael, neutral American male). Alternatives in
  `kokoro-en-v0_19`: am_adam=5, bm_george=9, bm_lewis=10. Audition WAVs were generated to
  `/tmp/kokoro_sid{5,6,9,10}.wav`. Changing the voice is one env var (no rebuild).

- **Gotchas learned**:
  - `gen` is a **reserved keyword in Rust edition 2024** (this crate's edition) — don't name a var `gen`.
  - sherpa-onnx Rust API: `OfflineTts::create(&cfg) -> Option`; `generate_with_config(text,
    &GenerationConfig{sid, speed, ..}, None::<fn(&[f32],f32)->bool>)`; `GeneratedAudio.samples()` +
    `.sample_rate()`. `OfflineTts` is `Send + Sync`.
  - The real model artifact is the release **tarball** `kokoro-en-v0_19.tar.bz2` (loose
    `kokoro-82m.onnx` / `voices-v1.0.bin` URLs that early research suggested do NOT exist).

- **Working-tree state (uncommitted as of this checkpoint)**:
  - New (mine): `hushai-rag/src/tts.rs`, `hushai-rag/examples/tts_audition.rs`,
    `hushai-android/.../net/TtsClient.kt`, `assistant/AudioPlayer.kt`, `assistant/WavPcm.kt`,
    `app/src/test/.../WavPcmTest.kt`, `local_dev/fetch_tts_model.sh`, this file, and the Phase-2
    issue.
  - Modified (mine): `hushai-rag/{Cargo.toml,src/config.rs,src/state.rs,src/lib.rs,src/routes.rs}`,
    `hushai-android/.../VoiceAssistant.kt`, `capture/CaptureService.kt`, `AGENTS.md`,
    `hushai-android/README.md`.
  - Also present in the tree (separate, concurrent speaker-attribution feature — not part of this
    ticket): `hushai-rag/src/speakers.rs`, `src/llm.rs`, `src/retrieve.rs`, Android
    `net/SpeakersClient.kt`, `ui/VoicesScreen.kt`, etc.
  - Nothing committed yet.
