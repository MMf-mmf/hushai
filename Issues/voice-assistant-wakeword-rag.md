**Title:** `[hushai-android + hushai-rag] - Voice-assistant mode: always-on wake word → owner voice-ID → spoken question → RAG answer spoken back`

> **✅ DONE & VERIFIED (2026-06-24)** — built and verified end-to-end on the physical Galaxy
> S8 with the user speaking. The full loop works: say the wake word ("computer", user-set) →
> owner voice verified → question transcribed → answered from the user's recorded history →
> **spoken aloud**. All offline/on-device except the local RAG call. Implementation notes:
> - **Vosk** does all three speech tasks offline: wake-word spotting, question STT, and the
>   speaker x-vector (`org.vosk.SpeakerModel`, 128-dim) for owner verification — no cloud,
>   no TFLite. Models bundled as APK assets (`assets/vosk/model-en` + `model-spk`), unpacked
>   to filesDir on first run.
> - **Shared mic**: `MicSource` (one 16 kHz `AudioRecord`) fans PCM to the AAC segment encoder
>   AND the assistant; `AudioEncoder` is now push-based with its OWN worker thread so encoding
>   never stalls the mic reader (keeps capture + assistant independent). Capture/upload contract
>   unchanged (verified: cam0-video/cam0-audio still upload `status=200`).
> - **Speaker verification** (greenfield) via cosine vs an enrolled owner centroid. Verified
>   margin on-device: **owner ≈ 0.63–0.80, stranger ≈ 0.41–0.42, threshold 0.50** → owner
>   accepted, stranger correctly ignored.
> - RAG answer via `hushai-rag /v1/rag/query` scoped to `device_id`; TTS via Android
>   `TextToSpeech`. Watchdogs guard the THINKING/SPEAKING/ENROLLING states.
> - **Key bug found + fixed in review/test:** Vosk `getResult()` returns the utterance JSON
>   and RESETS, so calling it twice (once for text, once for `spk`) silently dropped the
>   speaker vector — call it once and parse both from the same string.

- **Description**:

  Turn `hushai-android` from a pure A/V capture front-end into a hands-free **personal voice
  assistant** — an "Alexa for your own life-log" — layered on the existing
  capture→ingest→transcribe→embed→RAG stack (`Issues/initial-android-app.md`,
  `Issues/initial-backend.md`, `Issues/transcription-embedding-and-rag.md`). `hushai-rag`
  already answers questions grounded in the user's transcribed history
  (`POST /v1/rag/query` on :8090); this ticket builds the on-device voice loop in front of it.

  Pipeline (all new on the Android side unless noted):

  1. **Always-on wake-word listening.** A user-configurable keyword set in the app (persisted
     in `config/Settings.kt` DataStore alongside url/token) that the app continuously listens
     for, even screen-off, running inside the existing `CaptureService` foreground service
     (`camera|microphone` + `PARTIAL_WAKE_LOCK`, `START_STICKY`). Use an **on-device/offline
     keyword spotter** so arbitrary user-set words work air-gapped — recommend **Vosk** (small
     en model, free-form keyword spotting from its STT stream); Porcupine is an alternative but
     custom keywords need pre-built `.ppn` artifacts.
  2. **Owner voice identification (greenfield — no speaker/voice-biometric data exists anywhere
     in worker/backend/schema today).** Before acting, verify the speaker is the enrolled
     owner. One-time **enrollment** (owner records a few phrases → compute + store a reference
     speaker embedding on-device) + per-utterance **verification** (embed the utterance,
     cosine-compare to the reference, accept above a threshold). Recommend an **on-device
     speaker-verification model** (TFLite ECAPA-TDNN / x-vector style) to keep voice biometrics
     private/local. Non-owner voices are ignored.
  3. **Question capture + STT.** After an owner-verified wake word, capture the following
     utterance and transcribe it to text **on-device** (reuse the offline STT engine from
     step 1).
  4. **RAG answer.** POST the transcribed question to `hushai-rag` `/v1/rag/query`
     (request `{"query": "...", "filters": {"device_id": "<this device_id>"}}`) so answers are
     grounded in the owner's own recordings; use the returned `answer` (response shape:
     `{answer, sources[{segment_id, device_id, text, start_unix_nanos, distance}]}`). Send
     `Authorization: Bearer <RAG_TOKEN>` if the server has one configured. **No backend change
     required for the happy path** — the endpoint already returns grounded answers + `sources`.
  5. **Spoken response (TTS).** Speak the `answer` back via Android `TextToSpeech`. Reflect
     assistant state in the foreground notification + UI (Listening → Heard wake word →
     Verifying → Listening for question → Thinking → Speaking → Listening).

  **Critical architecture note — shared mic.** The mic is currently *exclusively* owned by
  `capture/AudioEncoder.kt` (one `AudioRecord` on `MIC`, 44.1 kHz mono PCM16 → AAC-LC 2 s MP4
  segments; the raw PCM is ephemeral inside the encoder pump loop). On API 26 (the target S8)
  two concurrent `AudioRecord`s won't reliably both receive mic data, so this requires
  refactoring capture into a **single shared mic-PCM source** that fans out to (a) the existing
  AAC segment encoder and (b) the new wake-word / STT / speaker-ID pipeline — **without
  disrupting the existing segment-upload contract** (`contracts/cameraToBackendContract.md`
  v0.1.0; the 2 s gapless segment streams must keep uploading exactly as today).

  **Scope:** `hushai-android` — shared mic-PCM refactor, wake-word engine, owner
  enrollment + on-device verification, question STT, a RAG client in `net/` (OkHttp, like
  `net/Uploader.kt`), TTS playback, UI for keyword config + enrollment + live assistant state,
  and DataStore settings. `hushai-rag` — none required for the happy path (`device_id`
  filtering already supported via `filters`). New on-device model assets (STT + speaker
  verification) ship/download to the device and stay **offline**, consistent with the existing
  local-only stack (whisper.cpp `ggml-base.en.bin` + local Ollama). Suggested phasing if too
  large for one PR: (A) shared-mic refactor + wake word + state UI; (B) owner enroll + verify;
  (C) STT + RAG client + TTS.

