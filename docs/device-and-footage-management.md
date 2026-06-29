# Device & footage management (delete / rename / retention / export)

Built 2026-06-28. The UI surface for managing what's stored: **rename a device**, see **per-device
and per-date storage usage**, set a per-device **retention policy** ("keep last N days") that
auto-purges, **delete footage by date** (one day or several at once), **delete a whole device**, and
**export** a date's footage to MP4 before deleting it.

Until this shipped, hushai only ever grew — capture clients ingested 2 s segments forever, devices
were raw client-assigned ids (`iphone-bob`), and there was no way to reclaim disk or remove a device.

It lands as a new dedicated page (**`🗄 Files`** → `/manage.html`) plus light affordances baked into
the existing viewer dropdown and System-dashboard camera cards.

---

## Where things live

All mutations live in **hushai-backend** — the only writer of `segments`/blobs — mirroring the
existing speaker/person admin surfaces. The **hushai-viewer** reverse-proxies `/v1/devices*` to the
backend (injecting the `DEVICE_TOKEN` bearer server-side, exactly as for `/v1/speakers*` /
`/v1/persons*`). **Export** is a viewer route because the viewer owns ffmpeg + the blob cache.

| Where | What |
|-------|------|
| `hushai-backend/migrations/0011_device_management.sql` | Adds `devices.display_name` + `devices.retention_days` (additive, idempotent). |
| `hushai-backend/src/devices.rs` | All device-management + footage-deletion handlers + the retention sweep. |
| `hushai-backend/src/storage.rs` → `reclaim_blobs()` | Content-addressed, reference-rechecking blob reclamation. |
| `hushai-viewer/src/export.rs` | `GET /api/devices/{id}/export.mp4` — streamed MP4 download. |
| `hushai-viewer/src/proxy.rs` → `is_backend_path()` | Routes `/v1/devices*` to the backend. |
| `hushai-viewer/ui/manage.html` + `ui/js/manage/manage.js` | The Files page. |
| `hushai-viewer/ui/js/confirm.js` | Shared destructive-action confirm dialog (impact + type-to-confirm). |

---

## Backend API (`hushai-backend`, bearer-authed; proxied through the viewer)

Times on the wire are **Unix nanoseconds, UTC**. All delete responses report
`{ segments_deleted, logical_bytes }`.

| Method | Path | What |
|--------|------|------|
| `GET` | `/v1/devices` | Management list: `device_id, display_name, source_kind, segment_count, session_count, logical_bytes, first/last_capture_unix_nanos, retention_days, has_video/audio/muxed`. |
| `GET` | `/v1/devices/{id}/usage?tz=<IANA>` | Per-**local-day** breakdown `[{ day "YYYY-MM-DD", segment_count, logical_bytes, first/last_capture_unix_nanos }]`. Bucketed in the caller's tz; an invalid tz → **400** (not 500). |
| `PATCH` | `/v1/devices/{id}` | Rename: body `{ "display_name": "Front Door" }` (trimmed; empty → 400; unknown id → 404). |
| `PUT` | `/v1/devices/{id}/retention` | Set/clear policy: body `{ "retention_days": N }` (N ≥ 1) or `{ "retention_days": null }` to keep forever. A dedicated endpoint so `null` unambiguously means "clear". |
| `DELETE` | `/v1/devices/{id}/footage?tz=<IANA>&day=YYYY-MM-DD` | Delete one local-day bucket (the same boundaries `usage` reported — no bleed into adjacent days). |
| `POST` | `/v1/devices/{id}/footage/bulk-delete` | Body `{ tz, days:["YYYY-MM-DD", …] }` — delete several days; each is its own committed tx; returns a per-day result array. |
| `DELETE` | `/v1/devices/{id}` | Delete the device **and all its footage** (full teardown). |

`logical_bytes` is `SUM(byte_len)` — the *logical* footage size (an estimate; it can exceed bytes
reclaimed on disk if any blob is shared, since storage is content-addressed). Actual disk reclaimed
is logged by the GC, not returned inline.

### Export (viewer route — not proxied)

`GET /api/devices/{id}/export.mp4?from=<ns>&to=<ns>&kind=muxed` — streams the window's segments,
in stitch order, of one kind (`muxed` default, or `video`) into a single downloadable MP4. Reuses the
scrub pipeline's per-segment MPEG-TS remux (shared cache), then pipes the concatenated TS through one
long-lived ffmpeg into a *fragmented* MP4 (`+frag_keyframe+empty_moov`, so it streams with bounded
memory at any window size). Per-segment TS carries each segment's codec init; a codec/resolution
*change* within the window still can't go into one clean copy-MP4 — re-encode is a follow-up.

---

## Safety model (load-bearing — it's destructive)

These invariants came out of an adversarial review of the deletion logic and are enforced in
`devices.rs` / `storage.rs`:

1. **Device delete is one transaction, ordered, and holds the speaker + person advisory locks.**
   `speakers.first_seen_device_id` and `persons.first_seen_device_id` are NO-ACTION FKs to `devices`;
   they're NULLed first (inside the tx) so a concurrent voice/face *mint* can't re-point a fresh row
   at the device and FK-fail the delete after segments are already gone. Order: NULL speakers/persons
   `first_seen_device_id` → `DELETE FROM segments` (cascades the derived child tables) →
   `DELETE FROM streams` → `DELETE FROM sessions` → `DELETE FROM devices`. (`streams`/`sessions` do
   **not** cascade off `segments`, so they're deleted explicitly — and **only** in this whole-device
   teardown.)
