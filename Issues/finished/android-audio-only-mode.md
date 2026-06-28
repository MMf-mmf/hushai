**Title:** `[hushai-android] - Audio-only capture mode (skip video, mic-only) + headless-stop reliability fix`

> **✅ DONE & VERIFIED (2026-06-25)** — implemented and verified end-to-end on the physical
> Galaxy S8 (SM-G950U, Android 9) against the live backend, driven over `adb` (`HUSHAI_TX`
> logcat + Postgres row inspection). All acceptance criteria below are checked. Key facts:
> - Audio-only is a **pre-Start mode toggle**: when on, the camera is never opened and the
>   H.264 encoder never runs — only the `cam0-audio` AAC stream is produced/uploaded. The
>   audio path (mic → AAC → upload, plus the voice assistant) is **byte-for-byte unchanged**;
>   the session simply never produces a `cam0-video` stream. **No wire-contract change.**
> - The foreground-service type **narrows to `microphone`** in audio-only
>   (`CaptureService.foregroundServiceType(audioOnly)`), a subset of the manifest's declared
>   `camera|microphone`, so it neither needs nor claims the camera FGS type/permission.
> - **No `CAMERA` runtime permission** is requested or gated on in audio-only; the camera
>   `uses-feature` is now `required="false"` (app stays installable on camera-less devices).
> - **Redundant-start guard:** a second start intent with a flipped flag must not re-declare a
>   FGS type that contradicts the live session (e.g. narrow to mic-only while the camera is
>   open). When already running, the live mode is authoritative; mode changes require Stop → Start.
> - **Bonus fix (pre-existing, not part of the feature):** `MainActivity` was the default
>   *standard* launch mode, so `am start … --ez stop true` against a **foregrounded** app was
>   silently dropped (no `onNewIntent`) — making the headless/script stop unreliable. Fixed
>   with `android:launchMode="singleTop"`.
>
> **On-device evidence (audio-only run, `localhost:8080` via `adb reverse`):**
> ```
> HUSHAI_TX: preflight healthz=200 readyz=200 url=http://localhost:8080
> HUSHAI_TX: capture started device=android-019efbfe… audioOnly=true (no video)
> HUSHAI_TX: stream=cam0-audio seq=0 bytes=25336 sha256=9753d532… status=200
> … (cam0-audio seq 1,2,3,… all status=200; NO cam0-video lines) …
> [summary] accepted (status=200) per stream during the run:  13 cam0-audio
> ```
> Postgres delta for the audio-only session (rows since a baseline taken just before Start):
> ```
>  stream_id  | media_type | segs | min_seq | max_seq | sessions
> ------------+------------+------+---------+---------+----------
>  cam0-audio |          1 |   28 |       0 |      27 |        1     (zero cam0-video rows)
> ```
> The run script granted **no** `CAMERA` permission yet capture succeeded — confirming the
> camera is never opened. Graceful-stop fix verified: plain `--ez stop true` now logs
> `stopping capture — finalizing in-flight segments` → `capture stopped; 0 segment(s) still
> buffered` and capture halts (DB row count goes stable).

- **Description**:

  Add an **"Audio only"** capture mode to the verified `hushai-android` client
  (`Issues/initial-android-app.md`, `Issues/android-preview-and-battery-saver.md`). Sometimes
  the video isn't needed; capturing it anyway wastes **storage, upload bandwidth, and battery
  / processing time**. When audio-only is enabled, the app records and uploads **only the
  `cam0-audio` stream** — the camera is never opened, the video encoder never runs, and no
  `cam0-video` stream exists for the session.

  Client-only change — **no change to the wire contract** (`contracts/cameraToBackendContract.md`
  v0.1.0: endpoint, segment shape, auth, per-stream monotonic `sequence` from 0 all unchanged).
  An audio-only session is just a session that never opens the `cam0-video` stream, which the
  backend and worker already handle (audio is the transcription path; only *video-only*
  segments are the known server-side problem case — not this one). No `409` risk: `cam0-audio`
  still starts at `sequence=0` and increments monotonically per `(session, stream)`.

- **Acceptance criteria** (all met):

  1. ✅ A pre-Start **"Audio only"** toggle in the UI, **disabled while capturing** (changing
     mode mid-session would mean opening/closing the camera). Persisted across launches.
  2. ✅ When audio-only: **no camera opened**, **no H.264 encoder**, **only `cam0-audio`**
     uploaded; all uploads still honor the backend state machine (`200` delete, etc.).
  3. ✅ Foreground-service type **narrows to `microphone`** in audio-only (valid subset of the
     manifest-declared `camera|microphone`).
  4. ✅ **No `CAMERA` permission required** in audio-only (neither requested nor gated on); the
     camera `uses-feature` is `required="false"`.
  5. ✅ **No camera preview** rendered in audio-only (a "🎙 Audio-only — video disabled"
     placeholder instead); status card shows `Mode: audio only` and `cam0-video seq: off`.
  6. ✅ Headless control: `am start … --ez audio_only true` and `run_hushai_app.sh --audio-only`
     (which also skips the `CAMERA` grant).
  7. ✅ Audio path (AAC segments + voice assistant) and the existing audio+video mode are
     unchanged.

