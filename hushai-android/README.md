# hushai-android

The first **native Android capture client** for Project Hushai: an always-on
camera + microphone app that continuously captures audio/video, slices it into
conforming **segments**, and `POST`s them to the Hushai backend — the client half
of the intake system (`../hushai-backend`). The wire boundary is fixed by
`../contracts/cameraToBackendContract.md` (v0.1.0); that document wins.

**Status: built and verified end-to-end on a physical Galaxy S8 (Android 9), 2026-06-24.**
Live camera+mic → two gapless streams → all POSTs `200` → durable rows + sha-matched
blobs in the backend; a single video segment `ffprobe`-decodes standalone.

## What it does

- **Camera2 + two `MediaCodec` encoders** (H.264 video, AAC audio), each into its
  own per-segment MP4 via a fresh `MediaMuxer`.
- **~2 s, independently-decodable segments** (contract §5.1/§5.4): video cuts only
  on a real `BUFFER_FLAG_KEY_FRAME` (never mid-GOP); each segment carries its CSD
  (SPS/PPS, AudioSpecificConfig) so it decodes standalone.
- **Two separate streams**: `cam0-video` (H.264) and `cam0-audio` (AAC), each with
  its own monotonic `sequence` from 0 per `(stream, session)`.
- **Square Wire** compiles the *identical* `hushai.v1.SegmentManifest` from the
  shared `../hushai-backend/proto` (contract §8 anti-drift). `segment_id`/
  `session_id` are 16 raw bytes; `content_sha256` 32 bytes.
- **OkHttp uploader** posts `multipart/form-data` (`manifest` + `body`,
  `Authorization: Bearer <token>`) and honors the backend's full response state
  machine — `200` delete; `400/409/413` quarantine (no blind retry); `401` retain +
  re-auth; `422` re-send same `segment_id` (bounded); `429/507/other` retain +
  backoff. **Never deletes a local copy on a non-`200`.**
- **Always-on `CaptureService`** — foreground service typed `camera|microphone`,
  `START_STICKY`, partial wakelock; survives screen-off, backgrounding, Activity
  death. Stops only on explicit Stop (notification action or `stop` intent).
- **Bounded retry buffer** (in-memory + file spill): a segment leaves only on `200`
  or quarantine; on overflow it drops the oldest and sets `gap_before=true` on the
  next surviving segment of that stream (honest gaps, §5.8).
- **Compose settings UI** + **headless Intent-extra control** (`url`/`token`/
  `autostart`/`stop`) so the automation script drives it without UI taps.

## Source map

```
app/src/main/kotlin/com/hushai/android/
  MainActivity.kt              UI host, runtime permissions, Intent-extra (head­less) control
  ui/CaptureScreen.kt          Compose settings screen + live status
  capture/
    CaptureService.kt          orchestrator: camera + encoders + uploader loop + retry buffer + wakelock
    CameraController.kt        Camera2 open + session feeding the video encoder surface
    VideoEncoder.kt            H.264 MediaCodec, keyframe-aligned ~2s segment cutting, CSD capture
    AudioEncoder.kt            AudioRecord + AAC MediaCodec, ~2s segment cutting, CSD capture
    SegmentMuxer.kt            one MediaMuxer per segment -> standalone MP4 + sha/byte_len + Segment
    Segment.kt                 immutable finalized-segment model (maps 1:1 to the manifest)
    SegmentManifestBuilder.kt  Segment -> hushai.v1.SegmentManifest wire bytes (Wire)
    RetryBuffer.kt             bounded store-and-forward queue + gap_before + quarantine
    CaptureNotification.kt     persistent FGS notification + Stop action
  net/
    Uploader.kt                OkHttp multipart POST + response->action classification
    UploadOutcome.kt           sealed result type for the response state machine
    Reachability.kt            preflight GET /healthz + /readyz
    Http.kt                    shared OkHttp clients (upload vs fast probe)
  config/{Settings,DeviceIdentity}.kt   DataStore url/token/device_id; per-run session_id
  util/{Uuid7,Sha,Status,HushaiLog}.kt  UUIDv7 (16 bytes), SHA-256, status bus, HUSHAI_TX logging
app/src/test/kotlin/...         JVM unit tests (manifest round-trip, uploader state machine, retry buffer)
app/src/debug/                  cleartext network_security_config (DEBUG ONLY) + manifest overlay
```

