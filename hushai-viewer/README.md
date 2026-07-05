# hushai-viewer — unified browser app: NVR + chat over recordings

A browser-based **network video recorder (NVR)** over the Hushai data store, **plus a
chat-over-recordings panel beside the scrubber**. It turns the thousands of tiny
content-addressed clips that capture clients upload into **one continuous, scrubbable
timeline** — pick a camera, watch the footage play through with synced audio, drag a
timeline to any moment, see where recording has gaps — and lets you **ask questions over
all the recordings** in a chat that streams its answer and cites the exact moments;
**clicking a citation jumps the video timeline there.**

It is an Axum service (`127.0.0.1:8070` by default; reachable as **`https://hushai.local/`** on the
LAN — see "Friendly URL" below) plus a self-contained web UI. The NVR side is
strictly **read-only** against the DB and blob store (reusing `hushai-backend` as a
library for the pool/config). The chat side is a thin **reverse proxy**: `/v1/*` is
forwarded to **hushai-rag** (`proxy.rs`) so the browser talks to one origin (no CORS), the
`RAG_TOKEN` bearer is injected server-side, and the response body is **streamed unbuffered**
so chat tokens arrive incrementally (SSE). The one exception is **`/v1/speakers*`**, the
speaker-admin surface that lives in **hushai-backend** (its own bind + `DEVICE_TOKEN`): the
same proxy handler dispatches those by path to the backend upstream (a second axum wildcard
would conflict), which is what powers the **Voices** settings page. The chat UI lives in
`ui/js/chat/*` + the shared `ui/js/store.js`; the Voices page in `ui/js/settings/voices.js`;
citations deep-link the timeline through `store.js` → `app.js`'s `seekToCitation` (the
timeline/player modules are untouched). Sibling of `hushai-worker` and `hushai-rag`.

---

## TL;DR — start it

**Easiest:** [`../local_dev/run_stack.sh`](../local_dev/run_stack.sh) brings up the viewer
together with the rag service (`:8090`) + Ollama + backend that its **chat** panel needs, then
open **http://127.0.0.1:8070/** (or **https://hushai.local/** with `--lan` — see "Friendly URL").

To run the viewer **alone** against an already-running stack, from the **workspace root** (so
`BLOB_DIR=./hushai-backend/data` and the UI dir resolve):

```bash
# Scrubbing needs: Postgres (the Hushai DB, migrations auto-applied) + the blob store + ffmpeg.
# The CHAT panel additionally needs hushai-rag (:8090) running (which itself needs Ollama).
# Set RAG_BASE_URL if the rag service isn't at the http://127.0.0.1:8090 default.
SQLX_OFFLINE=true cargo run -p hushai-viewer
```

Then open **http://127.0.0.1:8070/** in a browser.

Config is read from the root `.env` and `hushai-backend/.env` (same as worker/rag). The viewer
binds to localhost only by default. It **is** the admin panel, so every route (except `/healthz`)
is gated by an IP allowlist (loopback always allowed) **and a password**: set `VIEWER_ADMIN_PASSWORD`
(or `VIEWER_ADMIN_PASSWORD_HASH`), or `VIEWER_AUTH_DISABLED=true` for pure-local dev. `run_stack.sh`
sets a dev password for you. See "Admin access control & TLS" below.

### Friendly URL — `https://hushai.local/`

The default address is `http://127.0.0.1:8070/`. To reach the viewer by a nicer, no-port name over
HTTPS — the same name the TLS cert is issued for (`local_dev/gen_certs.sh` → `CN=hushai.local`) —
**one command does it all (macOS):**

```bash
./local_dev/serve.sh              # cert + CA-trust + Bonjour name + 443→8070 redirect + serve (idempotent)
./local_dev/serve.sh --check      # report what's set up; change nothing
# → open https://hushai.local/
```

**Linux/Windows:** see [`../docs/friendly-url-linux.md`](../docs/friendly-url-linux.md) and
[`../docs/friendly-url-windows.md`](../docs/friendly-url-windows.md).

`serve.sh` just chains these (skipping any already done), if you'd rather run them by hand:

```bash
./local_dev/gen_certs.sh          # cert valid for hushai.local + this host's LAN IP(s)
sudo security add-trusted-cert -d -r trustRoot \
  -k /Library/Keychains/System.keychain local_dev/certs/ca.crt
./local_dev/setup_hostname.sh     # sudo once: LocalHostName=hushai (Bonjour) + a 443→8070 redirect
./local_dev/run_stack.sh --lan    # binds 0.0.0.0:8070 + allowlists this host; serves HTTPS
```