- **Implementation** (files):

  - `config/Settings.kt` — persisted `audio_only` boolean (`KEY_AUDIO_ONLY`), with
    `audioOnlyBlocking()` / `setAudioOnlyBlocking()` following the existing pattern.
  - `util/Status.kt` — `CaptureStatus.audioOnly` so the UI can render the live mode.
  - `capture/CaptureService.kt`:
    - `foregroundServiceType(audioOnly)` companion helper: `MICROPHONE` alone vs
      `CAMERA|MICROPHONE`.
    - `onStartCommand` resolves the requested mode (intent extra `EXTRA_AUDIO_ONLY`, else
      persisted setting for the `START_STICKY` OS-restart case) **before** `startForeground`.
      A **redundant start while running declares the live mode (`activeAudioOnly`), not the new
      flag**, so it can't narrow the FGS type out from under an open camera.
    - `startCapture(url, token, audioOnly)` skips `CameraController.select`, the `VideoEncoder`,
      and the `CameraController` when audio-only; `video`/`camera` stay null (all teardown is
      null-safe). Publishes `audioOnly` to `StatusBus`; `activeAudioOnly` is set before
      `running` flips true (volatile happens-before).
  - `MainActivity.kt` — `requiredPermissions(audioOnly)` drops `CAMERA`;
    `hasCapturePermissions(audioOnly)` gates only on `RECORD_AUDIO` in audio-only; the flag is
    threaded through the permission round-trip (`PendingStart`) and the headless `audio_only`
    intent extra (persisted on receipt).
  - `ui/CaptureScreen.kt` — the "Audio only" `Switch` (disabled while running), preview gated to
    `running && !audioOnly` with a placeholder otherwise, status card reflects the mode.
  - `AndroidManifest.xml` — camera `uses-feature` → `required="false"`; `MainActivity`
    `launchMode="singleTop"` (the stop-reliability fix).
  - `local_dev/run_hushai_app.sh` — `--audio-only` flag (skips the `CAMERA` grant, passes
    `--ez audio_only true`); portable empty-array expansion under `set -u`.

- **Review**: a 5-dimension adversarial review (FGS/permissions, capture pipeline, Compose
  state, headless script, contract/integration) ran over the diff with per-finding verification.
  Two real findings, both fixed: (1) the redundant-start FGS-type narrowing above; (2) the
  `--help` `sed` range over-ran into shell code. Nothing else surfaced.

- **How to test** (real, end-to-end over `adb` against the live backend):

  ```bash
  # 0. Backend up + reachable from the USB phone (S8 = Android 9, USB adb):
  cd hushai-backend && SQLX_OFFLINE=true cargo run &     # :8080 (Postgres must be up)
  $ANDROID_HOME/platform-tools/adb reverse tcp:8080 tcp:8080

  # 1. Audio-only session for 30s (skips the CAMERA grant; points at localhost via reverse):
  local_dev/run_hushai_app.sh --audio-only --url http://localhost:8080 --duration 30

  # PASS: logcat shows `audioOnly=true (no video)` and ONLY `stream=cam0-audio … status=200`
  #       (no cam0-video). Summary prints just "N cam0-audio".

  # 2. Confirm in Postgres that the session produced audio only:
  psql "$DATABASE_URL" -c "WITH s AS (SELECT session_id FROM segments ORDER BY received_at DESC LIMIT 1) \
    SELECT g.stream_id, g.media_type, count(*) FROM segments g JOIN s USING(session_id) GROUP BY 1,2;"
  # PASS: one row, stream_id=cam0-audio, media_type=1 (AUDIO). No cam0-video row.

  # 3. Graceful stop (validates the singleTop fix — works even with the app foregrounded):
  adb shell am start -n com.hushai.android/.MainActivity --ez stop true
  # PASS: logcat shows "stopping capture …" → "capture stopped; 0 segment(s) still buffered";
  #       DB row count goes stable.
  ```

- **Notes / not-in-scope**:
  - Mode is a **per-session choice** (locked while capturing). A live mid-capture toggle
    (start/stop the camera without stopping the session) was deliberately not built.
  - Docs updated in the same change: `hushai-android/README.md` (Feature 5) and `AGENTS.md`.
  - The `singleTop` change benefits the whole headless control surface (url/token/autostart/
    stop), not just audio-only.
