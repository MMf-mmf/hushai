#!/usr/bin/env bash
# run_loadtest.sh — drive the camera fan-out capacity benchmark across worker-config profiles.
#
# Prereq: the stack is already up (./local_dev/run_stack.sh) — backend :8080, viewer :8070, postgres,
# and (for non-audio profiles) the vision models + Ollama provisioned. This script owns ONLY the
# worker lifecycle: for each profile it stops the running worker, relaunches it with that profile's
# env (the worker reads all toggles at startup — there is no runtime reconfig), waits for a fresh
# heartbeat + a drained queue, then runs `hushai-loadtest` to ramp 1..N cameras and write a report.
#
# Usage:
#   ./local_dev/run_loadtest.sh                         # default sweep
#   ./local_dev/run_loadtest.sh audio-only everything   # specific profiles, in order
#   ./local_dev/run_loadtest.sh --release --max 30 --soak 150 everything
#
# To see the LIVE dashboard panel, the viewer must be started with
#   VIEWER_LOADTEST_LIVE_JSON=local_dev/logs/loadtest-live.json
# exported (add it to the root .env, then restart the stack). The CSV/report are written regardless.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOG_DIR="$SCRIPT_DIR/logs"
LIVE_JSON="$LOG_DIR/loadtest-live.json"
OUT_DIR="$REPO_ROOT/loadtest-out"
mkdir -p "$LOG_DIR" "$OUT_DIR"

PROFILE="debug"
MAX_CAMERAS=30
SOAK=150
SAMPLE=5
VIDEO="$REPO_ROOT/IMG_7256.mp4"
NO_PM=""
PROFILES=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --release) PROFILE="release"; shift ;;
    --max)     MAX_CAMERAS="$2"; shift 2 ;;
    --soak)    SOAK="$2"; shift 2 ;;
    --sample)  SAMPLE="$2"; shift 2 ;;
    --video)   VIDEO="$2"; shift 2 ;;
    --no-pm)   NO_PM="--no-powermetrics"; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *)         PROFILES+=("$1"); shift ;;
  esac
done
[[ ${#PROFILES[@]} -eq 0 ]] && PROFILES=(audio-only audio-sentiment audio-vision everything)

PROFILE_DIR="$REPO_ROOT/target/$PROFILE"
PROFILE_FLAG=(); [[ "$PROFILE" == "release" ]] && PROFILE_FLAG=(--release)
# Same DYLD discipline as run_stack.sh so the worker's ORT/sherpa dylibs resolve under SIP.
DYLD_FB="$PROFILE_DIR/deps:$PROFILE_DIR:/usr/local/lib:/usr/lib"
WORKER_METRICS_ADDR="127.0.0.1:9100"
DASHBOARD_URL="http://localhost:8070/api/dashboard"
TOKEN="${DEVICE_TOKEN:-dev-secret-token}"

log() { printf '\033[1;36m[loadtest]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[loadtest] %s\033[0m\n' "$*" >&2; exit 1; }

# Per-profile worker env (printed as `KEY=VAL` lines). To disable the object lane (no master flag)
# we point its model paths at a nonexistent file so it self-disables while faces still run.
profile_env() {
  echo "WORKER_METRICS_ADDR=$WORKER_METRICS_ADDR"
  case "$1" in
    audio-only)      echo "VISION_ENABLED=false"; echo "SENTIMENT_ENABLED=false"; echo "PLATE_ENABLED=false"; echo "EVENTS_ENABLED=false" ;;
    audio-sentiment) echo "VISION_ENABLED=false"; echo "SENTIMENT_ENABLED=true";  echo "PLATE_ENABLED=false"; echo "EVENTS_ENABLED=false" ;;
    audio-vision)    echo "VISION_ENABLED=true";  echo "SENTIMENT_ENABLED=true";  echo "PLATE_ENABLED=false";
                     echo "OBJECT_DET_MODEL_PATH=/nonexistent-disable.onnx"; echo "CLIP_IMAGE_MODEL_PATH=/nonexistent-disable.onnx"; echo "EVENTS_ENABLED=false" ;;
    everything)      echo "VISION_ENABLED=true";  echo "SENTIMENT_ENABLED=true";  echo "PLATE_ENABLED=true";  echo "EVENTS_ENABLED=true" ;;
    conc1)  echo "VISION_ENABLED=true"; echo "SENTIMENT_ENABLED=true"; echo "PLATE_ENABLED=true"; echo "WORKER_CONCURRENCY=1" ;;
    conc2)  echo "VISION_ENABLED=true"; echo "SENTIMENT_ENABLED=true"; echo "PLATE_ENABLED=true"; echo "WORKER_CONCURRENCY=2" ;;
    conc4)  echo "VISION_ENABLED=true"; echo "SENTIMENT_ENABLED=true"; echo "PLATE_ENABLED=true"; echo "WORKER_CONCURRENCY=4" ;;
    conc6)  echo "VISION_ENABLED=true"; echo "SENTIMENT_ENABLED=true"; echo "PLATE_ENABLED=true"; echo "WORKER_CONCURRENCY=6" ;;
    # vision-lane fan-out: hold the audio lane fixed and sweep VISION_CONCURRENCY (the new knob) so
    # the report attributes the vision saturation knee. Run against a VIDEO/MUXED corpus (--video).
    visconc1) echo "VISION_ENABLED=true"; echo "SENTIMENT_ENABLED=true"; echo "PLATE_ENABLED=true"; echo "WORKER_CONCURRENCY=2"; echo "VISION_CONCURRENCY=1" ;;
    visconc2) echo "VISION_ENABLED=true"; echo "SENTIMENT_ENABLED=true"; echo "PLATE_ENABLED=true"; echo "WORKER_CONCURRENCY=2"; echo "VISION_CONCURRENCY=2" ;;
    visconc4) echo "VISION_ENABLED=true"; echo "SENTIMENT_ENABLED=true"; echo "PLATE_ENABLED=true"; echo "WORKER_CONCURRENCY=2"; echo "VISION_CONCURRENCY=4" ;;
    *) die "unknown profile '$1' (try: audio-only audio-sentiment audio-vision everything conc1 conc2 conc4 conc6 visconc1 visconc2 visconc4)" ;;
  esac
}