How it fits together: `setup_hostname.sh` makes macOS **Bonjour** advertise `hushai.local` → this
host's LAN IP and adds a `pf` redirect **443 → 8070** (so the port can be dropped); the viewer keeps
binding the unprivileged `8070` and terminates TLS there with the `hushai.local` cert. Because
`hushai.local` resolves to the LAN IP (not loopback), the viewer must bind `0.0.0.0` (`--lan` does
this) and the connecting machine's IP must be in `VIEWER_ADMIN_IP_ALLOWLIST` — `--lan` allowlists
this host automatically; add other admin machines' IPs yourself. Undo with
`./local_dev/setup_hostname.sh --remove`. Re-run after a DHCP IP change (same as `gen_certs.sh`).

---

## What was built

A new workspace crate `hushai-viewer/` with:

### Backend (Rust / Axum)

| Module | Responsibility |
|--------|----------------|
| [`src/main.rs`](src/main.rs) | 3-line entry point → `hushai_viewer::run()`. |
| [`src/lib.rs`](src/lib.rs) | Bootstrap: load config, reuse the backend's DB pool, run shared migrations, create the cache dir, build state, serve (graceful shutdown on Ctrl-C/SIGTERM). |
| [`src/config.rs`](src/config.rs) | `ViewerConfig::from_env()` — viewer-only knobs (bind addr, cache dir, ffmpeg concurrency, window cap). DB/blob config comes from `hushai_backend::config::Config`. |
| [`src/state.rs`](src/state.rs) | Shared `ViewerState` (pool, config, ffmpeg semaphore, single-flight map). |
| [`src/timeline.rs`](src/timeline.rs) | The device-list and windowed-segment SQL queries, and the coalescing that turns raw segment rows into the **spans / coverage / session boundaries** the scrub bar draws. |
| [`src/remux.rs`](src/remux.rs) | Lazily remuxes a stored blob into an MPEG-TS segment with `ffmpeg -c copy`, cached on disk by content hash. Single-flight + a semaphore bound concurrent ffmpeg. |
| [`src/playlist.rs`](src/playlist.rs) | HLS playlist generation — the master playlist and the per-stream media playlists with `#EXT-X-PROGRAM-DATE-TIME` and `#EXT-X-DISCONTINUITY`. |
| [`src/routes.rs`](src/routes.rs) | The HTTP surface: `/api/*` JSON, `/hls/*` playlists + segments, and the static UI. |
| [`src/export.rs`](src/export.rs) | **Footage export:** `GET /api/devices/{id}/export.mp4` — streams a window's segments (per-segment TS remux, shared cache) through one ffmpeg into a fragmented MP4 download. Powers the **Files** page's per-day ⬇ MP4 links. |
| [`src/error.rs`](src/error.rs) | `ViewerError` → HTTP status mapping. |

