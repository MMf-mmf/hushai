**Title:** `[hushai-android] - Initial Android capture client: always-on dual-stream (video+audio) segment uploader conforming to cameraToBackendContract v0.1.0`

> **Update (2026-06-23) — the backend is now built and verified.** The companion backend
> ticket `Issues/initial-backend.md` is ✅ **complete and verified end-to-end** (see
> `hushai-backend/README.md`). The connection details this ticket originally left as "TBD
> until the backend is up" are now **concrete and source-verified** (folded into
> *Backend connection details* below). The real backend is now the **primary** real-world
> test target; the previously-proposed `segment_sink.py` stub is demoted to an **optional
> offline fallback**. All values below were verified against `hushai-backend/src/` (not just
> the README).

- **Description**:

  Build the **first native Android app for Project Hushai** (`local_dev/AhithophelPlan.md`,
  "PROJECT Hushai"): an always-on camera + microphone client that continuously captures
  audio/video, slices it into conforming **segments**, and **POSTs them to the local Hushai
  backend** to be ingested. This is the client half of the intake system; its companion is
  the now-built backend `Issues/initial-backend.md` (`[hushai-backend]`). This ticket is "the
  thing that actually connects to the backend and sends the footage" — the real wire path,
  not a mock.

  The wire boundary is fixed by `contracts/cameraToBackendContract.md` (v0.1.0) — that
  document wins on the endpoint, the `SegmentManifest` message, the client obligations (§5),
  and the response guarantees (§6). This ticket defines the **Android-side** internals the
  contract intentionally leaves open (capture pipeline, segmentation, UI, the always-on
  service, the automation/test tooling).

  **The user-visible deliverable:** open the app, enter the local **backend URL** + **token**,
  the app confirms the backend is **reachable**, hit **Start**, and it begins recording and
  streaming **video and audio** to the backend — and **keeps streaming, always-on, until
  explicitly stopped** (survives screen-off, backgrounding, and Activity death via a
  foreground service). A one-command **automation script** must launch and drive all of this
  for us so a human/agent can watch footage flow without tapping through the UI.

  **Where the code lives:** a new top-level **`hushai-android/`** Gradle project (Kotlin,
  `minSdk 26`, Gradle Kotlin DSL), sibling to `hushai-backend/`, `contracts/`, `local_dev/`.
  The new local-dev tooling (`run_hushai_app.sh`, and optionally `segment_sink.py`) lives in
  `local_dev/`.

  ---

  ### Backend connection details (RESOLVED — backend built & verified)

  These are sourced from `hushai-backend/{README.md, src/config.rs, src/routes.rs,
  src/auth.rs, src/error.rs, src/ingest.rs, .env.example}` and the reference client
  `local_dev/feed_segments.py`, and were adversarially re-verified against the source:

  - **Scheme/host/port:** **cleartext `http://<dev-machine-LAN-IP>:8080`**. The server binds
    `BIND_ADDR=0.0.0.0:8080` and serves **plain HTTP — there is no TLS in the backend** (the
    only `tls-rustls` in its `Cargo.toml` is for the *Postgres* connection, not the
    client-facing server). From the **Android emulator**, the host loopback is reached at
    **`http://10.0.2.2:8080`**; from a **physical phone**, use the dev machine's **LAN IP**
    (the bind is `0.0.0.0`, so it's reachable on the LAN). There is **no `8443`/HTTPS** — that
    was a guess in the first draft.
    > Contract §3 mandates HTTPS; the v0.1 backend does not satisfy that yet. TLS will be
    > added later via a reverse proxy / terminator in front of `:8080` — **out of scope here**.
    > For now the client connects over `http://`.
  - **Ingest endpoint:** `POST /v1/segments` — `multipart/form-data`, bearer-auth, body-limited.
  - **Preflight / health:** `GET /healthz` (unauthenticated, **always `200` when the process
    is up** — use this as the canonical reachability probe; **no token needed**) and
    `GET /readyz` (unauthenticated; `200` ready, **`503`** when Postgres is unreachable or the
    blob volume is below its free-space watermark). Probe `/readyz` to distinguish "up but not
    ready" from healthy.
  - **Auth:** `Authorization: Bearer <token>`. The dev token value is **`dev-secret-token`**
    (backend `DEVICE_TOKEN` env var; a **single-token allowlist** in `auth.rs`). The prefix
    match is **case-sensitive `"Bearer "`**; the token is whitespace-trimmed. Missing/empty/wrong
    token → `401`.
  - **Size limits:** request **body ≤ 32 MiB** (`MAX_BODY_BYTES=33554432`; oversized → **`413`**);
    the **`manifest` part is independently capped at 1 MiB** (oversized manifest → **`400`**).
    A ~2 s segment is far under both, but the client must treat these as hard limits.
  - **Shared proto (single source of truth):** `hushai-backend/proto/hushai/v1/segment.proto`
    **exists** and is a **verbatim copy of contract §4** (package `hushai.v1`, all 17 fields,
    `reserved 18 to 40`). Note `capture_start_unix_nanos` is proto **`fixed64`** (Wire → `Long`).
    Point Square Wire at this file (`sourcePath { srcDir("../../hushai-backend/proto") }`).
  - **Reference conforming client to diff against:** `local_dev/feed_segments.py`
    (+ generated `local_dev/segment_pb2.py`) is a **working, verified** Python client. It emits
    a single MUXED stream (`stream_id="<device>-muxed"`, `source_kind="file_replay"`,
    `media_type=MUXED`, `codec="h264+aac"`, `container="fmp4"`), 16-byte UUIDv7 ids,
    `content_sha256`+`byte_len`, and POSTs `manifest`(`application/x-protobuf`) +
    `body`(`application/octet-stream`). The Android manifest/multipart bytes should be
    **byte-diffable** against what this client emits (a cheap anti-drift check short of the
    full §8 golden-vector corpus). Its flags `--bad-token`/`--corrupt-body`/`--conflict`
    exercise `401`/`422`/`422`.

  ---

  **Scope for v1 (confirmed):** build the **full wire-conforming live path** *plus a bounded
  retry buffer*. Crash-durable, reboot-surviving on-disk store-and-forward is a **noted
  fast-follow**, out of scope here (see below). Per-device token issuance is out of band per
  contract §3 and out of scope (the backend uses one dev token today). The shared-proto
  golden-vector cross-check (contract §8) belongs to a separate shared-proto/CI ticket.

  **Capture & segmentation (the core):**
  - **Camera2 + AudioRecord → two `MediaCodec` encoders** (H.264 video, AAC audio), each
    into its **own** small MP4 per segment. (CameraX `Preview` may be used for the preview
    surface, but the encode path is MediaCodec — `VideoCapture`/`MediaRecorder` can't give
    per-~2s, independently-decodable segments with extractable init data.)
  - **~2s, independently-decodable segments (contract §5.1, §5.4):** configure
    `KEY_I_FRAME_INTERVAL` to ~2s and force an IDR at each boundary via
    `setParameters(PARAMETER_KEY_REQUEST_SYNC_FRAME)`, then **cut on the actual
    `BUFFER_FLAG_KEY_FRAME`** — never mid-GOP. Use **one fresh `MediaMuxer` per segment**
    (`MUXER_OUTPUT_MPEG_4`): start on the boundary IDR, write ~2s, `stop()`/`release()`; the
    finalized small MP4 (own `moov`+`mdat`) **is** the segment body, self-contained. Keep each
    body **well under 32 MiB** (the backend's `413` limit).
  - **`codec_init_data`:** capture the encoder's `BUFFER_FLAG_CODEC_CONFIG` buffer (CSD-0/
    CSD-1 = H.264 SPS/PPS, AAC `AudioSpecificConfig`) once per stream and attach it to every
    segment so each body decodes standalone.
  - **Two separate streams** (not MUXED for v1): `cam0-video` (H.264, container declared
    honestly — `mp4`) and `cam0-audio` (AAC). Each has its **own** monotonic `sequence`
    (§5.3, per `(stream_id, session_id)`). Contract §2/§4 allow multiple concurrent streams;
    the backend stores bodies opaquely and **never branches on `media_type`/`source_kind`**
    (verified — §7 CI-guarded), so the two-stream design is accepted with zero backend change.
    The reference feeder uses one MUXED stream; the Android app is the first client to exercise
    two concurrent separate streams (a supported path).
  - **Sequence uniqueness (verified — `409`):** the backend enforces `UNIQUE (session_id,
    stream_id, sequence)`. Reusing that triple under a **different** `segment_id` returns a
    **permanent `409`** (`SequenceConflict` — re-sending cannot help). The client MUST
    guarantee a strictly-monotonic, non-repeating `sequence` per `(stream_id, session_id)`,
    starting at 0, and treat any `409` as a sequence-assignment bug to surface — never a retry.
  - **Timing (contract §5.5 — raw, uncorrected):** at each segment's boundary, snapshot wall
    `System.currentTimeMillis()*1_000_000` → `capture_start_unix_nanos` and monotonic
    `SystemClock.elapsedRealtimeNanos()` → `monotonic_start_nanos`. `duration_nanos` = last −
    first sample PTS. **Do not** NTP-correct or snap to server time.
  - **Integrity (§5.6):** compute `content_sha256` (`MessageDigest`) and `byte_len` over the
    exact finalized body bytes.

  **The proto (single source of truth):** the app compiles the **identical**
  `hushai.v1.SegmentManifest` from `hushai-backend/proto/hushai/v1/segment.proto` via **Square
  Wire**. One `.proto` on disk feeds both prost (Rust) and Wire (Kotlin) — the §8 anti-drift
  guarantee. `segment_id`/`session_id` are **16 raw bytes** (UUIDv7), not the 36-char string.
  `uint64`/`fixed64` proto fields surface as Kotlin `Long` (document the unsigned cast,
  mirroring the backend's `i64` note).

  **Identity:** `device_id` = a stable install ID minted once and persisted (DataStore).
  `session_id` = fresh UUIDv7 per service start; `sequence` restarts at 0 per session.
  `segment_id` = UUIDv7 minted **once** per segment and **reused on every retry** (§5.2).
  `source_kind = "android_app"` (descriptive only — backend never branches on it, §7).

  **Networking (contract §3, §6):** **OkHttp** `MultipartBody` (`FORM`) with part `manifest`
  + part `body`, and an `Authorization: Bearer <token>` interceptor, to
  `POST {baseUrl}/v1/segments`. The backend keys parts **by name only** (`manifest`, `body`)
  and **ignores their Content-Type** — so part-name correctness is what matters. For parity
  with the verified reference client, tag the `manifest` part `application/x-protobuf` and the
  `body` part `application/octet-stream`. On any failure, re-POST the **whole** segment with
  the **same** `segment_id` (idempotent). Out-of-order upload is allowed (§5.3), so the
  uploader may parallelize via a bounded `Dispatcher`.

  **Response handling — the backend's real superset of §6 (verified in `error.rs`).** §6
  enumerates `200/401/422/429/507`; the built server adds `400/409/413` (and `408` on
  request timeout). The client MUST branch on all of them — and **never delete a local copy
  on any non-`200`**:

  | Status | Meaning | Client action |
  |--------|---------|---------------|
  | `200` | Durably accepted (or idempotent re-accept of same `segment_id`+bytes) | Delete local copy |
  | `400` | Malformed request — missing/dup part, undecodable manifest, wrong-length id, oversized manifest | **Client bug.** Do **not** retry blindly; log loudly, quarantine the segment |
  | `401` | Bad/missing/wrong token | Keep data; surface re-auth; do not drop |
  | `409` | `(session_id, stream_id, sequence)` reused by a **new** `segment_id` — **permanent** ordering conflict | **Client bug.** Re-send cannot help; log loudly, quarantine; never loop |
  | `413` | Body exceeds backend `MAX_BODY_BYTES` (32 MiB) | **Permanent for that segment.** Do not retry unchanged; fix encoder/segment size; log loudly |
  | `422` | Integrity mismatch (`content_sha256`/`byte_len`) **or** same `segment_id` re-sent with **different** bytes | Integrity case → re-send same `segment_id`; "different bytes" case is a client bug — log loudly |
  | `429` | Overloaded (in-flight `CONCURRENCY_CAP`=64 exceeded) | Retain; exponential backoff; retry |
  | `507` | Storage pressure / DB pool exhausted | Retain; exponential backoff; retry |
  | _other non-2xx_ (e.g. `408`, `5xx`) | Transient/unknown | Retain; backoff; retry — **never** delete on non-`200` |

  **Always-on (requirement):** a `CaptureService` foreground service typed
  `CAMERA|MICROPHONE` with a persistent "Hushai is capturing" notification (+ Stop action),
  holding the camera session, both encoders/muxers, the uploader, and the retry buffer. The
  Activity is a thin controller; capture lifecycle lives in the service so screen-off,
  backgrounding, and Activity death don't stop it. Partial wakelock to survive screen-off.
  (Doze/OEM-battery hardening for multi-hour runs is a noted fast-follow.)

  **Bounded retry buffer (v1 store-and-forward, §5.7/§5.8):** a bounded in-memory + single-
  file-spill queue with the correct contractual semantics — delete a segment only after
  `200`; re-POST whole on failure; survive a brief outage. If the buffer overflows on a long
  outage, drop oldest and set `gap_before = true` on the next surviving segment (honest gaps,
  §5.8). **Crash-durable / reboot-surviving on-disk queue (Room/WAL) is the named fast-follow,
  explicitly out of scope here.**

  **Settings UI (minimal, Compose):** one screen — backend **URL** field (default
  `http://10.0.2.2:8080` for emulator), **token** field (default `dev-secret-token`),
  **Start/Stop**, and a live status line (reachable/ready + per-stream segment counters).
  Persist URL+token in DataStore. **Debug builds** also read `url`/`token`/`autostart` from
  Intent extras so the script can drive it headlessly.

  **Preflight reachability/health (requirement 1):** before capture starts, validate the URL
  and **`GET {baseUrl}/healthz`** (unauthenticated, returns `200` when up — verified). Treat
  `200` as reachable+live; optionally also `GET /readyz` to warn "backend up but not ready"
  (`503` = Postgres down or disk low). Only a connect/timeout failure is "unreachable". (The
  generic "any HTTP response = reachable" rule remains the fallback for endpoints lacking
  `/healthz`, e.g. the optional offline sink.)

  **Permissions:** manifest `CAMERA`, `RECORD_AUDIO`, `FOREGROUND_SERVICE`,
  `FOREGROUND_SERVICE_CAMERA`, `FOREGROUND_SERVICE_MICROPHONE`, `POST_NOTIFICATIONS`,
  `INTERNET`, `ACCESS_NETWORK_STATE`. Runtime grants (`CAMERA`, `RECORD_AUDIO`,
  `POST_NOTIFICATIONS`) gated before Start; the automation script pre-grants them.

  **Cleartext to the LAN backend (mandatory, not optional):** the verified backend serves
  **cleartext HTTP** on `:8080`, so the app **must** permit cleartext to the LAN. Ship a
  **debug-only** `res/xml/network_security_config.xml` allowing cleartext to the dev subnet
  (and/or `10.0.2.2` for the emulator). TLS is a future backend concern (reverse proxy); when
  it lands, the client switches to `https://` with the proxy's cert — no code change beyond
  the URL.

  **The automation script — `local_dev/run_hushai_app.sh`** (parameterized, idempotent;
  `--url`, `--token`, `--device`, `--no-build`, `--stop`, `--duration`; defaults
  `--url http://10.0.2.2:8080` for emulator / dev-machine LAN IP for a physical phone,
  `--token dev-secret-token`): locate/validate `adb` (it is **not** on PATH in this env —
  search `$ANDROID_HOME`/`~/Library/Android/sdk`); build (`./gradlew :app:assembleDebug`
  unless `--no-build`); `adb install -r -g`; explicitly `adb shell pm grant` the three runtime
  perms; launch + configure + autostart **via Intent extras**
  (`am start -n <pkg>/.MainActivity --es url … --es token … --ez autostart true`) — cleaner
  and less fragile than `adb shell input tap`; stream for `--duration`; let the operator
  **observe footage flowing** via logcat (one structured line per accepted segment, e.g.
  `HUSHAI_TX stream=cam0-video seq=12 bytes=… sha256=… status=200`) and/or the backend's
  `psql` row counts; then send the stop Intent for a clean stop and print a summary.
  Re-pointing at a different backend (or the optional sink) = same script, different
  `--url`/`--token`.

  **(Optional) offline test receiver — `local_dev/segment_sink.py`.** Now that the real
  backend exists and is the primary test target, this stub is **optional** — useful only when
  you want to drive the phone **without** bringing up Postgres + `cargo run` (e.g. on the go).
  If built, it must **match the real backend's verified semantics** so a diff is meaningful:
  stdlib `http.server`; `POST /v1/segments` → parse multipart, decode the `manifest` protobuf,
  recompute SHA-256 + length (mismatch → `422`; missing part → `400`); validate the Bearer
  **value** against an allowlist (wrong/absent → `401`); enforce a 32 MiB body cap (oversized
  → `413`); distinguish `409` (`(session,stream,sequence)` reused by a new `segment_id`) from
  `422` (same `segment_id`, different bytes); dedupe by `segment_id` (repeat → `200`); write
  the body to `out/<stream_id>/<sequence>-<sha8>.<ext>`; expose `GET /healthz` (200) and
  `GET /stats`. If you'd rather not maintain a faithful stub, **skip it** and always test
  against the real backend.

- **Acceptance Criteria**:
  - [ ] New `hushai-android/` Gradle project builds (`./gradlew :app:assembleDebug`) and
        installs/runs on a device or emulator (`minSdk 26`).
  - [ ] App compiles the **identical** `hushai.v1.SegmentManifest` from
        `hushai-backend/proto/hushai/v1/segment.proto` via Square Wire (one `.proto` shared
        with the backend); a round-trip encode/decode test passes and proves `segment_id`/
        `session_id` are emitted as **16 raw bytes**.
  - [ ] The app's emitted `manifest` bytes + multipart layout are **diff-compatible** with the
        verified reference client `local_dev/feed_segments.py` (same field set, part names,
        16-byte ids, sha256/byte_len) — a cheap anti-drift check.
  - [ ] Settings screen accepts a backend **URL** (default `http://10.0.2.2:8080`) + **token**
        (default `dev-secret-token`), persists them, and a **Start/Stop** control runs/stops
        capture; status line shows reachability/readiness + per-stream segment counters.
  - [ ] On Start the app runs a **preflight** `GET /healthz` (→ `200` = reachable+live) and
        reports reachable/unreachable; only a connect/timeout failure is "unreachable".
  - [ ] Capture runs in an **always-on foreground service** (typed CAMERA+MICROPHONE) with a
        persistent notification, and **survives screen-off, backgrounding, and Activity
        death** — stopping only on explicit Stop.
  - [ ] App emits **two streams** — `cam0-video` (H.264) and `cam0-audio` (AAC) — as ~2s,
        **independently-decodable** segments (each starting on a **keyframe** for video) with
        correct `codec_init_data`, each body **< 32 MiB**.
  - [ ] Each segment carries a fully-populated manifest: 16-byte UUIDv7 `segment_id` (minted
        once, reused on retry), 16-byte UUIDv7 `session_id` (per run), stable `device_id`,
        per-`(stream,session)` monotonic `sequence` from 0 (no reuse → no `409`), **raw**
        `capture_start_unix_nanos` + `monotonic_start_nanos` + `duration_nanos`, exact
        `content_sha256` + `byte_len`, truthful `codec`/`container`/`media_type`,
        `source_kind="android_app"`.
  - [ ] App **POSTs** real captured footage to `http://…:8080/v1/segments` as
        `multipart/form-data` (`manifest` protobuf + `body` bytes) with
        `Authorization: Bearer dev-secret-token`; on `200` it deletes its local copy.
  - [ ] App honors the **full** response state machine — `200` accept/delete; `400`/`409`/`413`
        as permanent client-side errors (log + quarantine, no blind retry); `401` keep +
        re-auth; `422` integrity → re-send; `429`/`507`/other-non-2xx → retain + backoff — and
        **never deletes a local copy on a non-`200`**. On retry it re-POSTs the **whole**
        segment with the **same** `segment_id`.
  - [ ] A brief network outage is survived by the **bounded retry buffer**: on reconnect the
        same segments are re-delivered with **no duplicates** and either no gap or an honest
        `gap_before=true`. (Crash-durable on-disk buffer is a noted fast-follow, out of scope.)
  - [ ] **Primary real-world pass:** against the **real backend** (`cargo run` → `:8080` +
        live Postgres), a multi-minute run lands rows in `segments` and content-addressed
        blobs under `{BLOB_DIR}/blobs/ab/cd/<sha256>`, gapless per stream, and an idempotent
        re-run leaves counts unchanged.
  - [ ] `local_dev/run_hushai_app.sh --url … --token … [--device …]` builds, installs, grants
        permissions, injects config via Intent extras, autostarts capture, streams, lets the
        operator observe accepted segments, and cleanly stops — **idempotent and re-runnable**.
  - [ ] Re-pointing at a different backend/terminator needs **only** a different
        `--url`/`--token` — no app or script code change.

- **How to Test**:

  > **Primary path is now the real backend** (it's built and verified). The optional offline
  > sink path is listed last as a fallback.

  1. **(Real-world, mandatory) Bring up the real backend.** Per `hushai-backend/README.md`:
     `createdb hushai && export DATABASE_URL=postgres://localhost/hushai`,
     `(cd hushai-backend && sqlx migrate run)`, then `(cd hushai-backend && cargo run)` — it
     listens on `:8080`. Confirm: `curl -w '%{http_code}\n' localhost:8080/healthz` → `200`
     and `curl -w '%{http_code}\n' localhost:8080/readyz` → `200`.

  2. **(Real-world, mandatory) Launch + drive the real app with one command.** With a device
     or emulator connected, run (emulator example):
     `local_dev/run_hushai_app.sh --url http://10.0.2.2:8080 --token dev-secret-token --duration 120`
     (physical phone: use the dev machine's LAN IP, e.g. `http://192.168.1.50:8080`).
     **Observe:** it builds, installs, grants permissions, autostarts capture via Intent
     extras (no manual tapping), and the app's status line/logcat shows the backend
     **reachable** (`/healthz` `200`) and capture **running**.

  3. **(Real-world, mandatory) Watch real footage flow.** Tail logcat
     (`adb logcat | grep HUSHAI_TX`). **Observe:** for ~2 minutes, both `cam0-video` and
     `cam0-audio` segments POST continuously; per-stream `seq` increments 0,1,2,… ; every
     POST returns `200`.

  4. **(Real-world, mandatory) Verify the bytes really landed in the backend:**
     - Rows present and gapless per stream:
       `psql "$DATABASE_URL" -c "SELECT device_id, stream_id, count(*), max(sequence)+1 FROM segments GROUP BY 1,2;"`
       → `count == max(sequence)+1` for both `…cam0-video` and `…cam0-audio`.
     - Blobs on disk match their digests: each `segments.blob_uri` points at
       `{BLOB_DIR}/blobs/ab/cd/<sha256>`, and `shasum -a 256 <file>` == the row's
       `content_sha256` (mirrors the backend's own verified check).
     - Self-contained media: `ffprobe` a `cam0-video` blob → shows H.264 and decodes
       standalone (proves §5.4 + correct `codec_init_data`).

  5. **(Real-world, mandatory — idempotency / always-on)** Re-run step 2 unchanged (same
     `session_id`/`segment_id`s via the sidecar) → all `200`, **row + blob counts unchanged**
     (exactly-once at rest). Separately, while a run is live, turn the screen off
     (`adb shell input keyevent KEYCODE_POWER`) and background the app → segments keep landing
     in the backend (the foreground service survives).

  6. **(Real-world — outage / idempotency)** Mid-run, drop connectivity for ~10s
     (`adb shell svc wifi disable` then `… enable`). **Observe:** the client retains segments
     (logcat shows non-`200` + retained), then on reconnect re-POSTs the **same** `segment_id`s;
     the backend row count catches up with **no duplicates** and either no gap or an honest
     `gap_before=true` if the buffer overflowed.

  7. **(Real-world — negative paths, mirroring `feed_segments.py`)**
     - Point the app at a **wrong token** → every POST `401`; the client keeps data and
       surfaces re-auth (no data loss). (Backend `auth.rs` validates the token value.)
     - Force a corrupted body (debug toggle) → backend `422`; the client re-sends.
     - Confirm the client **never** produces a `409`: inspect logs over a long run for zero
       `(session,stream,sequence)` reuse. A `409` is a sequence-assignment bug, not a retry.

  8. **(Real-world — clean stop)** Send stop (`run_hushai_app.sh --stop` or the notification
     action). **Observe:** the last in-flight segment finalizes + delivers, the FGS
     notification disappears, no partial/corrupt trailing segment lands, and the backend row
     count stops climbing. Re-running `--stop` is a no-op.

  9. **(Real-world — physical device)** Run steps 2–4 at least once on a **physical phone**
     (the emulator camera is a synthetic scene); confirm real-camera footage segments are
     accepted and `ffprobe`-decodable.

  10. **(Optional — offline fallback, only if `segment_sink.py` is built)** When you can't run
      Postgres, start `python segment_sink.py --out ./sink_out --port 8080`, point the app at
      it (`--url http://10.0.2.2:8080`), and verify the same observable results (N segments
      arrive, sha256 matches, gapless, blobs decode). This stands in for the backend; the real
      backend is authoritative for response semantics.

  11. **(Supporting) Unit/instrumented tests** (`./gradlew test connectedAndroidTest`):
      UUIDv7 emits 16 bytes; manifest round-trips through Wire with correct field numbers and
      is diff-compatible with `feed_segments.py` output; SHA-256/byte_len computed over exact
      body; full response→action mapping incl. `400/409/413`; segment-boundary keyframe logic.
      These back up the real run above — they do not replace it.