2. **Day/bulk/retention deletes touch `segments` only** (no stream/session GC — that would race live
   ingest's streams-upsert→segment-insert FK and could 500 a live segment). Deleting a segment cascades
   to every derived table (`transcript_sentences`, `speaker_segments`, `person_segments`,
   `scene_objects`, `segment_transcription_status`, `segment_vision_status`, …) via `ON DELETE CASCADE`.
3. **Blobs are reclaimed only after the row delete commits**, by `storage::reclaim_blobs`, which
   re-checks each content hash against the live DB before unlinking — so a blob still referenced by a
   kept segment (content-addressed storage can share one file across byte-identical segments) is never
   removed. A crash between commit and unlink only orphans a (GC-able) blob, never dangles a row — the
   same durability rule the ingest write path follows.
4. **Retention deletes "fully older" footage** (`capture_start + duration ≤ now − N days`), so a
   segment still partly inside the kept window survives. The sweep runs once at startup, then every
   `RETENTION_SWEEP_SECONDS` (default 6 h), and is idempotent (re-deleting an already-purged window is
   a 0-row no-op), so concurrent passes across processes are harmless.
5. **The worker tolerates a segment vanishing mid-flight** (`claim::segment_exists`): a footage/device
   delete (or retention) can remove a segment while the worker processes it; the derived-row inserts
   then FK-fail and the status row is itself cascade-gone, so it's logged as a benign skip, not an error.

> **Note:** bulk-deleting a speaker's/person's segments does **not** recompute their running-mean
> centroid (it drifts until a recluster). `rolling_summaries` has no `segment_id` and is intentionally
> untouched.

---

## UI

**`🗄 Files`** (top-bar link in the viewer and the System dashboard) → `/manage.html`:

- A **device list** — each row shows the friendly name (inline-rename), source kind, segment count,
  total size, date range, and a **"Keep last [N] days"** retention control.
- Expand a device for its **per-day breakdown** — each day shows segment count + size, a **⬇ MP4**
  export link, a **Delete** button, and a checkbox for **bulk select** ("Export selected" / "Delete
  selected").
- A **"Delete device"** button per device.

**Confirmation** (`confirm.js`): every delete shows its impact (e.g. *"Delete 3,450 segments (2.1 GB)
from Sat, Jun 20?"*). Deleting a whole **device** or its **entire history** additionally requires
typing the device's name to enable the button — there is no undo.

**Baked-in affordances:** the viewer's camera dropdown and the dashboard camera cards now show the
friendly `display_name` (falling back to the device id), and each dashboard card links to
`manage.html?device=<id>`.

---

## Config

| Env var | Default | Meaning |
|---------|---------|---------|
| `RETENTION_SWEEP_SECONDS` | `21600` (6 h) | Interval of the backend retention task (also runs once at startup). |

No new viewer config — export reuses `FFMPEG_BIN` + the existing cache; the proxy reuses
`BACKEND_BASE_URL` / `BACKEND_TOKEN`.

---

## How to test

**Integration tests** (`hushai-backend/tests/devices.rs`, gated on `DATABASE_URL`):

```bash
DATABASE_URL=postgres://localhost/hushai cargo test -p hushai-backend --test devices
```

Covers: list/rename/retention validation, usage local-day bucketing + invalid-tz→400, day delete +
child-table cascade + spared adjacent day, **device delete NULLs `first_seen_device_id` then tears
down**, retention "fully older" + idempotency, and **blob ref-counting** (a shared blob survives until
its last referencing segment is gone).

**Real HTTP end-to-end** (backend on `:8080`, `DEVICE_TOKEN` bearer) — exercise on a *throwaway*
device so you never touch real footage:

```bash
AUTH="Authorization: Bearer $DEVICE_TOKEN"
curl -s -H "$AUTH" localhost:8080/v1/devices | jq                       # list
curl -s -H "$AUTH" "localhost:8080/v1/devices/$DEV/usage?tz=UTC" | jq    # per-day
curl -s -H "$AUTH" -X PATCH -H 'content-type: application/json' \
     -d '{"display_name":"Front Door"}' localhost:8080/v1/devices/$DEV
curl -s -H "$AUTH" -X PUT -H 'content-type: application/json' \
     -d '{"retention_days":30}' localhost:8080/v1/devices/$DEV/retention
curl -s -H "$AUTH" -X DELETE "localhost:8080/v1/devices/$DEV/footage?tz=UTC&day=2021-01-10"
curl -s -H "$AUTH" -X DELETE "localhost:8080/v1/devices/$DEV"            # full teardown
```

After a delete, the blob files under `{BLOB_DIR}/blobs/…` for the removed segments disappear within a
moment (reclamation runs in the background). In the browser, open **`/manage.html`**, rename a device
(confirm it shows in the camera dropdown + dashboard card), set "keep last 1 day", export a date, then
delete it (impact confirm) and delete the device (type-to-confirm).