Plus a shared, additive migration
[`hushai-backend/migrations/0005_viewer_timeline_index.sql`](../hushai-backend/migrations/0005_viewer_timeline_index.sql)
— a `(device_id, capture_start_unix_nanos)` index that backs the windowed-by-device queries.
(`segments` is a plain table, so it's an ordinary btree; every binary runs the shared migration set.)

### Frontend (vanilla JS, no build step)

Self-contained ES modules served at `/`, with **hls.js vendored locally** (no CDN — the
system is no-egress). Files under [`ui/`](ui/):

| File | Responsibility |
|------|----------------|
| `ui/index.html` | Page shell. |
| `ui/styles.css` | Dark NVR theme. |
| `ui/vendor/hls.min.js` | Pinned **hls.js v1.5.20** (Apache-2.0; license in `ui/vendor/hls.LICENSE.txt`). Loaded as a classic script → exposes `window.Hls`. |
| `ui/js/time.js` | ns↔ms conversion and local-timezone formatting. |
| `ui/js/api.js` | `fetch` wrappers; converts the API's Unix-nanosecond fields to milliseconds at the boundary. Includes the chat (`/v1/rag/*`) and speaker-admin (`/v1/speakers*`) helpers. |
| `ui/js/player.js` | Wraps `<video>` + the hls.js instance and exposes a **wall-clock seek API** (the core glue). |
| `ui/js/timeline.js` | The canvas scrub bar — spans/gaps/ticks/playhead, hover tooltip, click-to-seek with gap snapping, wheel-zoom, drag-pan. |
| `ui/js/detections.js` | The **Detections** overlay: fetches `/api/devices/{id}/detections`, indexes frames by timestamp, and draws labeled bounding boxes on a canvas over the `<video>`, synced to playback via the app's rAF ticker (binary-search to the nearest sampled frame; letterbox-aware scaling against `video.videoWidth/Height`). |
| `ui/js/app.js` | Orchestration: device list, state, controls, keyboard, window/seek logic, and the **Video\|Detections** mode toggle (wires `detections.js` into the ticker). |
| `ui/js/chat/*` | Chat dock: `workspace.js` (mounts the **single auto-routed** chat bound to the `auto` agent), `chat-pane.js` (one conversation — message list, composer, **camera-scope dropdown**, **New chat**), `citation.js`. (`agent-picker.js` is retained but unused — routing is server-side.) |
| `ui/js/settings/voices.js` | The **⚙ Voices** modal: list/name/merge speakers + play sample audio (mirrors the Android Voices screen). |
| `ui/manage.html` + `ui/js/manage/manage.js` | The **🗄 Files** page (sibling of the System dashboard): per-device + per-date storage usage, inline **rename**, **retention** ("keep last N days"), **delete** footage by date / in bulk / a whole device, and per-date **⬇ MP4 export**. All mutations proxy to hushai-backend's `/v1/devices*`. |
| `ui/js/confirm.js` | Shared destructive-action confirm dialog — shows impact (segments + size); **type-to-confirm** (the device name) for whole-device / entire-history deletes. |

The friendly `display_name` set on the Files page also shows in the **camera dropdown** (`app.js`) and the **System-dashboard camera cards** (`dashboard.js`), and each dashboard card links to `manage.html?device=<id>`.

---

## How it works (under the hood)

```
 Postgres `segments`         {BLOB_DIR}/blobs/ab/cd/<sha256>
 (metadata, timestamps)      (self-contained ~2s MP4 / fMP4 clips)
            \                       /
             \                     /
        ┌──── hushai-viewer (:8070) ────┐
        │  timeline query → spans/JSON  │   ── /api/* ──►  scrub bar (canvas)
        │  playlist build → m3u8        │   ── /hls/*.m3u8 ─►  hls.js
        │  ffmpeg remux  → MPEG-TS      │   ── /hls/seg/*.ts ─► hls.js → <video>
        │  (cached by content hash)     │
        └───────────────────────────────┘
```

1. **The data.** Capture clients upload ~2s **segments**. The Android client sends *separate*
   self-contained MP4 streams — video (`cam0-video`, H.264, each segment starts on a keyframe →
   independently decodable) and audio (`cam0-audio`, AAC). The `feed_segments.py` reference client
   sends *muxed* fMP4 (a bare fragment + an init blob that must be prepended). Each segment row
   carries its absolute wall-clock start (`capture_start_unix_nanos`), duration, container, and a
   content hash pointing at the blob.

2. **Timeline structure.** `GET /api/devices/{id}/timeline` queries the device's segments in a time
   window, ordered by `(stream_id, session_id, sequence)` (never wall clock, which is skew-prone —
   the same rule `export_capture.sh` uses), and coalesces them into **spans** (contiguous recorded
   runs), a merged **coverage** union (what the bar fills), and **session boundaries**. The UI draws
   this on a canvas.

3. **Stitched playback via HLS.** `GET /hls/{id}/master.m3u8` returns an HLS playlist. Each ~2s blob
   is lazily **remuxed to an MPEG-TS segment** with `ffmpeg -c copy` (no re-encode — milliseconds)
   and **cached forever, keyed by content hash** (immutable bytes → no invalidation). The media
   playlist lists those segments with `#EXT-X-PROGRAM-DATE-TIME` (absolute wall clock) and
   `#EXT-X-DISCONTINUITY` at every session boundary or gap. **hls.js** plays it and stitches across
   discontinuities (including resolution changes) natively.

4. **Synced audio.** Android's separate audio is served as an HLS **alternate-audio rendition** that
   hls.js syncs to video. (Muxed fMP4 sessions are served as a single rendition.)

5. **Wall-clock seeking.** Because every segment carries `PROGRAM-DATE-TIME`, hls.js gives each
   fragment an absolute `programDateTime` and a continuous media `start`. Clicking a time on the
   scrub bar binary-searches the fragment containing that wall-clock instant and seeks
   `video.currentTime = frag.start + (target − frag.programDateTime)`. The playhead is driven back
   from `hls.playingDate`. Clicking a gap snaps to the nearest recorded content; gaps that fall
   *inside* playback are skipped automatically because the timeline jumps across the discontinuity.

### ffmpeg recipes (in `remux.rs`)

```bash
# Android video segment (self-contained mp4 → TS)
ffmpeg -i <blob> -map 0:v:0 -c:v copy -bsf:v h264_mp4toannexb \
       -muxdelay 0 -muxpreload 0 -output_ts_offset <capture_start_s> -f mpegts out.ts

# Android audio segment
ffmpeg -i <blob> -map 0:a:0 -c:a copy \
       -muxdelay 0 -muxpreload 0 -output_ts_offset <capture_start_s> -f mpegts out.ts

# Muxed fMP4 segment: prepend codec_init_data (ftyp+moov) to the bare fragment first, then:
ffmpeg -i <tmp> -map 0 -c copy -bsf:v h264_mp4toannexb \
       -muxdelay 0 -muxpreload 0 -output_ts_offset <capture_start_s> -f mpegts out.ts

# Upright re-encode (only when the source carries a rotation matrix — see below):
# swap `-c:v copy -bsf:v h264_mp4toannexb` for a libx264 re-encode so ffmpeg autorotate
# BAKES the rotation into the pixels (audio still `-c:a copy` for the muxed case).
ffmpeg -i <blob> -map 0:v:0 -c:v libx264 -preset veryfast -crf 20 -pix_fmt yuv420p \
       -muxdelay 0 -muxpreload 0 -output_ts_offset <capture_start_s> -f mpegts out.ts
```

> **Why rotated capture is re-encoded (upright playback).** Android stamps an MP4 rotation
> matrix (`MediaMuxer.setOrientationHint`) so portrait/rotated capture is meant to display
> upright — and ffmpeg autorotate honors it for the vision worker + face/plate JPEG frames.
> But a `-c copy` remux to MPEG-TS **drops the matrix** (TS can't carry it) and hls.js/MSE
> ignore container rotation anyway, so the browser would play sideways. So `remux.rs` first
> `ffprobe`s the video's display-matrix rotation (`stream_side_data_list` — the ffmpeg-7.x
> location; legacy `tags.rotate` is empty there); when non-zero it re-encodes the video with
> libx264 (autorotate default-on → **no** `-vf transpose`/`-noautorotate`, and drop the
> `h264_mp4toannexb` bsf), baking upright pixels (720×1280 for a 90° source). 0°/matrix-less
> segments keep the fast copy path. This also fixes detection-overlay alignment on rotated
> footage, generalizes to any matrix-bearing source (iOS/fMP4), and covers already-recorded
> footage. Toggle with `VIEWER_UPRIGHT_REENCODE=false`. **Purge `cache/ts/` once on deploy** so
> any previously-cached sideways TS regenerates upright.

> **Why `-output_ts_offset <capture_start_s>` matters (the one non-obvious detail).** Each segment
> is remuxed independently, so each TS would otherwise restart its PTS at 0. Serving separate video +
> alternate-audio renditions that way makes hls.js stall right after the first audio segment
> (`bufferStalledError`) because the two renditions don't share a clock. Stamping **every** TS with
> its absolute capture time puts video and audio on one wall-clock PTS clock; hls.js then interleaves
> them and corrects the 33-bit MPEG-TS rollover. The offset is intrinsic to the segment, so the
> content-addressed cache stays valid. Anything that touches the remux must preserve this.

---

## Configuration

DB + blob config is inherited from `hushai_backend::config::Config` (`DATABASE_URL`, `BLOB_DIR`).
Viewer-specific knobs (all optional, with defaults):

| Env var | Default | Meaning |
|---------|---------|---------|
| `VIEWER_BIND_ADDR` | `127.0.0.1:8070` | Bind address. Set `0.0.0.0:8070` to reach it from admin computers — the IP allowlist + password gate then restrict who gets in. |
| `VIEWER_HOSTNAME` | _(none)_ | **Cosmetic only.** Friendly host shown in the startup "open …" log (e.g. `hushai.local` → `https://hushai.local/`). Does not change the bind or routing. Set automatically by `run_stack.sh --lan`. |
| `VIEWER_UI_DIR` | `hushai-viewer/ui` | Directory of the static UI (relative to CWD). |
| `VIEWER_CACHE_DIR` | `{BLOB_DIR}/viewer-cache` | Where remuxed `.ts` segments are cached. |
| `FFMPEG_BIN` | `ffmpeg` | ffmpeg binary (shared with the worker). |
| `FFPROBE_BIN` | _(derived from `FFMPEG_BIN`)_ | ffprobe binary used to read a segment's rotation. Defaults to the `ffprobe` sibling of `FFMPEG_BIN`. |
| `VIEWER_UPRIGHT_REENCODE` | `true` | Re-encode rotated segments upright (bake pixels) so HLS/MSE playback isn't sideways. `false` ⇒ stream-copy everything (today's behavior). |
| `VIEWER_REENCODE_CRF` | `20` | libx264 CRF for the upright re-encode (lower = higher quality). |
| `VIEWER_REENCODE_PRESET` | `veryfast` | libx264 preset for the upright re-encode. |
| `VIEWER_FFMPEG_CONCURRENCY` | # CPUs | Max concurrent ffmpeg remuxes. |
| `VIEWER_DEFAULT_WINDOW_NANOS` | `3600000000000` (1h) | Default window when the client omits `from`/`to`. |
| `VIEWER_MAX_WINDOW_NANOS` | `21600000000000` (6h) | Hard cap on a *playable* window so a playlist can't blow up. |
| `RAG_BASE_URL` | `http://127.0.0.1:8090` | hushai-rag upstream for proxied `/v1/rag/*`, `/v1/tts`. |
| `RAG_TOKEN` | _(none)_ | Bearer injected on rag-bound `/v1/*` requests. |
| `BACKEND_BASE_URL` | `http://127.0.0.1:8080` | hushai-backend upstream for proxied `/v1/speakers*` (the Voices page). |
| `BACKEND_TOKEN` | `DEVICE_TOKEN` | Bearer injected on `/v1/speakers*` requests. Falls back to `DEVICE_TOKEN` (already loaded from `hushai-backend/.env`), so a single-host setup needs no extra config. |

