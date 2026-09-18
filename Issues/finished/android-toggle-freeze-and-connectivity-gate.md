**Title:** `[hushai-android] - Fix capture-toggle UI freeze (off-main-thread teardown) + hard-gate Start on backend connectivity`

> **✅ IMPLEMENTED & DEVICE-VERIFIED (2026-06-26)** — both fixes are coded and exercised
> end-to-end on the physical Galaxy S8 (SM-G950U, Android 9) over `adb`. **NOT yet committed**
> (working-tree only) and a couple of follow-ups remain — see **What's left** at the bottom.
>
> **What we did, in one line each:**
> - **Freeze:** capture start/stop teardown was running on the **main thread** (`onStartCommand`
>   → `stopCapture()`), blocking the UI for up to ~20s (worst when the backend was unreachable
>   and the 10s buffer-flush couldn't drain). Moved all transitions onto a single
>   `hushai-lifecycle` executor; the UI now flips instantly and teardown runs in the background.
> - **Gate:** flipping the master switch ON now probes the backend first and **refuses to start
>   (any mode) when it isn't healthy**, showing a dismiss-only "Not connected" dialog. Previously
>   you could start capture with no backend — it would only buffer locally and uploads would hang.
>
> **On-device evidence (Galaxy S8, unreachable backend on purpose):**
> ```
> # Gate (backend down) — tap master switch ON:
> UI: "Checking connection…" → AlertDialog "Not connected" / "Can't reach the backend at … OK"
>     master switch stayed checked="false"; NO foreground capture; NO HUSHAI_TX lines.
>
> # Freeze worst-case — capturing against unreachable backend, tap master switch OFF:
> UI master state == "Stopped" IMMEDIATELY after the tap (dumped before teardown finished)
> HUSHAI_TX (TID 25096 = hushai-lifecycle bg thread, NOT main TID 25069):
>   22:51:36.350  stopping capture — finalizing in-flight segments
>   22:51:41.966  capture stopped; 23 segment(s) still buffered      ← ~5.2s, OFF the main thread
> dumpsys: startRequested=false ; ANR count: 0
>
> # Multi-cycle: start → stop → start → stop — executor reused (same TID 25096), no wedge, clean each time.
> ```

- **Description**

  Two problems with the master on/off control in the verified `hushai-android` client, reported
  together (`Issues/finished/android-audio-only-mode.md`, `…/android-preview-and-battery-saver.md`):

  1. **Toggling capture on/off froze the app.** `CaptureService.onStartCommand(ACTION_STOP)` runs
     on the **main thread** and called `stopCapture()` synchronously. `stopCapture()` blocks on
     four sequential `Thread.join(2_000)` teardowns (camera/mic/video/audio/assistant, ~8s), a 10s
     `Thread.sleep` buffer-flush loop, and a 2s `uploadThread.join()` — up to ~20s of frozen UI
     (MainActivity + CaptureService share one process/main thread). `onDestroy()` had the same
     blocking call (ANR risk). The flush always burned its full timeout when the backend was
     **unreachable** (uploader hung on `Http.upload`'s 120s `callTimeout`, buffer never drained) —
     which is why the freeze felt tied to connectivity.
  2. **Capture could start with no backend connection.** Reachability was only probed *after* start
     (the in-service preflight); there was no pre-Start gate and the app had zero dialogs.

  Client-only change; **no wire-contract change**. The audio/video upload path is untouched —
  only the service lifecycle threading and a pre-Start UI gate were added.

- **Decisions (confirmed with the user)**
  - **Hard block**, not a soft "Start anyway" — capture cannot start while disconnected.
  - Applies to **all modes** (audio+video AND audio-only), since an unreachable backend only
    buffers locally either way.
  - "Connected" = **`/healthz == 200`** (`Reachability.Health.live`). Not `reachable` alone (a
    captive portal / wrong host can fake an HTTP response); not `live && ready` (`/readyz` 503s on
    transient DB/disk issues that self-heal and the uploader retries through). `ready`/`reachable`
    are used only to tailor the dialog message, not to block.

- **Acceptance criteria** (status this session)
  1. ✅ Stopping capture no longer freezes the UI — the switch flips to "Stopped" instantly; the
     join/flush teardown runs off the main thread (verified: teardown on TID 25096, not main).
  2. ✅ Start/stop transitions are race-free across rapid toggles and `onDestroy` (single-thread
     `lifecycle` executor + `desiredRunning` reconcile; `stopSelf(startId)`).
  3. ✅ `onDestroy` no longer does an unbounded main-thread teardown — bounded to ≤8s, only on
     system-initiated destroy.
  4. ✅ Flipping ON probes the backend first ("Checking connection…"), and **does not start** when
     unhealthy — verified for unreachable (dialog shown, no capture, switch stays off).
  5. ✅ Dismiss-only "Not connected" `AlertDialog` with a tailored message (unreachable vs.
     responded-but-unhealthy vs. no URL set), themed to the app's scheme.
  6. ✅ Headless autostart path stays **ungated** (automation/testing).
  7. ⚠️ Gate-**PASS** happy path (healthy backend → Start → `cam0-* status=200`) **not re-run this
     session** against the live backend — see What's left. (The reachable→Start path is the
     unchanged original start; only the pre-Start probe/gate is new.)

- **Implementation** (files — my changes this session)

  - `net/Uploader.kt` — `@Volatile inFlight: Call?` set in `upload()` (try/finally clears it);
    new `cancelInFlight()` aborts a hung `execute()` so a stop can't wait out the 120s timeout. A
    cancelled call throws `IOException` → already classified `RetryLater` → segment stays buffered.
  - `capture/CaptureService.kt`:
    - New single-thread `lifecycle = Executors.newSingleThreadExecutor("hushai-lifecycle")`; a
      `@Volatile desiredRunning` plus `pendingUrl/pendingToken/pendingAudioOnly` captured on the
      main thread.
    - `onStartCommand` no longer blocks: **STOP** sets `desiredRunning=false`, re-asserts
      `startForeground` only when already running (avoids a typed-FGS `SecurityException` on a
      bound-only instance), publishes `running=false` to `StatusBus` for instant UI, calls
      `uploader.cancelInFlight()`, submits `reconcile(startId)`, returns `START_NOT_STICKY`.
      **START** sets `desiredRunning=true` + pending params, `startForegroundTyped`, submits
      `reconcile(startId)`, returns `START_STICKY`.
    - New `reconcile(startId)` (lifecycle thread only): drives actual capture toward
      `desiredRunning`. Start when stopped; re-assert `StatusBus.running=true` on a redundant
      start (covers quick OFF→ON we never tore down); else `stopCapture()` then
      `stopForeground`/`stopSelf(startId)` only if still not wanted (no foreground stripped from a
      live session; `stopSelf(startId)` no-ops if a newer start arrived).
    - `startCapture(...)` now runs on the lifecycle thread (removed the ad-hoc `hushai-start`
      thread); body otherwise unchanged.
    - `stopCapture()` — `cancelInFlight()` up front, the flush `Thread.sleep` loop is now
      interruptible (catch `InterruptedException`/`break`), `uploader = null` after the join.
    - `onDestroy()` — runs teardown on `lifecycle` and joins with `ONDESTROY_JOIN_MS` (8s), then
      `shutdownNow()` (interrupts a still-draining flush). Bounds the only remaining main-thread block.
    - Constants: `FLUSH_TIMEOUT_MS` 10_000 → **3_000**; added `ONDESTROY_JOIN_MS = 8_000`.
  - `MainActivity.kt` — new `onCheckConnection` callback passed to `CaptureScreen`: runs
    `Reachability(Http.probe, url).check()` on `Dispatchers.IO`, seeds `StatusBus`
    (`reachable/live/ready`) so the Debug "Backend" line reflects the pre-Start probe, returns the
    `Health` (keeps OkHttp out of the UI layer).
  - `ui/CaptureScreen.kt` — `onCheckConnection` param; `scope`/`checkingConnection`/
    `showDisconnectedDialog`/`dialogHealth` state; the master toggle now probes before `onStart`
    and only starts when `health.live` (empty URL short-circuits to the dialog with no network
    call; probe exceptions default to an unreachable `Health` and always clear `checkingConnection`);
    `MasterControlCard` gained a `checking` param (a `CircularProgressIndicator` + "Checking
    connection…" + disabled switch while probing); new dismiss-only `DisconnectedDialog`.
  - `hushai-android/README.md` — new "**6. Connectivity gate + non-freezing toggle**" section.
  - `AGENTS.md` / `REVIEW.md` — **not changed**: AGENTS.md only covers the Android client at a
    high level (build/test/phone) and doesn't describe the capture-service internals or start/stop
    flow, so the change doesn't make it inaccurate; REVIEW.md doesn't exist in this repo.

- **How to test** (real, end-to-end over `adb`)

  ```bash
  ADB="$ANDROID_HOME/platform-tools/adb"
  cd hushai-android && JAVA_HOME=/opt/homebrew/opt/openjdk@17 ANDROID_HOME="$HOME/Library/Android/sdk" ./gradlew :app:assembleDebug
  "$ADB" install -r app/build/outputs/apk/debug/app-debug.apk

  # A) GATE (backend down): launch, tap master switch ON.
  "$ADB" shell am force-stop com.hushai.android
  "$ADB" shell pm grant com.hushai.android android.permission.CAMERA
  "$ADB" shell pm grant com.hushai.android android.permission.RECORD_AUDIO
  "$ADB" shell am start -n com.hushai.android/.MainActivity --es url http://localhost:9999
  # tap the master switch (find bounds via `adb shell uiautomator dump`; S8 ≈ 894,468)
  # PASS: "Checking connection…" → "Not connected" dialog; switch stays off; no HUSHAI_TX; no foreground service.

  # B) FREEZE (worst case): autostart against an unreachable backend (bypasses the gate), then stop.
  "$ADB" shell am force-stop com.hushai.android
  "$ADB" shell am start -n com.hushai.android/.MainActivity --es url http://localhost:8080 --ez autostart true
  # …let it buffer a few seconds, then tap the master switch OFF.
  # PASS: UI shows "Stopped" instantly; logcat (threadtime) shows "stopping capture" → "capture stopped"
  #       on a TID != the main thread, several seconds later; `dumpsys` startRequested=false; zero ANR.

  # C) GATE PASS (live backend) — NOT YET RUN this session:
  cd hushai-backend && SQLX_OFFLINE=true cargo run &     # :8080, Postgres up
  "$ADB" reverse tcp:8080 tcp:8080
  # set the app's Backend URL to http://localhost:8080, tap ON.
  # EXPECT: probe passes, capture starts, HUSHAI_TX shows cam0-audio/-video … status=200.
  ```

- **What's left / follow-ups**
  - **Not committed.** All of the above is working-tree only; needs a commit/PR (the diff is
    tangled with a separate concurrent feature — see last bullet — so commit selectively).
  - **Gate-PASS happy path (test C) not re-run** against the live backend this session. Low risk
    (the reachable→Start path is the unchanged original start), but worth one confirming run.
  - **`onDestroy` still blocks the main thread**, now bounded to ≤8s and only on system-initiated
    destroy (Android offers no async `onDestroy`). Accepted tradeoff.
  - **`FLUSH_TIMEOUT_MS` cut 10s → 3s:** at a normal stop the last 1–2 segments may not flush; they
    stay buffered on disk and re-send next session (idempotent by `segment_id`). By design — confirm
    acceptable. (Test runs left 23/44 segments buffered against the dead backend, as expected.)
  - **Device APK was built from my changes BEFORE the concurrent Voices/TTS feature landed** in the
    same files. The combined working tree **compiles** (verified `:app:compileDebugKotlin`), but the
    on-device APK was not rebuilt/reinstalled with the merged code — re-verify after commit.
  - **Concurrent, unrelated work is present in the same files** (NOT part of this task): a
    backend-TTS + speaker-management feature — `ui/VoicesScreen.kt`, `net/SpeakersClient.kt`,
    `net/TtsClient.kt`, `assistant/AudioPlayer.kt`, `assistant/WavPcm.kt` (+ a test), and the
    `onOpenVoices`/Voices-screen nav and `TtsClient` wiring in `MainActivity.kt` / `CaptureScreen.kt`
    / `CaptureService.kt`. Keep these separate when committing.

- **Notes / not-in-scope**
  - A live mid-capture mode change was not built (unchanged from audio-only mode: Stop → flip → Start).
  - Memory updated: `hushai-android-adb-verification` now records the headless-stop method and the
    off-main-thread teardown signature (TID ≠ main).
