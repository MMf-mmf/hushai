# Viewer E2E sweep

Drives the real UI in **real Google Chrome** headlessly (open-source Chromium lacks
H.264/AAC decoders, so `puppeteer`'s bundled browser can't play the footage).

## Run

```sh
# 1. Stack up with viewer auth disabled (no VIEWER_ADMIN_PASSWORD set) and at least
#    one video-bearing device in the DB:
SQLX_OFFLINE=true cargo run -p hushai-viewer          # from the repo root
python3 local_dev/feed_segments.py                    # seed synthetic footage if needed

# 2. From this directory:
npm i
node run.mjs                                          # or: npm test
```

Environment knobs: `VIEWER_URL` (default `http://127.0.0.1:8070`), `CHROME` (default
`/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`).

## What it checks

Boot + H.264 decode, the public pre-auth `/styles.css`, directional gap-seek toast +
optimistic playhead, frame stepping, hover preview thumbnails, event markers + alert-bell
ack, clip-export selection→href→real MP4 download, modal focus restore, the omni palette,
the cameras poster grid, the alerts center sections, the dashboard KPIs + audit trail —
and, across the whole run, that **no native `alert()`/`confirm()` ever fires**.

Checks whose fixture prerequisite is missing (no events seeded, audio-only device) report
`SKIP` with the reason instead of a false pass; the process exits non-zero only on `FAIL`.