### Admin access control & TLS (`src/auth.rs`)

The viewer **is** the admin panel, so every route (except `/healthz`) is gated by an **IP allowlist
plus a password** (defense in depth). See AGENTS.md "LAN security model" for the full picture.

| Env var | Default | Meaning |
|---------|---------|---------|
| `VIEWER_ADMIN_IP_ALLOWLIST` | _(empty)_ | Comma-separated IPs/CIDRs allowed to reach any route. Empty ⇒ loopback only. |
| `VIEWER_ALLOW_LOOPBACK` | `true` | Always allow `127.0.0.1`/`::1` (host box + curl). |
| `VIEWER_ADMIN_PASSWORD` | _(none)_ | Admin password (hashed in-memory at startup with argon2). |
| `VIEWER_ADMIN_PASSWORD_HASH` | _(none)_ | Pre-computed argon2 PHC hash (preferred in prod; wins over the plaintext). |
| `VIEWER_SESSION_SECRET` | _(random)_ | HMAC key for the signed session cookie. Set it so logins survive restarts. |
| `VIEWER_SESSION_TTL_SECS` | `604800` (7d) | Session lifetime. |
| `VIEWER_COOKIE_SECURE` | _(TLS on?)_ | Add `Secure` to the cookie; defaults to whether TLS is configured. |
| `VIEWER_AUTH_DISABLED` | `false` | Skip the password gate for pure-local dev (the IP gate still applies). |
| `VIEWER_TLS_CERT_PATH` / `VIEWER_TLS_KEY_PATH` | _(bare `TLS_*`)_ | Native rustls TLS; both set ⇒ HTTPS. Falls back to the shared `TLS_CERT_PATH`/`TLS_KEY_PATH`. See `local_dev/gen_certs.sh`. |
| `VIEWER_UPSTREAM_CA` | _(none)_ | CA bundle the proxy/dashboard HTTP client trusts when `RAG_BASE_URL`/`BACKEND_BASE_URL` are `https://` with the private LAN CA. `run_stack.sh --tls` sets this + the https upstream URLs automatically. |

