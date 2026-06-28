# Camera → Backend Contract (`cameraToBackendContract.md`)

**Status:** v0.1.0 (draft) · **Project:** Hushai data-intake subsystem (Ahithophel)

> This document is the **single, authoritative interface contract** between any data-capture
> client ("a camera") and the Hushai backend. Both the mobile-app tickets and the backend tickets
> reference this file. If client and backend ever disagree, **this document wins.**

---

## 1. Purpose & scope

The goal is to **decouple the backend from any specific camera**. The backend must never need to
know or care *what kind* of device sent data. A new camera type (today: a native Android app;
tomorrow: a webcam, dashcam, GoPro, RTSP IP cam, file replay) becomes usable by **conforming to
this contract — with zero backend changes.**

**This contract defines ONLY the boundary:** the endpoint a client calls, the message it sends,
the rules it must follow, and the guarantees it can rely on in return.

**This contract deliberately does NOT specify backend internals** — not the database, queue,
storage layout, file formats at rest, AI/processing pipeline, or deployment. Those are free to
change at any time as long as the guarantees in §6 hold. Do not add backend-configuration details
to this document.

**First client:** the native Android app. Camera-specific capture choices (resolution, codec,
framerate) are *declared by the client per §4*, never mandated here — that is exactly what keeps
the backend uncoupled.

---

## 2. Core concepts

| Term | Meaning |
|------|---------|
| **Source / device** | A physical client that captures data (the Android phone, a webcam host, …). Identified by a stable `device_id`. |
| **Stream** | One continuous channel of one media type from a source, e.g. `cam0-video`, `cam0-audio`. A source may emit several streams at once. Identified by `stream_id`. |
| **Session** | One capture run (e.g. since app/process start). Identified by `session_id`. `sequence` numbering restarts per session. |
| **Segment** | A short, **immutable**, self-contained slice of one stream (target ~2 seconds). The unit of upload. Each carries one `SegmentManifest` + an opaque media body. |

A source's continuous audio/video is therefore delivered as an ordered run of immutable segments
per stream. The backend reassembles a timeline from the segment metadata, **not** from upload order.

---

## 3. Transport & endpoint

- **Protocol:** HTTPS (HTTP over TLS). LAN-local; no public-internet route.
- **Auth:** `Authorization: Bearer <device-token>` header on every request. Tokens are
  provisioned to a device out of band; how the backend issues/verifies them is out of scope here.
- **Upload one segment:**

  ```
  POST /v1/segments
  Authorization: Bearer <device-token>
  Content-Type: multipart/form-data; boundary=...

    part "manifest"  →  serialized hushai.v1.SegmentManifest  (protobuf bytes)
    part "body"      →  the opaque, codec-tagged media bytes for this segment
  ```

That is the entire required surface. A client that can `POST` this is a conforming source.
(Service discovery — how a client learns the backend's host/port and obtains its token — is an
operational concern outside this contract; the operational runbook is
[`docs/onboarding-a-camera.md`](../docs/onboarding-a-camera.md).)

> **Implementation note (informative, non-normative).** As of 2026-06-28 the backend terminates
> rustls TLS natively when `TLS_CERT_PATH`/`TLS_KEY_PATH` are configured (the LAN uses a self-signed
> CA with IP SANs — see `local_dev/gen_certs.sh`; clients trust the CA). Tokens may be issued
> per-device via `DEVICE_TOKENS` (`label:token,…`) so one device can be revoked individually. None of
> this changes the contract surface — it's how the §3 HTTPS + Bearer requirements are met today.

---

## 4. The message: `hushai.v1.SegmentManifest` ("the proto call")

The manifest is the canonical, **source-agnostic envelope**. It is defined once in Protocol
Buffers (proto3) and is the shared source of truth that both the client and backend compile
against (Rust via `prost`, Kotlin/Android via Square **Wire**). **Media bytes never go inside
protobuf** — they travel as the opaque `body` part; the manifest only describes them.

