**Title:** `[hushai-android + hushai-rag] - Move wake-word + STT + owner speaker-ID to the backend (phone becomes a pure audio/video gateway)`

- **Description**:

  Phase 2 of the "phone does no AI processing" direction. Phase 1 (done, 2026-06-25) moved the
  **spoken reply** to the backend: `hushai-rag POST /v1/tts` synthesizes the answer with Kokoro-82M
  and the phone just plays the WAV (`assistant/AudioPlayer.kt`). But the phone still runs the rest of
  the assistant's AI on-device with **Vosk**: the always-on **wake word**, the question **STT**, and
  the **owner voice-ID** (speaker x-vectors). The stated goal is that the front end is *"just a
  gateway for video capture and audio relay"* — so those three should move server-side too.

  After this change the assistant pipeline is:

  ```
  phone: stream mic PCM ──▶ backend: VAD ▶ wake-word ▶ owner speaker-ID ▶ STT ▶ RAG ▶ /v1/tts
  phone: ◀── play WAV
  ```

  The phone keeps only: capture (camera + mic), audio relay, and playback. It no longer bundles the
  Vosk models or runs `VoiceAssistant`'s recognizer loop.

  **Grounded starting point (what exists today):**
  - `hushai-android` `capture/MicSource.kt` already produces one shared 16 kHz mono PCM stream and
    fans it to `PcmSink`s; `capture/AudioEncoder.kt` (AAC ~2 s) and `assistant/VoiceAssistant.kt` are
    the current sinks. A new "assistant relay" sink would stream PCM to the backend.
  - `capture/AudioEncoder.kt` + `net/Uploader.kt` already show the chunked-audio-upload pattern to
    mirror (but this path needs **low-latency streaming**, not ~2 s segments).
  - `assistant/VoiceAssistant.kt` holds the on-device state machine (wake → verify → STT → RAG →
    speak) + `assistant/VoiceModels.kt` (Vosk) + `assistant/SpeakerMath.kt` (cosine/centroid). These
    move server-side or are deleted on the phone.
  - `hushai-rag` already owns the RAG answer and (Phase 1) `/v1/tts`. It would gain a streaming
    endpoint and host the Vosk/whisper acoustic + speaker models.
  - Owner enrollment (the x-vector centroid) currently persists in the phone's DataStore
    (`config/Settings.kt` `owner_embedding`); it must move to / be keyed by `device_id` server-side.

  **Design sketch (decide at implementation time):**
  - A streaming transport: WebSocket or chunked HTTP from phone → `hushai-rag` carrying 16 kHz PCM
    frames, with the answer audio streamed back. Reuse `device_id` for scoping/auth (bearer as today).
  - Server-side: VAD → wake-word spotting → speaker x-vector verify against the stored owner centroid
    → STT → existing RAG → existing `/v1/tts`. The single-worker / call-`getResult()`-once Vosk
    gotchas from `hushai-android-voice-assistant` carry over to the server.
  - Battery/latency: streaming raw mic audio continuously is more power-hungry than on-device
    wake-word spotting. Consider a lightweight on-device VAD/wake gate that only opens the stream when
    speech (or a rough wake) is detected — i.e. keep a *tiny* gate on the phone but move the real
    recognition server-side. Quantify battery impact before committing to always-streaming.
  - Relationship to `Issues/unfinished/speaker-identity-sentiment-rag-attribution.md` and
    `Issues/unfinished/rolling-window-asr-conversation-grouping.md`: that work already pulls
    speaker-ID / ASR server-side for the *recorded* stream; this ticket reuses the same server-side
    models for the *live assistant* path. Coordinate so the speaker x-vector + ASR provisioning is
    shared, not duplicated (see `hushai-conversation-analysis-roadmap`).

- **Acceptance Criteria**:
  - The phone no longer bundles Vosk models or runs an on-device recognizer for the assistant; the
    `assistant/` recognizer/speaker code is removed or reduced to a thin (optional) VAD/wake gate.
  - Wake-word detection, question STT, and owner speaker-ID all run in the backend.
  - Owner enrollment is stored/identified server-side, keyed by `device_id`; existing enrolled users
    keep working (migrate the centroid, or re-enroll flow).
  - End-to-end latency and behavior are at least as good as Phase 1 (wake → spoken answer); a
    non-owner is still ignored.
  - Battery impact of the live audio path is measured and documented; if always-streaming is too
    costly, a minimal on-device gate is implemented and justified.
  - APK size drops (no ~40 MB+ Vosk assets).

- **How to Test** (real, end-to-end on the physical Galaxy S8):
  1. Build + run the backend stack (`ollama serve`, Postgres, `cargo run -p hushai-rag`); `adb
     reverse` the streaming + RAG ports.
  2. Build/install the APK, enable the assistant, enroll the owner (now server-side).
  3. Speak `"computer, <a question answerable from history>"` into the phone in the owner's voice →
     confirm the wake fires, the question is transcribed (server logs), the answer streams back, and
     it is **spoken in the Kokoro voice** through the phone.
  4. Have a **different person** say the wake word → confirm it is ignored (owner gate works
     server-side).
  5. Confirm the phone has no Vosk models packaged (`unzip -l app-debug.apk | grep -i vosk` empty)
     and APK is smaller than the Phase-1 build.
  6. Measure battery drain over a fixed window with the assistant on vs. off; record the delta.
