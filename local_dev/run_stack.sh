#!/usr/bin/env bash
#
# run_stack.sh — bring up the WHOLE Hushai backend/AI/web stack with ONE command.
#
# Starts (in dependency order) the two infra deps + all four Rust services, streams
# each service's logs to local_dev/logs/, health-checks every port, then holds the
# foreground. A single Ctrl-C tears the whole thing down cleanly. Infra the script
# did NOT start (an already-running Postgres / Ollama) is left running on exit.
#
#   ./local_dev/run_stack.sh                    # infra + backend + worker + rag + viewer
#   ./local_dev/run_stack.sh --with-android     # ...also build+drive the phone client (best effort)
#   ./local_dev/run_stack.sh --release          # build/run the release binaries
#   ./local_dev/run_stack.sh --no-build         # skip cargo build (run existing target/<profile> bins)
#   ./local_dev/run_stack.sh --pull             # `ollama pull` any missing models, then continue
#   ./local_dev/run_stack.sh --down             # stop a stack started earlier, then exit
#   ./local_dev/run_stack.sh --with-android -- --audio-only --duration 60
#                                               # everything after `--` is forwarded to run_hushai_app.sh
#
# WHY a script and not docker-compose: this machine has no Docker, Postgres is a
# Homebrew service, Ollama is native, and the AI stack dlopen()s local ONNX/whisper
# models with CoreML/Metal accel that a Linux container on macOS can't reach. So the
# one-command answer here is native process orchestration.
#
# Services + the env each one needs (mirrors AGENTS.md "Run the full stack" + the
# launchd worker plist):
#   backend  :8080   CWD=hushai-backend   (loads hushai-backend/.env; BLOB_DIR=./data)
#   worker   (no port)  CWD=repo root     (loads root .env; models/* + blobs are relative)
#   rag      :8090   CWD=repo root        (loads root .env)
#   viewer   :8070   CWD=repo root        (loads root .env)
# All are launched with SQLX_OFFLINE=true and DYLD_FALLBACK_LIBRARY_PATH pointing at
# the build dir so sherpa-rs's bundled libonnxruntime.1.17.1.dylib resolves for a
# directly-launched binary (AGENTS.md "vision ONNX runtime" / worker plist).
set -euo pipefail

# ---------------------------------------------------------------------------
# Layout
# ---------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOG_DIR="$SCRIPT_DIR/logs"

# ---------------------------------------------------------------------------
# Args
# ---------------------------------------------------------------------------
WITH_ANDROID=0
DO_BUILD=1
PROFILE="debug"
PULL_MODELS=0
DOWN_ONLY=0
ANDROID_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --with-android) WITH_ANDROID=1; shift ;;
    --no-build)     DO_BUILD=0; shift ;;
    --release)      PROFILE="release"; shift ;;
    --pull)         PULL_MODELS=1; shift ;;
    --down)         DOWN_ONLY=1; shift ;;
    --)             shift; ANDROID_ARGS=("$@"); break ;;
    -h|--help)      sed -n '3,30p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1 (try --help)" >&2; exit 2 ;;
  esac
done

PROFILE_DIR="$REPO_ROOT/target/$PROFILE"
PROFILE_FLAG=(); [[ "$PROFILE" == "release" ]] && PROFILE_FLAG=(--release)
# Build dir first so the loader finds sherpa's bundled libonnxruntime.1.17.1.dylib
# (AGENTS.md launch note). This is set + exec'd by `launch` *directly* (see there):
# routing through /usr/bin/env would let SIP strip DYLD_* before the binary runs.
DYLD_FB="$PROFILE_DIR/deps:$PROFILE_DIR:/usr/local/lib:/usr/lib"

mkdir -p "$LOG_DIR"

# Parallel arrays: a running service's name -> its PID (filled by `launch`).
SVC_NAMES=()
SVC_PIDS=()
OLLAMA_PID=""   # set only if WE started ollama (then we stop it on teardown)

