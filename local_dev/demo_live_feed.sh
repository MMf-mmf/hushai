#!/usr/bin/env bash
#
# demo_live_feed.sh — replay the demo clips in real time so a stack built by build_demo.sh reads
# as a LIVE install for as long as this runs.
#
# Why this exists: the dashboard and the camera wall classify a camera purely from how recently
# it uploaded (<15s live, <5m idle, else offline — hushai-viewer/ui/js/cameras/cameras.js:20).
# A dataset injected once and then left alone therefore goes red within five minutes, so a
# perfectly healthy demo stack screenshots as three OFFLINE cameras and "0 / 3 CAMERAS ONLINE".
# This loop keeps the trailing second of each camera current, which is exactly what a real
# capture client does.
#
# It deliberately feeds only ONE 2-second segment per camera per tick: enough to stay inside the
# 15s live threshold, little enough that the work queues do not build a backlog that reads as the
# worker failing to keep up.
#
# Usage:
#   ./local_dev/demo_live_feed.sh &                       # note the PID it prints
#   cd hushai-viewer/e2e && SHOTS_PASS=live node shots.mjs
#   kill %1                                               # or the printed PID
#   ./local_dev/demo_live_feed.sh --cleanup               # REQUIRED afterwards, see below
#
# Ctrl-C / SIGTERM stops it cleanly. Segment ids are seeded per tick, so nothing ever collides
# with the historical dataset or with a previous run.
#
# Clean up when you are done. Every segment this fed carries `attrs.demo_live = "1"`, and
# `--cleanup` deletes exactly those (all dependent rows cascade). This is not housekeeping — it is
# load-bearing. The player can only navigate the last VIEWER_MAX_WINDOW_NANOS (6h) of a camera's
# footage, measured back from its NEWEST segment. Leaving live segments behind pins that window to
# the present, which puts every frame of the demo's own footage permanently out of reach and makes
# the next `SHOTS_PASS=player` run silently photograph the live tail instead.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$ROOT/local_dev/.demo_work"
CLIPDIR="$WORK/clips"
BACKEND="${DEMO_BACKEND:-http://localhost:8080}"
DB_URL="${DEMO_DB_URL:-postgres://${USER}@localhost:5432/hushai_demo}"
TICK="${DEMO_LIVE_TICK:-8}" # seconds between ticks; must stay under the 15s live threshold

log() { printf '[live] %s\n' "$*"; }
die() { printf '[live] FATAL: %s\n' "$*" >&2; exit 1; }

# Same guard as build_demo.sh: never let this touch a real deployment.
DB_NAME="${DB_URL##*/}"; DB_NAME="${DB_NAME%%\?*}"
[[ "$DB_NAME" == *_demo ]] || die "refusing to run against \"$DB_NAME\" — this script only ever
       touches a database whose name ends in _demo. Set DEMO_DB_URL if yours is elsewhere."

if [[ "${1:-}" == "--cleanup" ]]; then
  # Transcripts, detections, person/speaker attributions and queue status all cascade off
  # `segments`. `events` does NOT: its FK is ON DELETE SET NULL (migration 0014), so the events
  # this replay produced would survive as segment-less rows and keep showing up as sightings on
  # the Investigate page. Delete them explicitly, before the segments they point at disappear.
  psql "$DB_URL" -v ON_ERROR_STOP=1 -q <<'SQL'
BEGIN;
DELETE FROM events e
 USING segments s
 WHERE e.segment_id = s.segment_id
   AND s.attrs->>'demo_live' = '1';
DELETE FROM segments WHERE attrs->>'demo_live' = '1';
COMMIT;
SQL
  log "removed the live-replay segments and their events; the demo footage is navigable again"
  # Cumulative aggregates are a different matter: the entity graph's edge counters and the profile
  # digests were incremented by those sightings and cannot be decremented back. They are harmless
  # for a demo, but if the counts matter (they do for a screenshot of the Investigate page) the
  # honest fix is a rebuild: DEMO_RESET=1 ./local_dev/build_demo.sh.
  log "note: entity-graph counters keep the replay's contribution — DEMO_RESET=1 to start clean"
  exit 0
fi

TOKEN="${DEVICE_TOKEN:-}"
if [[ -z "$TOKEN" && -f "$ROOT/.env" ]]; then
  TOKEN="$(grep -E '^DEVICE_TOKEN=' "$ROOT/.env" | head -1 | cut -d= -f2- || true)"
fi
TOKEN="${TOKEN:-dev-secret-token}"

# device_id|clip
CAMERAS=(
  "demo-front-door|front_door.mp4"
  "demo-driveway|driveway.mp4"
  "demo-back-garden|back_garden.mp4"
)

for row in "${CAMERAS[@]}"; do
  clip="${row#*|}"
  [[ -f "$CLIPDIR/$clip" ]] || die "missing $CLIPDIR/$clip — run ./local_dev/build_demo.sh first"
done

running=1
trap 'running=0' INT TERM

log "replaying 3 cameras live every ${TICK}s against $BACKEND — Ctrl-C to stop"
tick=0
while (( running )); do
  now_ns="$(python3 -c 'import time; print(int(time.time()*1e9))')"
  # Land the segment so it ENDS at `now`: a segment stamped in the future would put footage on
  # the timeline that has not happened yet.
  start_ns=$(( now_ns - 2000000000 ))
  for row in "${CAMERAS[@]}"; do
    dev="${row%%|*}"; clip="${row#*|}"
    python3 "$ROOT/local_dev/feed_segments.py" \
      --device "$dev" \
      --video "$CLIPDIR/$clip" \
      --url "$BACKEND/v1/segments" \
      --token "$TOKEN" \
      --limit 1 \
      --capture-start-ns "$start_ns" \
      --segment-id-seed "live-$dev-$now_ns-$tick" \
      --attr demo_live=1 \
      >/dev/null 2>&1 || log "WARN: $dev tick $tick did not land"
  done
  tick=$(( tick + 1 ))
  (( running )) || break
  sleep "$TICK"
done
log "stopped after $tick ticks"
