**Title:** `[hushai-android] - App touch-up: collapse debug/config, live camera preview while capturing, and true screen-off battery-saver mode`

> **✅ DONE & VERIFIED (2026-06-24)** — implemented and verified end-to-end on the physical
> Galaxy S8 (SM-G950U, Android 9) against the live backend. All acceptance criteria below are
> checked. Verification was driven over `adb` (build/install, `screencap`, `HUSHAI_TX` logcat,
> `dumpsys media.camera`/`power`/`SurfaceFlinger`, `dpm set-active-admin`) plus a human eyeball
> pass on the preview image and the battery-saver lock. Key implementation notes:
> - Live preview = a 2nd Camera2 output surface (`SurfaceView`) on the same session as the
>   encoder; the camera is owned by `CaptureService` and the Activity binds (`LocalBinder`) to
>   hand it the surface. A **session-generation guard** in `CameraController` makes the
>   last-requested reconfigure win (else a stale encoder-only session could leave the preview
>   black / throw "session has been closed").
> - Cross-thread service fields (`camera`/`video`/`audio`/`uploader`/`wakeLock`/`uploadThread`)
>   are `@Volatile`; preview teardown is identity-guarded so a stale `surfaceDestroyed` can't
>   detach a fresh preview.
> - **Testing gotcha:** a camera-fed `SurfaceView` renders **black in `adb screencap`** (it's a
>   hardware overlay layer) even when the preview is working — confirm it via the camera's
>   output-stream count (2 = encoder+preview) + a live SurfaceFlinger `BufferLayer`, then eyeball.
> - Backend persisted the test run durably (`cam0-video`/`cam0-audio` rows in Postgres,
>   all uploads `status=200`); capture continued with the screen off (segments kept flowing).

