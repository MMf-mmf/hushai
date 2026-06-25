# hushai-android

The native Android client for Project Hushai. It started as an **always-on
camera + microphone capture client** (continuously captures A/V, slices it into
conforming **segments**, and `POST`s them to `../hushai-backend`) and is now also
a **hands-free personal voice assistant** — say a wake word, it verifies it's the
owner's voice, transcribes your question, answers it from your *own* recorded
history (via `../hushai-rag`), and speaks the answer back. An "Alexa for your
life-log", fully on-device except the local RAG call.

The capture wire boundary is fixed by `../contracts/cameraToBackendContract.md`
(v0.1.0); that document wins.

**Status (2026-06-25): capture + live preview + battery-saver + voice assistant
all built and verified end-to-end on a physical Galaxy S8 (Android 9).** Owner
enrolled, owner-asked questions answered aloud, a different speaker correctly
ignored; capture/upload unaffected throughout.

---

## Features

### 1. Capture (v1 — the foundation)

- **Camera2 + two `MediaCodec` encoders** (H.264 video, AAC audio), each muxed into
  its own per-segment MP4 via a fresh `MediaMuxer`.
- **~2 s independently-decodable segments** (contract §5.1/§5.4): video cuts only on
  a real `BUFFER_FLAG_KEY_FRAME` (never mid-GOP); each segment carries its CSD
  (SPS/PPS, AudioSpecificConfig) so it decodes standalone.
- **Two streams**: `cam0-video` (H.264) and `cam0-audio` (AAC), each with its own
  monotonic `sequence` from 0 per `(stream, session)`.
- **Square Wire** compiles the *identical* `hushai.v1.SegmentManifest` from the
  shared `../hushai-backend/proto` (contract §8). `segment_id`/`session_id` = 16 raw
  bytes; `content_sha256` = 32 bytes.
- **OkHttp uploader** posts `multipart/form-data` (`manifest` + `body`, bearer auth)
  and honors the backend response state machine — `200` delete; `400/409/413`
  quarantine; `401` retain + re-auth; `422` re-send same id (bounded); `429/507/other`
  retain + backoff. **Never deletes a local copy on a non-`200`.**
- **Always-on `CaptureService`** — foreground service typed `camera|microphone`,
  `START_STICKY`, partial wakelock; survives screen-off, backgrounding, Activity death.
- **Bounded retry buffer** (in-memory + file spill); overflow drops oldest and sets
  `gap_before=true` on the next surviving segment of that stream.
- **Compose UI** + **headless Intent-extra control** (`url`/`token`/`autostart`/`stop`).

### 2. Live camera preview (this session)

When the app is open and capturing, a `SurfaceView` shows a live preview of exactly
what's being streamed. It's a **second Camera2 output target** on the same session as
the encoder input surface — the preview renders the same sensor frames the encoder
gets. The camera is owned by `CaptureService`, so the Activity **binds** (`LocalBinder`)
and hands its preview `Surface` to the service; the service (re)creates the capture
session to add/drop the preview target as the Activity binds/unbinds. The encoder feed
is the source of truth and is never torn down by preview changes.

- **Session-generation guard** (`CameraController.configGeneration`): when the session
  is recreated (preview attach/detach), only the latest config "wins" — a stale
  `onConfigured` closes itself, preventing a black preview / "session has been closed".
- ⚠️ **Gotcha:** a camera-fed `SurfaceView` renders **black under `adb screencap`** (it's
  a hardware overlay layer). Verify the preview is live via `dumpsys media.camera`
  (expect **2 output streams**) + a live `SurfaceFlinger` `BufferLayer`, then eyeball.

### 3. Battery-saver mode (this session)

A button (while capturing) that calls `DevicePolicyManager.lockNow()` to turn the
screen off while capture keeps running. Needs a one-time **Device Admin** grant
(`HushaiDeviceAdminReceiver` + `res/xml/device_admin.xml` declaring `force-lock`).
Locking backgrounds the Activity → it unbinds → the preview detaches (camera drops to
1 output stream) → the foreground service + wakelock keep capture + upload flowing with
the display off. (Verified: segments keep uploading `200` with the screen `OFF`.)

### 4. Voice assistant (this session — headline)

```
wake word  →  owner voice-ID  →  question (STT)  →  RAG answer  →  spoken reply
 (Vosk)        (Vosk x-vector)     (Vosk)            (hushai-rag)    (Android TTS)
```