```proto
syntax = "proto3";
package hushai.v1;

enum MediaType { MEDIA_TYPE_UNSPECIFIED = 0; AUDIO = 1; VIDEO = 2; MUXED = 3; }

message SegmentManifest {
  // --- Identity & ordering ---
  bytes  segment_id  = 1;  // 16-byte UUIDv7. GLOBAL idempotency key. Minted ONCE; reused on every retry.
  string device_id   = 2;  // stable per physical client
  string stream_id   = 3;  // e.g. "cam0-video", "cam0-audio"
  bytes  session_id  = 4;  // 16-byte UUIDv7. New per capture run/process start.
  uint64 sequence    = 5;  // monotonic per (stream_id, session_id), starting at 0. Enables gap detection.

  // --- Source tag (DESCRIPTIVE ONLY) ---
  string source_kind = 6;  // free text, e.g. "android_app", "usb_webcam". The backend MUST NOT branch on this. (§7)

  // --- Media descriptor (client DECLARES what it sent) ---
  MediaType media_type      = 7;
  string    codec           = 8;  // e.g. "h264", "aac", "opus", "mjpeg", "pcm_s16le"
  string    container       = 9;  // e.g. "fmp4", "none", "raw"
  bytes     codec_init_data = 10; // decoder init (SPS/PPS / extradata) so the body decodes standalone; empty if N/A

  // --- Timing (RAW device clocks, uncorrected — see §5) ---
  fixed64 capture_start_unix_nanos = 11; // device WALL clock at segment start (UTC ns)
  uint64  monotonic_start_nanos    = 12; // device MONOTONIC clock at segment start (e.g. Android elapsedRealtimeNanos)
  uint64  duration_nanos           = 13; // segment length

  // --- Integrity ---
  bytes  content_sha256 = 14; // 32-byte SHA-256 of the EXACT body bytes
  uint64 byte_len       = 15; // length of the body in bytes

  // --- Continuity ---
  bool   gap_before = 16; // true if the client KNOWS data is missing immediately before this segment

  // --- Extension escape hatch ---
  map<string, string> attrs = 17; // optional, free-form: geo, orientation, battery, future fusion keys, … (§8)

  reserved 18 to 40; // typed fields (e.g. promoted fusion fields) may be added here later, additively
}
```

### Body part
The `body` is opaque, codec-tagged bytes — exactly what `codec`/`container`/`codec_init_data`
describe. The backend stores it as-is and does not need to decode it to accept it.

---

## 5. Client obligations (the rules a conforming source MUST follow)

1. **Segment immutability.** A segment's bytes and its `segment_id` never change. Targeted
   duration is ~2 s (recommended 1–6 s); the actual length is reported in `duration_nanos`.
2. **Idempotency.** `segment_id` is a UUIDv7 minted **once** when the segment is first created and
   **reused on every retry**. Re-sending a segment with the same `segment_id` must be safe.
3. **Ordering metadata.** `sequence` increases by 1 per segment within a `(stream_id, session_id)`.
   Upload order is irrelevant; the backend orders by metadata. (Out-of-order upload is allowed.)
4. **Self-contained decoding.** Each segment must be independently decodable given its
   `codec`/`container` + `codec_init_data` (e.g. video segments start on a keyframe).
5. **Raw clocks, uncorrected.** Report device wall **and** monotonic clocks as captured. **Do not**
   pre-correct timestamps to "server time" — the backend owns any clock correction. (This keeps a
   future multi-camera timeline possible without re-uploading anything.)
6. **Integrity.** `content_sha256` and `byte_len` must match the body exactly.
7. **Store-and-forward (no data loss).** If the client cannot reach the backend, it **buffers
   segments locally** and uploads them when connectivity returns. A segment is deleted locally
   **only after** the backend confirms acceptance (§6). On failure, the client re-`POST`s the
   **whole** segment (idempotent by `segment_id`).
8. **Honest gaps.** If the client is ever forced to drop captured data (e.g. local storage full),
   it must set `gap_before = true` on the next surviving segment rather than fail silently.
9. **Capability honesty.** `codec`/`container`/`media_type` must truthfully describe the body. The
   client is free to choose its codecs; it must just declare them accurately.

---