Startup **fails** if neither `VIEWER_ADMIN_PASSWORD` nor `VIEWER_ADMIN_PASSWORD_HASH` is set (unless
`VIEWER_AUTH_DISABLED=true`). `./local_dev/run_stack.sh [--tls]` wires sensible dev defaults and prints
them in its banner.

---

## HTTP API

Times on the wire are **Unix nanoseconds, UTC** (matching `capture_start_unix_nanos`). The UI
localizes for display.

| Method | Path | Response |
|--------|------|----------|
| `GET` | `/healthz` | `ok` |
| `GET` | `/api/devices` | `{ "devices": [ { device_id, source_kind, first_capture_unix_nanos, last_capture_unix_nanos, segment_count, session_count, has_video, has_audio, has_muxed } ] }` |
| `GET` | `/api/devices/{id}/timeline?from&to` | `{ device_id, from, to, spans:[{session_id, stream_id, kind, start_unix_nanos, end_unix_nanos, segment_count}], coverage:[{start_unix_nanos, end_unix_nanos}], session_boundaries:[ns] }` |
| `GET` | `/api/devices/{id}/detections?from&to` | AI detection boxes for the **Detections** overlay, grouped by sampled frame: `{ device_id, from, to, truncated, frames:[{ t_unix_nanos, detections:[{ kind:"person"\|"object", label, person_id, bbox:[x,y,w,h], det_score }] }] }`. Reads `person_segments`(+`persons`) and `scene_objects` (excludes `__frame__` whole-frame rows); `bbox` is original-frame pixels. Window clamped to 6h; `truncated=true` (+ a server WARN) if the per-table cap is hit. |
| `GET` | `/hls/{id}/master.m3u8?from&to` | Master playlist (video variant + alt-audio rendition, or a single muxed variant). |
| `GET` | `/hls/{id}/{video\|audio\|muxed}.m3u8?from&to` | Media playlist (`#EXTINF`, `#EXT-X-PROGRAM-DATE-TIME`, `#EXT-X-DISCONTINUITY`, `#EXT-X-ENDLIST`). |
| `GET` | `/hls/seg/{sha256}.{video\|audio\|muxed}.ts` | The remuxed TS segment (remux-on-demand, then cached; `Cache-Control: immutable`). |
| `GET` | `/api/devices/{id}/export.mp4?from&to&kind` | **Footage export** (`export.rs`): streams the window's segments of one `kind` (`muxed` default, or `video`) into a single fragmented-MP4 download (`Content-Disposition: attachment`). Per-segment TS remux (shared cache) piped through one ffmpeg. Used by the Files page's per-day ⬇ MP4 links **and the in-player ✂ clip export**. |
| `GET` | `/api/devices/{id}/thumb.jpg?t=<ns>` | **Still frame** at wall-clock `t` (`stills.rs`): ffmpeg first-frame extraction (320w) cached content-addressed under `cache/still/`, `ETag`/304. Backs the timeline **hover preview**. |
| `GET` | `/api/devices/{id}/poster.jpg[?t=]` | Newest frame (480w, `max-age=5` + `ETag`) — the **Cameras grid** tiles. Same cache/ffmpeg machinery as `thumb.jpg`. |
| `GET` | `/api/devices/{id}/sentiment?from&to` | **Mood ribbon** data (`sentiment.rs`): positive/neutral/negative runs coalesced from `transcript_sentences` at segment grain. Same 6h clamp + `truncated` flag as `/processing`. |
| `GET` | `/styles.css` | The shared stylesheet, deliberately **public** (pre-auth, still IP-allowlisted) so the login page shares the design system. |
| `ANY` | `/v1/events*`, `/v1/alert-rules*`, `/v1/watchlist*`, `/v1/audit` | **Reverse-proxied to hushai-backend**: the proactive layer behind the timeline events lane, the events drawer + alert bell, the **🔔 Alerts center** (feed ack, rule CRUD/edit), the **⭐ Watchlist**, and the System page's **audit trail**. |
| `ANY` | `/v1/rag/*`, `/v1/tts` | **Reverse-proxied to hushai-rag** (`RAG_BASE_URL`): chat (`POST /v1/rag/chat` SSE, `GET /v1/rag/agents`, `GET /v1/rag/chat/sessions[/{id}/messages]`), plus the existing `/v1/rag/query` + `/v1/tts`. `RAG_TOKEN` injected server-side; body streamed unbuffered (SSE). |
| `ANY` | `/v1/speakers*` | **Reverse-proxied to hushai-backend** (`BACKEND_BASE_URL`): the speaker-admin surface behind the **Voices** page — `GET /v1/speakers`, `PATCH /v1/speakers/{id}`, `GET /v1/speakers/duplicates`, `POST /v1/speakers/{id}/merge`, `POST /v1/speakers/merge-group`, `GET /v1/speakers/{id}/sample-audio`. `BACKEND_TOKEN` injected server-side. |
| `ANY` | `/v1/persons*` | **Reverse-proxied to hushai-backend**: the person (face) catalog — `GET /v1/persons`, `PATCH /v1/persons/{id}` (name a face), `POST /v1/persons/{id}/merge`, `GET /v1/persons/{id}/sample-face` (cropped JPEG). Same server-side bearer as `/v1/speakers*`. |
| `ANY` | `/v1/devices*` | **Reverse-proxied to hushai-backend**: the device-management + footage-deletion surface behind the **🗄 Files** page — `GET /v1/devices`, `GET /v1/devices/{id}/usage?tz=`, `PATCH /v1/devices/{id}` (rename), `PUT /v1/devices/{id}/retention`, `DELETE /v1/devices/{id}/footage?tz=&day=`, `POST /v1/devices/{id}/footage/bulk-delete`, `DELETE /v1/devices/{id}` (device + all footage). Same server-side bearer as `/v1/speakers*`. See [`../docs/device-and-footage-management.md`](../docs/device-and-footage-management.md). |
| `GET` | `/` and any other path | The static UI (`ServeDir`) — NVR scrubber + chat panel. |