- **Always-on wake word** — a user-configurable keyword (default `"computer"`),
  spotted offline by Vosk; runs inside `CaptureService`, so it works screen-off.
- **Owner voice identification** — on a wake, the speaker is verified against an
  enrolled owner profile (cosine vs a stored x-vector centroid). A different person is
  ignored. Enrollment is a one-time "talk for a few seconds" flow.
- **Question STT** — the utterance after the wake word is transcribed by the same Vosk
  recognizer.
- **RAG answer** — the question is `POST`ed to `../hushai-rag` `/v1/rag/query`, scoped
  to this device's `device_id`, so answers are grounded in the owner's own recordings.
- **Spoken reply** — the answer is read aloud via Android `TextToSpeech`.

All offline/on-device except the RAG call to the local backend — consistent with the
local whisper + Ollama stack. Full spec + acceptance criteria:
`../Issues/voice-assistant-wakeword-rag.md`.

---

## Voice-assistant internals (read this before changing audio/voice code)

### Shared microphone

The mic is owned by **one** `AudioRecord` in `capture/MicSource.kt` (16 kHz mono
PCM16). Its reader thread fans each PCM chunk to a list of `capture/PcmSink`s
**synchronously**, reusing one buffer (sinks must copy before returning). This exists
because **API 26 won't reliably give two `AudioRecord`s concurrent mic data** — so a
single source feeds both consumers:

- `AudioEncoder` (the AAC segment encoder) — now a **push-based `PcmSink`**. Its `onPcm`
  only copies the chunk into a bounded queue; a **dedicated worker thread** does the
  codec feed/drain + segment finalization (which includes SHA-256 file I/O). This keeps
  heavy work **off the mic reader thread**, so the encoder can never stall the assistant
  (and vice-versa).
- `VoiceAssistant` — the other `PcmSink`, added/removed live via `MicSource.addSink/
  removeSink` (a `CopyOnWriteArrayList`) when the assistant is toggled.

**Capture sample rate is now 16 kHz** (was 44.1 kHz) — what Vosk and whisper both want;
the AAC encoder handles it and the worker resamples anyway, so the contract is
unaffected. Teardown order in `CaptureService.stopCapture` is **mic first** (so no more
`onPcm`), then the encoder + assistant.

### The state machine (`assistant/VoiceAssistant.kt`)

`VoiceAssistant.onPcm` only copies PCM into a bounded queue, and **only while
LISTENING / AWAIT_QUESTION / ENROLLING** — it drops audio during THINKING/SPEAKING so
the assistant never transcribes its own TTS. **All Vosk work and every recognizer
mutation happen on a single worker thread**; the recognizer is not thread-safe, so
cross-thread requests (`enroll`, TTS-done) set `@Volatile` flags the worker reads.

Phases: `LISTENING → AWAIT_QUESTION → THINKING → SPEAKING → LISTENING`, plus
`ENROLLING`. Robustness watchdogs (so it can never wedge):

| Guard | Constant | Purpose |
|---|---|---|
| Question wait | `QUESTION_TIMEOUT_NANOS` = 8 s | bare wake word with no follow-up → back to LISTENING |
| Speaking | `SPEAK_TIMEOUT_NANOS` = 30 s | if a TTS callback never fires, force-resume |
| Enrolling | `ENROLL_TIMEOUT_NANOS` = 20 s | finish with whatever voiceprints we have (≥1), else fail |
| Init | try/catch in `init()` | model/recognizer load failure sets `running=false` (no zombie queue) |

Wake detection runs on **final** results (so the `spk` x-vector is available). A single
utterance carrying both wake word + question is handled (verify on it); a bare wake word
moves to AWAIT_QUESTION and verifies on the (longer, better) follow-up.

### Vosk integration (`assistant/VoiceModels.kt`, dep `com.alphacephei:vosk-android:0.3.47`)

- The speaker-model class is **`org.vosk.SpeakerModel`** (not `SpkModel`). Created via
  `Recognizer(model, 16000f, speakerModel)` so `getResult()` includes a 128-dim `spk`
  x-vector.
- 🐞 **The bug that cost a debug cycle:** `Recognizer.getResult()` returns the utterance
  JSON **and resets**. Calling it twice (once for `text`, once for `spk`) makes the 2nd
  read empty → `spk` silently `null` → speaker-ID/enrollment never works while STT/wake
  still do. **Always call `getResult()` once and parse both fields from the same string.**