- **Description**:

  Make the verified `hushai-android` capture client (`Issues/initial-android-app.md`) usable
  as a real always-on device. Today `app/src/main/kotlin/com/hushai/android/ui/CaptureScreen.kt`
  is a flat debug bench: the **Backend URL** field, **Device token** field, the `device_id`
  line, Start/Stop, and a status **Card** (state, backend reachability, cam0-video seq,
  cam0-audio seq, accepted, buffered, last error) are all permanently on screen. Three changes,
  client-only — **no change to the wire contract** (`contracts/cameraToBackendContract.md`
  v0.1.0: endpoint, segment shape, auth, upload behavior all unchanged):

  1. **Collapse debug/config into one collapsible section.** Move Backend URL, Device token,
     `device_id`, and the entire status card into a single **"Debug / Advanced"** section that
     is **collapsed by default**. The primary screen foregrounds Start/Stop (and, while
     capturing, the live preview). The URL/token fields stay fully editable once expanded; the
     status counters stay live. Files: `CaptureScreen.kt` (Compose only).

  2. **Live camera preview while the app is open and capturing.** When Start is pressed with
     the Activity in the foreground, show a live video feed of **exactly what is being streamed
     to the backend**. Today `CameraController.kt` targets a **single** Camera2 output Surface —
     the video encoder's input surface (wired in `CaptureService.kt`:
     `camera = CameraController(this, selection.cameraId, video!!.inputSurface)`). Add the
     on-screen preview as a **second Camera2 output surface** on the same capture session, so
     the preview renders the same sensor frames the encoder receives (literally "what it's
     streaming"). The camera is owned by `CaptureService`, not the Activity, so:
     - the Activity hosts a `SurfaceView`/`PreviewView` (via `AndroidView` in Compose) and hands
       its `Surface` to the service (bind to the service, or a shared surface holder), and
     - the service **(re)creates the capture session** to add the preview target when the UI
       attaches and to drop it when the UI detaches (Camera2 sessions have a fixed surface set).
     - **The encoder feed is the source of truth and must never be degraded, stalled, or
       interrupted by the preview** — preview is best-effort/secondary. If no preview surface is
       present (app backgrounded, or battery-saver), capture runs exactly as it does today with
       the encoder surface alone.
     Files: `CaptureScreen.kt`, `MainActivity.kt`, `CameraController.kt`, `CaptureService.kt`.

  3. **Battery-saver mode — true screen-off, capture continues.** Add a **"Battery saver"**
     control (shown while capturing). Tapping it **locks the device screen off via
     `DevicePolicyManager.lockNow()`**. This requires a one-time **Device Admin** grant: add a
     `DeviceAdminReceiver`, `res/xml/device_admin.xml`, the manifest registration, and a small
     "enable Device Admin" flow the first time. Capture + upload **continue with the screen
     off** — this already works today because `CaptureService` is a foreground service
     (`camera|microphone`) holding a `PARTIAL_WAKE_LOCK` (`CaptureService.acquireWakeLock()`),
     so the CPU stays awake and segments keep flowing while the display is off. Entering
     battery-saver should also detach the preview surface (stop rendering the preview) so the
     camera isn't driving an off-screen target; the encoder feed is untouched. Files: new
     `DeviceAdminReceiver` + `res/xml/device_admin.xml`, `AndroidManifest.xml`, `CaptureScreen.kt`,
     and `MainActivity.kt`/`CaptureService.kt` as needed.

- **Acceptance Criteria**:
  - [x] On launch, Backend URL, Device token, `device_id`, and the status counters are hidden
        inside a single collapsible section that is **collapsed by default**; Start/Stop are
        usable without expanding it.
  - [x] Expanding the section reveals editable Backend URL + Device token fields and the live
        status card (state, reachability, cam0-video seq, cam0-audio seq, accepted, buffered,
        last error) — same data as today, relocated.
  - [x] Pressing Start while the app is open shows a **live** camera preview on screen that
        updates in real time and reflects the frames being encoded/streamed (back camera, ~720p).
  - [x] While the preview is visible, uploads continue normally: cam0-video/cam0-audio seq
        advance and the backend keeps returning 200 (preview never replaces or stalls the
        encoder feed).
  - [x] A "Battery saver" control is present while capturing. After a one-time Device Admin
        grant, tapping it locks the screen **off**.
  - [x] With the screen off via battery-saver, capture + upload **continue**: new segments keep
        arriving (seq increases) for several minutes with the display dark.
  - [x] Waking the device and reopening the app returns to the live preview + controls, with
        capture having never stopped.
  - [x] Stop still cleanly stops capture and flushes buffered segments (`pending` → 0),
        unchanged from today.
  - [x] No change to `POST /v1/segments` payload/auth; `./gradlew :app:test` (RetryBuffer,
        ManifestRoundTrip, Uploader) still passes and `:app:assembleDebug` is green.

- **How to Test**:

  Most of this is verifiable **remotely over `adb`** (build/install, screenshots, logcat,
  screen-state) — per the user's instruction, do those directly. The genuinely human-only step
  is the **one-time Device Admin grant** (a system dialog tap, like the original USB-debugging
  enable); prompt the user for that and for any final eyeball sanity-check.

  Setup (remote):
  1. Backend: `cd hushai-backend && SQLX_OFFLINE=true cargo run` (cleartext :8080).
  2. Phone over USB: `"$ANDROID_HOME"/platform-tools/adb reverse tcp:8080 tcp:8080`, point app at
     `http://localhost:8080`.
  3. `cd hushai-android && JAVA_HOME=/opt/homebrew/opt/openjdk@17 ANDROID_HOME="$HOME/Library/Android/sdk" ./gradlew :app:installDebug`.

  Real-world checks:
  - **Collapsed default (remote):** Launch the app, then `adb exec-out screencap -p > /tmp/s1.png`
    and read it — confirm Start/Stop visible and **no** URL/token fields until "Debug/Advanced"
    is expanded. Expand it (`adb shell input tap …` or eyeball) and screenshot again → URL, token,
    device_id, counters appear.
  - **Live preview (remote):** Start capture (UI tap or the headless autostart intent). Screenshot
    `adb exec-out screencap -p` → confirm a live camera image is rendered on screen. In parallel,
    `adb logcat -s HUSHAI_TX` shows `stream=cam0-video … status=200` and `stream=cam0-audio …
    status=200` lines with **increasing `seq=`** the whole time the preview is up → preview did not
    stall the stream.
  - **Battery-saver / screen-off (mostly remote; one human tap):** With capture running, tap
    "Battery saver"; **prompt the user to grant Device Admin** the first time (human-only). Then
    verify remotely: `adb shell dumpsys power | grep -E "mWakefulness|Display Power"` reports the
    screen **OFF/asleep**, while `adb logcat -s HUSHAI_TX` keeps emitting `status=200` lines with
    rising `seq=` for 3–5 minutes → footage streams with the display off. (`seq` should rise by
    ≈ elapsed_seconds / 2 per stream, since segments are 2s.)
  - **Wake + resume (remote):** `adb shell input keyevent KEYCODE_WAKEUP`, reopen app, screenshot →
    live preview returns; counters show capture never stopped.
  - **Stop (remote):** Tap Stop → `HUSHAI_TX` stops emitting new `status=200` lines and the
    buffered/`pending` count drains to 0.

  Supporting (remote): `./gradlew :app:assembleDebug` green and `./gradlew :app:test` passes —
  these back up the on-device run; they don't replace it.