log()  { echo "[$1] ${*:2}"; }
die()  { echo "ERROR: $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# --down: stop a previously-started stack from its pid files, then exit.
# ---------------------------------------------------------------------------
stop_from_pidfiles() {
  local stopped=0 f name pid
  shopt -s nullglob
  for f in "$LOG_DIR"/*.pid; do
    name="$(basename "$f" .pid)"; pid="$(cat "$f" 2>/dev/null || true)"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      log down "stopping $name (pid $pid)"; kill -TERM "$pid" 2>/dev/null || true; stopped=1
    fi
    rm -f "$f"
  done
  shopt -u nullglob
  [[ "$stopped" -eq 0 ]] && log down "no tracked processes were running"
}

if [[ "$DOWN_ONLY" -eq 1 ]]; then
  stop_from_pidfiles
  log down "done"
  exit 0
fi

# ---------------------------------------------------------------------------
# Teardown — reverse order, idempotent, runs on Ctrl-C / error / normal exit.
# Postgres (a brew service) is deliberately left running.
# ---------------------------------------------------------------------------
CLEANED=0
cleanup() {
  [[ "$CLEANED" -eq 1 ]] && return; CLEANED=1
  echo
  log down "stopping services…"
  local i
  for (( i=${#SVC_PIDS[@]}-1; i>=0; i-- )); do
    local pid="${SVC_PIDS[i]}" name="${SVC_NAMES[i]}"
    if kill -0 "$pid" 2>/dev/null; then
      log down "  $name (pid $pid)"; kill -TERM "$pid" 2>/dev/null || true
    fi
    rm -f "$LOG_DIR/$name.pid"
  done
  # Give them a moment, then hard-kill any straggler.
  sleep 1
  for (( i=${#SVC_PIDS[@]}-1; i>=0; i-- )); do
    kill -0 "${SVC_PIDS[i]}" 2>/dev/null && kill -KILL "${SVC_PIDS[i]}" 2>/dev/null || true
  done
  if [[ -n "$OLLAMA_PID" ]] && kill -0 "$OLLAMA_PID" 2>/dev/null; then
    log down "  ollama (pid $OLLAMA_PID, we started it)"; kill -TERM "$OLLAMA_PID" 2>/dev/null || true
    rm -f "$LOG_DIR/ollama.pid"
  fi
  log down "done"
}
trap cleanup INT TERM EXIT

# ---------------------------------------------------------------------------
# Preflight: required + recommended tooling
# ---------------------------------------------------------------------------
command -v cargo >/dev/null 2>&1 || die "cargo not found — install Rust toolchain."
for opt in ffmpeg curl; do
  command -v "$opt" >/dev/null 2>&1 || log warn "'$opt' not found — worker/viewer remux + health checks need it."
done

# Port guard: if our ports are already taken, a stack is probably already up.
port_busy() { lsof -ti "tcp:$1" >/dev/null 2>&1; }
for p in 8080 8090 8070; do
  if port_busy "$p"; then
    die "port $p already in use — is the stack already running? Run '$0 --down' first (or free the port)."
  fi
done

# --- Postgres -------------------------------------------------------------
log infra "checking Postgres on :5432…"
if pg_isready -q -h localhost -p 5432 2>/dev/null; then
  log infra "Postgres already up"
else
  log infra "Postgres down — trying 'brew services start postgresql@16'"
  brew services start postgresql@16 >/dev/null 2>&1 \
    || brew services start postgresql >/dev/null 2>&1 \
    || log warn "could not start Postgres via brew — start it yourself."
  for _ in $(seq 1 40); do pg_isready -q -h localhost -p 5432 2>/dev/null && break; sleep 0.5; done
  pg_isready -q -h localhost -p 5432 2>/dev/null || die "Postgres never became ready on :5432."
  log infra "Postgres up"
fi
# DB-exists hint (migrations auto-apply on startup, but the database must exist).
if command -v psql >/dev/null 2>&1; then
  DB_URL="$(grep -E '^DATABASE_URL=' "$REPO_ROOT/.env" 2>/dev/null | head -1 | cut -d= -f2- || true)"
  if [[ -n "$DB_URL" ]] && ! psql "$DB_URL" -tAc 'select 1' >/dev/null 2>&1; then
    log warn "cannot connect to \$DATABASE_URL ($DB_URL). If the DB is missing: createdb hushai"
  fi
fi

# --- Ollama ---------------------------------------------------------------
OLLAMA_API="http://localhost:11434"
log infra "checking Ollama on :11434…"
if curl -sf -o /dev/null --max-time 2 "$OLLAMA_API/api/tags" 2>/dev/null; then
  log infra "Ollama already up"
elif command -v ollama >/dev/null 2>&1; then
  log infra "Ollama down — starting 'ollama serve'"
  ( exec ollama serve ) >"$LOG_DIR/ollama.log" 2>&1 &
  OLLAMA_PID=$!; echo "$OLLAMA_PID" >"$LOG_DIR/ollama.pid"
  for _ in $(seq 1 40); do curl -sf -o /dev/null --max-time 2 "$OLLAMA_API/api/tags" 2>/dev/null && break; sleep 0.5; done
  curl -sf -o /dev/null --max-time 2 "$OLLAMA_API/api/tags" 2>/dev/null || die "Ollama never became ready (see $LOG_DIR/ollama.log)."
  log infra "Ollama up (pid $OLLAMA_PID)"
else
  log warn "ollama not installed — worker embeddings + rag answers will fail."
fi
# Required models for the worker (embeddings) + rag (LLM).
if command -v ollama >/dev/null 2>&1; then
  HAVE_MODELS="$(ollama list 2>/dev/null || true)"
  for m in mxbai-embed-large llama3.2:3b; do
    if ! grep -q "$m" <<<"$HAVE_MODELS"; then
      if [[ "$PULL_MODELS" -eq 1 ]]; then
        log infra "pulling missing model: $m"; ollama pull "$m"
      else
        log warn "Ollama model '$m' not present — run: ollama pull $m   (or re-run with --pull)"
      fi
    fi
  done
fi

# --- Local model files (soft: services self-disable missing optional stages) ---
[[ -f "$REPO_ROOT/models/ggml-base.en.bin" ]] \
  || log warn "models/ggml-base.en.bin missing — worker ASR won't run (see hushai-backend/README)."
ORT_DYLIB="$REPO_ROOT/models/onnxruntime/onnxruntime-osx-arm64-1.20.0/lib/libonnxruntime.1.20.0.dylib"
[[ -f "$ORT_DYLIB" ]] \
  || log warn "ORT dylib missing — vision/objects/clip disabled. Fetch: local_dev/fetch_onnxruntime.sh"
for mf in nemo_en_titanet_large.onnx silero_vad.onnx; do
  [[ -f "$REPO_ROOT/models/$mf" ]] || log warn "models/$mf missing — speaker ID degraded."
done

# ---------------------------------------------------------------------------
# Build once (so the four launches are instant and don't race the compiler)
# ---------------------------------------------------------------------------
if [[ "$DO_BUILD" -eq 1 ]]; then
  log build "cargo build ($PROFILE) -p backend,worker,rag,viewer…"
  ( cd "$REPO_ROOT" && SQLX_OFFLINE=true cargo build ${PROFILE_FLAG[@]+"${PROFILE_FLAG[@]}"} \
      -p hushai-backend -p hushai-worker -p hushai-rag -p hushai-viewer ) \
    || die "build failed."
  log build "ok"
fi
for b in hushai-backend hushai-worker hushai-rag hushai-viewer; do
  [[ -x "$PROFILE_DIR/$b" ]] || die "missing binary $PROFILE_DIR/$b (build first, or drop --no-build)."
done

# ---------------------------------------------------------------------------
# Launch a service: launch <name> <cwd> <binary-name>
#   - runs the compiled binary directly (so the tracked PID *is* the server →
#     clean teardown), with the right CWD for dotenv + relative model/blob paths.
# ---------------------------------------------------------------------------
LAST_PID=""   # pid of the most recently launched service (bash 3.2 has no arr[-1])
# NB: set env via bash `export` + `exec` the binary DIRECTLY — do NOT go through
# `env`/any /usr/bin shim, or SIP strips DYLD_FALLBACK_LIBRARY_PATH and the
# sherpa-linked binaries (worker, rag) die with "@rpath/libonnxruntime.1.17.1.dylib …
# no LC_RPATH's found". (bash sets the var post-launch so it survives; exec'ing the
# unrestricted cargo binary directly passes it through.)
launch() {
  local name="$1" cwd="$2" bin="$3"
  ( cd "$cwd" && export SQLX_OFFLINE=true DYLD_FALLBACK_LIBRARY_PATH="$DYLD_FB" \
      && exec "$PROFILE_DIR/$bin" ) >"$LOG_DIR/$name.log" 2>&1 &
  local pid=$!
  SVC_NAMES+=("$name"); SVC_PIDS+=("$pid"); LAST_PID="$pid"
  echo "$pid" >"$LOG_DIR/$name.pid"
  log up "$name (pid $pid) → $LOG_DIR/$name.log"
}

# Wait for an HTTP endpoint to answer at all (any status code, not just 2xx).
# NB: on a refused connection curl prints "000" AND exits non-zero — so we take the
# code only on curl success, else force "000". (A bare `|| echo 000` double-prints.)
wait_http() {
  local url="$1" name="$2" timeout="${3:-30}" code
  for _ in $(seq 1 $((timeout*2))); do
    code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "$url" 2>/dev/null)" || code="000"
    if [[ -n "$code" && "$code" != "000" ]]; then
      log ok "$name healthy ($url → $code)"; return 0
    fi
    sleep 0.5
  done
  return 1
}

# Confirm a port-less service (the worker) is still alive a moment after launch.
alive_or_report() {
  local name="$1" pid="$2"
  sleep 1
  if ! kill -0 "$pid" 2>/dev/null; then
    log warn "$name exited immediately — last log lines:"; tail -n 15 "$LOG_DIR/$name.log" || true
  fi
}

log boot "starting services ($PROFILE)…"

# 1. backend (everything depends on it) — fatal if it doesn't come up.
launch backend "$REPO_ROOT/hushai-backend" hushai-backend
wait_http "http://localhost:8080/healthz" backend 40 \
  || { log warn "backend never answered :8080/healthz — last log lines:"; tail -n 20 "$LOG_DIR/backend.log" || true; die "backend failed to start."; }

# 2. worker (no port) — drains the queue; degraded-but-ok if a model is missing.
launch worker "$REPO_ROOT" hushai-worker
alive_or_report worker "$LAST_PID"

# 3. rag (:8090) — warn (not fatal) if unhealthy; backend+viewer still useful.
launch rag "$REPO_ROOT" hushai-rag
wait_http "http://localhost:8090/healthz" rag 40 \
  || { log warn "rag never answered :8090/healthz — see $LOG_DIR/rag.log"; }

# 4. viewer (:8070) — the webapp; root path returns the UI.
launch viewer "$REPO_ROOT" hushai-viewer
wait_http "http://127.0.0.1:8070/" viewer 30 \
  || { log warn "viewer never answered :8070 — see $LOG_DIR/viewer.log"; }

# ---------------------------------------------------------------------------
# Optional: best-effort Android capture client (needs a USB phone)
# ---------------------------------------------------------------------------
if [[ "$WITH_ANDROID" -eq 1 ]]; then
  ADB="${ANDROID_HOME:-$HOME/Library/Android/sdk}/platform-tools/adb"
  if [[ -x "$ADB" ]] && [[ -n "$("$ADB" devices | awk 'NR>1 && $2=="device"{print $1}')" ]]; then
    log android "device detected → run_hushai_app.sh ${ANDROID_ARGS[*]:-}"
    ( "$SCRIPT_DIR/run_hushai_app.sh" ${ANDROID_ARGS[@]+"${ANDROID_ARGS[@]}"} ) >"$LOG_DIR/android.log" 2>&1 &
    ANDROID_PID=$!; SVC_NAMES+=("android"); SVC_PIDS+=("$ANDROID_PID")
    echo "$ANDROID_PID" >"$LOG_DIR/android.pid"
    log android "driving phone (pid $ANDROID_PID) → $LOG_DIR/android.log"
  else
    log warn "no authorized USB device — skipping Android (connect a phone + 'adb devices', then re-run, or use run_hushai_app.sh)."
  fi
fi

# ---------------------------------------------------------------------------
# Up. Print the map, then hold the foreground until a service dies or Ctrl-C.
# ---------------------------------------------------------------------------
cat <<EOF

  ┌─ Hushai stack is up ($PROFILE) ───────────────────────────────
  │  webapp (NVR + chat)   →  http://127.0.0.1:8070
  │  rag api               →  http://localhost:8090   (/v1/rag/query, /v1/rag/chat, /v1/tts)
  │  backend ingest        →  http://localhost:8080   (/v1/segments, /v1/speakers, /v1/persons)
  │  worker                →  draining segments (transcribe + embed + speaker + vision)
  │  logs                  →  local_dev/logs/<service>.log
  │  Ctrl-C                →  stop everything cleanly
  └───────────────────────────────────────────────────────────────
EOF

# Liveness poll: exit (→ trap cleanup) if any service dies; Ctrl-C interrupts the sleep.
while true; do
  for i in "${!SVC_PIDS[@]}"; do
    if ! kill -0 "${SVC_PIDS[i]}" 2>/dev/null; then
      log warn "${SVC_NAMES[i]} exited — see $LOG_DIR/${SVC_NAMES[i]}.log"
      tail -n 15 "$LOG_DIR/${SVC_NAMES[i]}.log" 2>/dev/null || true
      exit 1
    fi
  done
  sleep 2
done