- Models are bundled as **APK assets** (`app/src/main/assets/vosk/model-en` =
  `vosk-model-small-en-us-0.15`; `model-spk` = `vosk-model-spk-0.4`) and unpacked to
  `filesDir` on first run (guarded by a `.unpacked` marker). They are **gitignored**
  (~82 MB) — run `../local_dev/fetch_vosk_models.sh` after a fresh checkout.

### Speaker verification (`assistant/SpeakerMath.kt` — pure + unit-tested)

- **Enrollment**: collect Vosk `spk` x-vectors from a few seconds of speech → L2-mean
  centroid → persisted as the owner embedding (a CSV string in DataStore).
- **Verify**: cosine(question-utterance x-vector, owner centroid) ≥ `SPEAKER_THRESHOLD`
  (**0.50**). Measured on-device: **owner ≈ 0.63–0.80, stranger ≈ 0.41–0.42** — a clean
  margin around 0.50. If you change the model or see false accepts/rejects, this is the
  knob to tune.
- Not enrolled → "respond to anyone" (with a note). The owner gate only applies once a
  profile exists.

### RAG client + TTS

- `net/RagClient.kt` → `POST {ragUrl}/v1/rag/query` body `{"query":..,"filters":
  {"device_id":..}}`, parses `{answer, sources[…]}`. Uses `Http.rag` (180 s read timeout
  — local LLM generation on CPU is slow). Bearer sent only if a token is set; the dev
  `hushai-rag` runs with no `RAG_TOKEN`, so `RAG_TOKEN = ""` in `CaptureService`.
- TTS = Android `TextToSpeech`; an `UtteranceProgressListener` `onDone` resumes listening.

### Settings + control surface