## Build

Toolchain on this Mac is non-standard (see the memory `hushai-android-build-env`):

```bash
export JAVA_HOME="/opt/homebrew/opt/openjdk@17"      # keg-only; system java is a broken stub
export ANDROID_HOME="$HOME/Library/Android/sdk"      # adb at $ANDROID_HOME/platform-tools/adb (NOT on PATH)
./gradlew :app:assembleDebug                          # APK -> app/build/outputs/apk/debug/app-debug.apk
./gradlew :app:testDebugUnitTest                      # 8 JVM unit tests
```

Pinned: Gradle **8.9** (wrapper), AGP **8.7.3**, Kotlin **2.0.21**, Square Wire
**4.9.9**, OkHttp 4.12, Compose BOM 2024.09.03. `minSdk 26`, `compileSdk 35`.
Do **not** build with the system `brew` Gradle (9.x — too new for AGP 8.7.3).

## Run it against the backend (one command)

```bash
# 1. backend up (separate terminal): cd ../hushai-backend && SQLX_OFFLINE=true cargo run   # :8080
# 2. device connected (see below), then:
./../local_dev/run_hushai_app.sh --url http://localhost:8080 --token dev-secret-token --duration 120
```

`run_hushai_app.sh` finds adb, builds, `install -r -g`, grants runtime perms,
autostarts via Intent extras, tails `HUSHAI_TX` logcat, prints a per-stream summary
(and backend row counts if `DATABASE_URL` is set), then sends a clean stop.
`--stop` stops a running capture; `--no-build` skips the rebuild.

### Connecting a phone (important gotchas)

- **USB only for the Galaxy S8** — it's **Android 9**, so `adb pair` (wireless
  pairing) does **not** exist (that's Android 11+). Enable **Developer options →
  USB debugging**, plug in, tap **Allow**. `adb devices` shows the device only once
  USB debugging is truly on (it shows nothing — not even `unauthorized` — when off).
- Because the phone is USB-connected, the most robust networking is
  **`adb reverse tcp:8080 tcp:8080`** + app URL `http://localhost:8080` (avoids
  Wi-Fi/firewall issues). For a real LAN test instead, use the Mac's LAN IP, e.g.
  `--url http://192.168.x.x:8080`.
- The backend serves **cleartext HTTP** (no TLS yet); debug builds permit cleartext
  via `src/debug/res/xml/network_security_config.xml`.

### View a capture

`../local_dev/export_capture.sh [cam0-video|cam0-audio] [session-prefix]` reassembles
a session's segments (ordered by `sequence`) into one playable file under
`../local_dev/captures/`.

## Done vs. fast-follows

**Done (v1 scope):** full wire-conforming live path, two-stream capture, bounded
retry buffer, always-on FGS, automation script, JVM unit tests, real-device E2E.

**Noted fast-follows (out of scope, per the ticket):**
- Crash-durable / reboot-surviving on-disk retry queue (Room/WAL) — current buffer
  is in-memory + single-file spill, not durable across process death.
- TLS (the backend serves cleartext today; client switches to `https://` when a
  terminator lands — URL change only).
- Doze / OEM battery hardening for multi-hour runs.
- The shared-proto golden-vector cross-check (contract §8) — separate CI ticket.
- The multi-agent adversarial review stalled and was not completed; a few
  review-driven fixes did land (MediaMuxer buffer position/limit, atomic status
  updates, B-frame `KEY_LATENCY` hint).