- **Acceptance Criteria**:
  - [x] A wake-word keyword is configurable in the app and **persists across restart**; changing
        it takes effect without reinstall.
  - [x] With the app running (incl. **screen off**), saying the wake word reliably triggers the
        assistant (observable: logcat state transition + notification/UI shows "heard wake
        word"). *(Latency is end-of-utterance bound, ~1–2 s, since Vosk emits the final result
        on a brief pause — acceptable; partial-result wake detection is a possible future tweak.)*
  - [x] Owner **enrollment** flow exists: the owner records sample phrases once; a reference
        voice profile is stored on-device. *(Verified: voiceprint dim=128 captured, "enrolled ✓".)*
  - [x] After the wake word, the speaker is **verified**: the enrolled owner is **accepted** and
        the flow proceeds; a **different person** is **rejected** and gets no answer. *(Verified
        on-device: owner cosine 0.63–0.80, stranger 0.41–0.42, threshold 0.50.)*
  - [x] After owner-verified wake, the spoken question is **transcribed to text**.
  - [x] The question hits `hushai-rag` `/v1/rag/query` scoped to this `device_id`; the
        `answer` is **grounded in the user's recordings** (verified: "You passed the camera
        multiple times.", 8 sources).
  - [x] The answer is **spoken aloud** via TTS; UI/notification reflects each state through the
        full cycle.
  - [x] The existing capture pipeline is **unaffected**: video+audio 2 s segments still upload
        `status=200` with seq advancing while the assistant runs (regression-verified).
  - [x] Everything runs **offline/on-device** except the RAG call to the local backend (no cloud
        STT/TTS/wake-word dependency), consistent with the local whisper+Ollama stack.
  - [x] `./gradlew :app:assembleDebug` green; existing JVM unit tests pass; new logic (keyword
        match via `SpeakerMathTest`, speaker-similarity via `SpeakerMathTest`, RAG-response
        parsing via `RagClientTest`) has unit coverage.

- **How to Test** (human-in-the-loop on device + remote where possible):

  Setup (remote): backend + worker + `hushai-rag` + Ollama all up; `adb reverse tcp:8080 tcp:8080`
  (ingest) and `adb reverse tcp:8090 tcp:8090` (rag); install the app; ensure there's real
  transcribed history in the DB to answer from (let capture + worker run first, or reuse the
  existing `transcript_sentences` rows).

  1. *(Real-world, remote — proves the Q&A backend on its own)* From the dev machine:
     `curl -s -XPOST localhost:8090/v1/rag/query -H 'content-type: application/json' -d '{"query":"<something you actually said recently>"}'`
     → a JSON `answer` grounded in your recordings with a non-empty `sources` array. This
     confirms the RAG half end-to-end *before* any voice wiring.
  2. *(I'll prompt you)* In the app, set the wake word (e.g. "Hushai") and complete owner
     **enrollment** (record the sample phrases). Confirm the keyword persists after an app
     restart.
  3. *(I'll prompt you — happy path)* Screen on: say the wake word, then ask a question you know
     is answerable from your recorded history ("what did I say about X?"). **Observable real
     result:** the app **audibly speaks back** a correct, grounded answer within a few seconds.
     I'll watch `adb logcat` to confirm the state machine fired (wake → owner-verified → STT text
     → RAG call → TTS) and that `/v1/rag/query` was hit on the backend side.
  4. *(I'll prompt you — negative / owner-ID)* Have a **different person** say the same wake word
     and ask a question. **Observable real result:** the assistant does **not** answer. I'll
     confirm via logcat that speaker verification failed and **no RAG call** was made.
  5. *(I'll prompt you — screen off)* Lock the screen, say the wake word, ask a question.
     **Observable real result:** it still wakes, verifies, and answers aloud (the foreground
     service keeps listening). Meanwhile I'll confirm via `adb logcat -s HUSHAI_TX` that normal
     2 s segment capture/upload kept flowing the whole time (the shared-mic refactor didn't break
     capture).
  6. *(Supporting)* `./gradlew :app:test` covers keyword matching, the speaker-similarity
     accept/reject threshold, and RAG-response JSON parsing. These back up the real device run;
     they don't replace it.

  I'll drive every backend/adb/logcat step myself and prompt you only for the on-device speaking
  actions; the spoken-answer correctness and the owner-vs-stranger distinction need your ears, so
  I'll ask you to confirm those specifically.