## 6. Backend guarantees (what a client can rely on)

The client interacts with the backend purely through HTTP responses to `POST /v1/segments`:

| Response | Meaning the client can rely on |
|----------|--------------------------------|
| **`200 OK`** | The segment is **durably accepted**. It is safe for the client to delete its local copy. |
| **`200 OK` on a duplicate** | A previously accepted `segment_id` re-sent → acknowledged again. Always safe to retry. |
| **`401 Unauthorized`** | Bad/expired token. Client must re-authenticate; do not drop data. |
| **`422 Unprocessable`** | `content_sha256` / `byte_len` mismatch. Client should re-send the segment. |
| **`429` / `507`** | Backend is temporarily unable to accept (busy / storage pressure). Client **retains** the segment and retries later with backoff. |

**Exactly-once at rest:** because the wire is at-least-once (clients retry) and `segment_id` is a
stable idempotency key, a segment accepted once is never duplicated and never lost. *How* the
backend achieves durability and dedup is out of scope — only the guarantee is contractual.

The backend makes **no** promise about *when* downstream processing happens; acceptance (`200`) is
only a promise that the bytes are safely persisted.

---

## 7. The source-agnostic invariant (the heart of this contract)

- `source_kind` is **descriptive metadata only**. The backend **MUST NOT** branch its behavior on
  it. Any per-source handling the backend needs is keyed off `media_type` + `codec`, never the
  camera identity.
- Therefore **adding a new camera type requires no backend change** — a new source simply produces
  conforming `SegmentManifest` + body. (The backend team should keep an automated check that no
  server logic branches on `source_kind`.)

---

## 8. Versioning & extensibility

- **Package version** (`hushai.v1`) changes only for breaking changes.
- **Within a version:** fields are **append-only**. Never renumber or reuse a tag; `reserved`
  every retired tag. Old clients and new backends (and vice-versa) must interoperate.
- **`attrs` map** is the cheap escape hatch: a source may attach new optional metadata (e.g.
  `geo`, `orientation`, future multi-camera-fusion keys) **without a schema change**. Fields that
  prove broadly useful can later be promoted to typed fields in the `reserved` range.
- **Conformance corpus:** a shared set of golden valid/invalid manifest byte-vectors is maintained
  alongside the `.proto`; both the Rust (`prost`) and Kotlin (`Wire`) builds must decode/encode it
  identically. This is the guard against the two sides silently drifting.

---

## 9. Explicitly out of scope of this contract

To keep the boundary clean and avoid coupling, this document says nothing about — and tickets must
not encode here — any of: the backend's database, message queue, blob/file storage layout,
on-disk formats, transcription/embedding/vision or any AI processing, real-time vs. batch
processing, scaling, multi-camera fusion algorithms, or deployment topology. Multi-camera fusion
support is limited, for now, to carrying **raw dual clocks** (§4 fields 11–12) and optional `attrs`
keys; the fusion pipeline itself is a separate future effort.

---

## 10. Conformance checklist (a client conforms to v0.1.0 if it…)

- [ ] `POST`s segments to `/v1/segments` as `multipart/form-data` with `manifest` + `body` parts, over TLS, with a Bearer token.
- [ ] Sends a valid `hushai.v1.SegmentManifest` (all required identity/timing/integrity fields populated).
- [ ] Mints `segment_id` as a UUIDv7 once and reuses it across retries; increments `sequence` per `(stream_id, session_id)`.
- [ ] Sends immutable, independently-decodable segments with accurate `codec`/`container`/`codec_init_data`.
- [ ] Reports raw wall + monotonic clocks, uncorrected.
- [ ] Sets `content_sha256` + `byte_len` matching the body exactly.
- [ ] Buffers locally when offline and deletes a segment only after a `200`; re-sends whole segments on failure.
- [ ] Sets `gap_before` honestly when data was dropped.
- [ ] Never relies on any backend behavior beyond the responses in §6.

---

*Once this contract is agreed, the mobile-app tickets and the backend-server tickets are written
independently against it — the app team builds a conforming client; the backend team builds a
server that honors §6 and the §7 invariant.*