`{kind}` is derived from `media_type`: `1`→`audio`, `2`→`video`, `3`→`muxed`.

---

## Using the UI

- **Camera selector** (top bar) — every device that has reported; opens on the one with the most footage.
- **Timeline (bottom)** — teal = recorded, dark = gap, amber tick = session start. **Click or drag** to
  seek; clicking a gap snaps to the nearest recording. **Scroll** to zoom (hours ↔ seconds), **drag the
  label strip** to pan, **Fit all** to see the whole device.
- **Transport** (overlay, appears on hover over the video) — prev/play/next recording, **speed**
  1×/2×/4×/8× (auto-mutes ≥4×), **🔇 mute / volume**, **⦿ Latest**, **⛶ fullscreen**.
- **Video | Detections tab** (top-left of the video, or press **`d`**) — switch into **Detections**
  mode to overlay AI bounding boxes on the playing video: each detected **person** is boxed and labeled
  with their name (or **"Unidentified"**), each **object** with its class (chair, cup, …). Boxes are
  synced to playback (snapped to the nearest sampled frame, ~1.5×/sec) and scale to the displayed video.
  Requires the worker's vision pipeline to have populated `person_segments`/`scene_objects`; with no
  detections the overlay is simply empty.
- **Date navigation** — `‹ ›` to step days, or the date picker to jump.
- **LIVE pill** — red while following the newest footage (auto-chases as segments land); any manual
  seek drops back to browsing, click **GO LIVE** (or `Shift+L`) to re-engage. A pulsing cap on the
  timeline marks the live edge while footage is fresh.