`config/Settings.kt` (DataStore) adds: `wake_word` (default `computer`), `rag_url`
(default `http://localhost:8090`), `assistant_enabled` (bool), `owner_embedding` (CSV).
The assistant is driven through `CaptureService.LocalBinder`: `setAssistantEnabled`
(live add/remove the sink — runs off the main thread), `setWakeWord`, `enrollOwner`.
UI state is published on `util/AssistantBus` (a `StateFlow`, observed by `CaptureScreen`).
The assistant only runs **while capturing** (that's when the mic is on); enabling it
while idle takes effect on the next Start.

---

## Source map

```
app/src/main/kotlin/com/hushai/android/
  MainActivity.kt              UI host; permissions; service binding (preview + assistant); device-admin lock; headless Intent control
  HushaiDeviceAdminReceiver.kt force-lock device admin (battery-saver lockNow)
  ui/CaptureScreen.kt          Compose UI: Start/Stop, live preview, battery-saver, Voice-assistant card, collapsible Debug
  capture/
    CaptureService.kt          orchestrator: camera + mic + encoders + uploader + retry buffer + wakelock + assistant; LocalBinder
    MicSource.kt               *single* 16kHz AudioRecord; fans PCM to PcmSinks (shared mic)         [new]
    PcmSink.kt                 interface for mic consumers                                           [new]
    CameraController.kt        Camera2 open + (re)configurable session (encoder + optional preview)  [+preview]
    VideoEncoder.kt            H.264 MediaCodec, keyframe-aligned ~2s cutting, CSD capture
    AudioEncoder.kt            push-based PcmSink: own worker thread, AAC ~2s cutting, CSD capture    [refactored]
    SegmentMuxer / Segment / SegmentManifestBuilder / RetryBuffer / CaptureNotification   (capture v1)
  assistant/
    VoiceAssistant.kt          Vosk recognizer + wake/verify/STT/RAG/TTS state machine + enrollment  [new]
    VoiceModels.kt             unpack Vosk assets to filesDir; load Model + SpeakerModel             [new]
    SpeakerMath.kt             cosine / centroid / wake-word token match (pure, unit-tested)         [new]
  net/
    RagClient.kt               OkHttp POST /v1/rag/query -> answer                                   [new]
    Uploader / UploadOutcome / Reachability / Http.kt   (Http.kt adds a long-timeout `rag` client)  [+rag]
  config/{Settings,DeviceIdentity}.kt   DataStore: url/token/device_id + wake_word/rag_url/enabled/owner_embedding
  util/{AssistantBus,Status,HushaiLog,Uuid7,Sha}.kt    status buses; HUSHAI_TX logging; UUIDv7; SHA-256
app/src/main/res/xml/device_admin.xml   force-lock policy
app/src/main/assets/vosk/{model-en,model-spk}/   Vosk models (GITIGNORED; fetch via script)
app/src/test/kotlin/...   JVM unit tests: ManifestRoundTrip, Uploader, RetryBuffer, SpeakerMath, RagClient
```

---

## Build

Toolchain on this Mac is non-standard (see memory `hushai-android-build-env`):

```bash
export JAVA_HOME="/opt/homebrew/opt/openjdk@17"      # keg-only; system java is a broken stub
export ANDROID_HOME="$HOME/Library/Android/sdk"      # adb at $ANDROID_HOME/platform-tools/adb (NOT on PATH)
../local_dev/fetch_vosk_models.sh                     # one-time: pull gitignored Vosk models into assets/
./gradlew :app:assembleDebug                          # APK -> app/build/outputs/apk/debug/app-debug.apk
./gradlew :app:testDebugUnitTest                      # JVM unit tests
```

Pinned: Gradle **8.9** (wrapper), AGP **8.7.3**, Kotlin **2.0.21**, Square Wire **4.9.9**,
OkHttp 4.12, Compose BOM 2024.09.03, **vosk-android 0.3.47**. Tests add `org.json:json`
(Android's bundled `org.json` is a throwing stub under JVM unit tests). `minSdk 26`,
`compileSdk 35`. Do **not** build with the system `brew` Gradle (9.x — too new for AGP).

---

## Run

### Capture only (against the ingest backend)

```bash
# backend up (separate terminal): cd ../hushai-backend && SQLX_OFFLINE=true cargo run   # :8080
../local_dev/run_hushai_app.sh --url http://localhost:8080 --token dev-secret-token --duration 120
```

### Full voice-assistant stack (all local, no egress)

```bash
ollama serve                                                  # :11434 (models mxbai-embed-large + llama3.2:3b)
cd ../hushai-backend && SQLX_OFFLINE=true cargo run           # :8080  ingest
cd ..                && SQLX_OFFLINE=true cargo run -p hushai-rag   # :8090  RAG  (needs transcribed history; run hushai-worker to populate)
adb reverse tcp:8080 tcp:8080 && adb reverse tcp:8090 tcp:8090
```

Then in the app: enable **Voice assistant**, tap **Enroll my voice** and talk ~6–8 s
(→ "enrolled ✓"), then say **"computer, <question>"**. (`../hushai-worker` must have
transcribed some audio into `transcript_sentences` for RAG to have anything to answer.)

> ⚠️ Long-running `cargo run` dev servers get **reaped when idle** — if the assistant
> returns "RAG error", just restart `hushai-rag`. The app stays installed + enrolled.

### Connecting / verifying on device

USB only for the S8 (Android 9 → no `adb pair`). `adb reverse` + `http://localhost:…`
is the most robust networking. Watch the pipeline via `adb logcat -s HUSHAI_TX`
(capture `status=200`; assistant `voice assistant ready`, `enroll: …`, `speaker
cosine=…`). Full adb verification playbook + gotchas: memory `hushai-android-adb-verification`.

---

## Known limitations & future development

- **Wake latency is end-of-utterance bound (~1–2 s)** — Vosk emits the `spk`-bearing
  final result on a brief pause. Lower latency would need partial-result wake detection
  (partials carry no `spk`, so speaker-ID would move to the question utterance only).
- **Wake-word recognition depends on the small ASR model** — common words ("computer")
  are reliable; uncommon/invented words may mis-transcribe. A larger Vosk model or a
  dedicated wake-word engine (Porcupine) is the upgrade path.
- **Enrollment uses a single utterance's x-vector** by default — averaging several
  separated utterances (raise `ENROLL_TARGET`, prompt for pauses) would harden the
  centroid; `SPEAKER_THRESHOLD` (0.50) is the accept/reject knob.
- **RAG is stateless single-turn** — no conversation memory across questions.
- **No TLS; `RAG_TOKEN` unset** — fine for local dev, must change for any network use.
- Capture fast-follows still open: crash-durable on-disk retry queue, Doze/OEM battery
  hardening, the shared-proto golden-vector CI check.
- See `../Issues/` for the broader roadmap (speaker/sentiment attribution, face/emotion,
  rolling-window ASR conversation grouping).