stop_worker() {
  if [[ -f "$LOG_DIR/worker.pid" ]]; then
    local pid; pid="$(cat "$LOG_DIR/worker.pid" 2>/dev/null || true)"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      log "stopping worker (pid $pid)"
      kill -TERM "$pid" 2>/dev/null || true
      for _ in $(seq 1 20); do kill -0 "$pid" 2>/dev/null || break; sleep 0.5; done
      kill -KILL "$pid" 2>/dev/null || true
    fi
    rm -f "$LOG_DIR/worker.pid"
  fi
}

start_worker() {
  local profile="$1" envfile; envfile="$(mktemp)"
  profile_env "$profile" >"$envfile"
  log "launching worker for profile '$profile':"; sed 's/^/    /' "$envfile"
  (
    cd "$REPO_ROOT"
    set -a
    export SQLX_OFFLINE=true DYLD_FALLBACK_LIBRARY_PATH="$DYLD_FB"
    # shellcheck disable=SC1090
    source "$envfile"
    set +a
    exec "$PROFILE_DIR/hushai-worker"
  ) >"$LOG_DIR/worker.log" 2>&1 &
  local pid=$!
  echo "$pid" >"$LOG_DIR/worker.pid"
  rm -f "$envfile"
  # Wait for /healthz.
  for _ in $(seq 1 40); do
    if curl -sf -o /dev/null --max-time 2 "http://$WORKER_METRICS_ADDR/healthz" 2>/dev/null; then
      log "worker up (pid $pid), metrics on $WORKER_METRICS_ADDR"; return 0
    fi
    kill -0 "$pid" 2>/dev/null || { tail -n 20 "$LOG_DIR/worker.log" || true; die "worker exited during startup"; }
    sleep 0.5
  done
  die "worker never answered /healthz (see $LOG_DIR/worker.log)"
}

# Poll the dashboard until both queues are ~drained (so a prior profile's backlog can't contaminate
# the next run's saturation point). Best-effort: gives up after ~2 min and proceeds.
drain_queue() {
  log "waiting for queues to drain before the run…"
  for _ in $(seq 1 120); do
    local depth
    depth="$(curl -sf --max-time 3 "$DASHBOARD_URL" 2>/dev/null | python3 -c '
import sys, json
try:
    d = json.load(sys.stdin)["queues"]
    t, v = d["transcription"], d["vision"]
    print(t["pending"]+t["processing"]+v["pending"]+v["processing"])
except Exception:
    print(-1)
' 2>/dev/null || echo -1)"
    [[ "$depth" == "0" ]] && { log "queues drained"; return 0; }
    sleep 1
  done
  log "drain wait timed out; proceeding anyway"
}

cleanup_on_exit() { stop_worker; }
trap cleanup_on_exit EXIT

# Build once.
log "building (profile=$PROFILE)…"
SQLX_OFFLINE=true cargo build "${PROFILE_FLAG[@]}" -p hushai-worker -p hushai-loadtest >/dev/null \
  || die "build failed"

[[ -f "$VIDEO" ]] || die "source video not found: $VIDEO (pass --video PATH)"
log "live.json -> $LIVE_JSON (set VIEWER_LOADTEST_LIVE_JSON to this for the dashboard panel)"

for prof in "${PROFILES[@]}"; do
  log "================  PROFILE: $prof  ================"
  stop_worker
  start_worker "$prof"
  drain_queue
  "$PROFILE_DIR/hushai-loadtest" \
    --video "$VIDEO" \
    --token "$TOKEN" \
    --max-cameras "$MAX_CAMERAS" \
    --soak-secs "$SOAK" \
    --sample-secs "$SAMPLE" \
    --profile "$prof" \
    --out-dir "$OUT_DIR" \
    --live-json "$LIVE_JSON" \
    --worker-pid-file "$LOG_DIR/worker.pid" \
    $NO_PM \
    || log "profile '$prof' run returned non-zero (see output above)"
done

log "sweep complete. Reports under $OUT_DIR/run-*-<profile>/  (report.md, summary_by_N.csv, timeseries.csv)"
log "NOTE: the benchmark worker has been stopped; restart your stack worker (./local_dev/run_stack.sh) for normal use."