- **Events lane + drawer + bell** — severity glyphs (◆ critical ▲ warning • info) on their own
  timeline lane (click to jump; dense spots cluster into ×N chips); **☰ Events** opens a filterable
  drawer beside the video; the topbar **🔔** badge counts unacked alerts with per-item Ack and
  jump-to-footage, and links into the full **Alerts center** (`/events.html` — feed, watchlist,
  event stream, rule editor).
- **✂ Export** (or `E`) — drag a range on the timeline (grips adjustable, `i`/`o` set in/out at the
  playhead) and download it as MP4; warns about gaps and the 6h server clamp.
- **Hover previews + mood** — hovering the bar floats a real frame thumbnail at that instant;
  a slim **MOOD** lane under Audio/Vision shades positive/neutral/negative stretches of speech.
- **`/` or `Cmd+K`** — the omni-search palette: cameras, jump-to-time ("yesterday 5pm"), license
  plates (server-side fuzzy search with crops), people & voices (jump to latest sighting), or hand
  the query to the AI chat. Available on every page.
- **📷 Cameras** (`/cameras.html`) — all cameras at a glance: poster tiles refreshing every 10s,
  live/idle/offline dots, unacked-alert chips, and a one-at-a-time live **peek** player on hover;
  click a tile to open that camera in the viewer.
- **Chat (right dock)** — ask questions over the recordings; answers stream and cite the moment
  (click a citation to jump the timeline). Per-conversation controls: a **camera-scope dropdown**
  (default **All cameras**, or limit answers to one camera — independent of the video selector) and
  **New chat** (clears the conversation and starts a fresh server session).
- **⚙ Voices** (top bar) — opens a modal listing the speakers found in your recordings, split into
  **Known voices** (already named/identified) and **Unidentified voices**. **Name** a voice, **play a
  sample** to recognize it by ear, and **merge** duplicates the matcher over-split (the web counterpart
  of the Android Voices screen; talks to hushai-backend via the proxy).
- **🗄 Files** (top bar → `/manage.html`) — device & footage management. **Rename** a device, see
  **per-device and per-date storage usage**, set a **retention** policy ("keep last N days", auto-purged
  by the backend), **delete** footage by date (one day or a bulk multi-select) or a whole device, and
  **⬇ export** a date to MP4 before deleting. Destructive deletes confirm with impact (segments + size);
  deleting a whole device / its entire history requires typing the device name. Full reference:
  [`../docs/device-and-footage-management.md`](../docs/device-and-footage-management.md).

### Keyboard shortcuts

Press **`?`** in the viewer for the built-in cheat sheet.

| Key | Action | Key | Action |
|-----|--------|-----|--------|
| `Space` / `K` | play / pause | `[` / `]` | previous / next recorded span |
| `←` / `→` | seek ∓5s (`Shift`: ∓60s) | `J` / `L` | seek ∓10s |
| `,` / `.` | frame step back / forward | `<` / `>` | speed down / up (0.25×–8×) |
| `Home` / `End` | earliest / latest footage | `Shift+L` | go LIVE (follow the edge) |
| `+` / `−`, `0` | zoom timeline in / out, fit all | `M` / `F` | mute / fullscreen |
| `1` `2` `3` `4` | speed 1× / 2× / 4× / 8× | `D` / `A` | detections overlay / AI ribbons |
| `E`, `i` / `o` | export mode, set in / out point | `/` or `Cmd+K` | omni-search palette |
| `Alt`+click/drag | precise seek (no gap snap) | `Shift`+drag | zoom to selection |

Touch: one-finger scrub/pan by zone, **two-finger pinch** zooms the bar; middle-drag pans anywhere.

---

## Caching & performance

- Remuxed segments live under `VIEWER_CACHE_DIR/ts/<ab>/<cd>/<sha>.<variant>.ts`, sharded like the
  blob store. They are **pure derived data** — deleting the cache is always safe (it regenerates on
  next view). There is no eviction loop; prune by mtime if it ever grows too large.
- First view of a segment pays one stream-copy remux (~tens of ms); cached thereafter. A fast scrub
  is bounded by `VIEWER_FFMPEG_CONCURRENCY` and a per-segment single-flight lock so the same bytes
  are never remuxed twice concurrently.
- The *playable* HLS window is capped at `VIEWER_MAX_WINDOW_NANOS` (6h). The timeline *view* can show
  the device's full extent (cheap JSON); the player loads a ≤6h window and reloads transparently when
  you seek beyond it.

---

## Verifying it works

```bash
# 1. Unit tests (timeline coalescing + playlist generation; no DB needed)
SQLX_OFFLINE=true cargo test -p hushai-viewer

# 2. API smoke (server running)
curl -s http://127.0.0.1:8070/api/devices | python3 -m json.tool
DEV="<device_id from above>"
curl -s "http://127.0.0.1:8070/api/devices/$DEV/timeline" | python3 -m json.tool | head

# 3. A remuxed segment really decodes
curl -s "http://127.0.0.1:8070/hls/$DEV/video.m3u8?from=0&to=9000000000000000000" | head
SHA=...   # from the playlist
curl -s "http://127.0.0.1:8070/hls/seg/$SHA.video.ts" -o seg.ts
ffprobe -v error -show_entries stream=codec_type,codec_name,start_time seg.ts

# 4. End-to-end stitch: ffmpeg consumes the playlist over HTTP and produces a real file
ffmpeg -i "http://127.0.0.1:8070/hls/$DEV/master.m3u8?from=<F>&to=<T>" -t 40 -c copy out.mp4
ffprobe out.mp4   # expect h264 + aac, ~40s
```

**Browser playback + the whole UI surface** are verified by the committed E2E sweep in
[`e2e/`](e2e/README.md) — it drives the *real* Google Chrome headlessly (open-source Chromium
lacks H.264/AAC) through boot/decode, seeking, frame stepping, hover previews, events + bell,
clip export (a real MP4 fetch), modal focus behavior, the omni palette, the Cameras grid, the
Alerts center and the audit trail, with a run-wide guard that no native `alert()`/`confirm()`
ever fires. `cd e2e && npm i && node run.mjs` against a running stack.

---

## Scope & follow-ups

**In scope (this version):** recorded/historical browsing + **multi-turn chat over the
recordings** (SSE-streamed, DB-backed conversations, citations that deep-link the timeline,
a single **auto-routed** chat box — the server classifier dispatches each message to the right
agent: Recordings / Reflection / Objects / People / Plates / Events). **LAN-ready:** the whole app
is gated by an IP allowlist + password and can serve native TLS (see "Admin access control & TLS").

**Deliberately deferred (clean follow-ups):**
- **More agents** — extra personas/scopes are one registry entry in `hushai-rag/src/agents.rs`; the
  server auto-router picks them up automatically (no UI change; `agent-picker.js` is unused).
- **Near-live tailing** — a non-`ENDLIST`, sliding playlist off the newest session. Reuses ~90% of
  `playlist.rs` / `remux.rs`. (The LIVE pill approximates this today by chasing window reloads.)
- ~~**Auth / LAN exposure**~~ — **done (2026-06-28):** IP allowlist + password gate + native TLS
  (see "Admin access control & TLS"). Future hardening: per-user accounts, hot token revocation.
- ~~**Hover thumbnails**~~ — **done (2026-07-02):** `stills.rs` + `thumb.jpg` + the `#tlPreview`
  hover card (part of the UI overhaul that also added the events lane/drawer/bell, watchlist,
  clip export, mood ribbon, omni-search, the Cameras grid, and the audit trail).
- **Retrieval query-condensation across chat turns** — currently retrieval re-anchors on the
  latest message (prior turns inform the LLM only); a future `RAG_CHAT_CONDENSE` flag would
  rewrite the follow-up into a standalone retrieval query.

**Notes / known edge cases:**
- Wall clock (`capture_start_unix_nanos`) is the device's uncorrected clock; it's treated as truth for
  placement. Sub-segment skew is invisible at 2s granularity.
- A/V sync is segment-granular (good for a camera viewer; not sample-accurate lip-sync).
- The TS cache keys on content hash; if two segments ever shared identical bytes but different capture
  times, the cached TS would carry the first one's PTS anchor (negligible for real footage).
