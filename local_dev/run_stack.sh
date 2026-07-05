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
#   ./local_dev/run_stack.sh --tls              # serve all services over HTTPS (auto-gen certs if absent)
#   ./local_dev/run_stack.sh --lan              # expose the viewer on the LAN for https://hushai.local/ (implies
#                                               #   --tls; binds 0.0.0.0 + allowlists this host's IP — run setup_hostname.sh too)
#   ./local_dev/run_stack.sh --add-camera NAME  # onboard a camera: mint+save a per-device token, print
#                                               #   its config card, then exit (see docs/onboarding-a-camera.md)
#   ./local_dev/run_stack.sh --pull             # `ollama pull` any missing models, then continue
#   ./local_dev/run_stack.sh --test-db          # point the stack at hushai_test + the hushai-eval
#                                               #   determinism profile (local_dev/eval.env) for regression runs
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

# Cross-platform adapters (OS detection, Postgres start, LAN-IP detection, port owners).
# shellcheck source=lib_platform.sh
source "$SCRIPT_DIR/lib_platform.sh"
HUSHAI_OS="$(hushai_os)"

# ---------------------------------------------------------------------------
# Args
# ---------------------------------------------------------------------------
WITH_ANDROID=0
DO_BUILD=1
PROFILE="debug"
PULL_MODELS=0
DOWN_ONLY=0
TLS=0
LAN=0
TEST_DB=0
ADD_CAMERA=""
ANDROID_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --with-android) WITH_ANDROID=1; shift ;;
    --no-build)     DO_BUILD=0; shift ;;
    --release)      PROFILE="release"; shift ;;
    --pull)         PULL_MODELS=1; shift ;;
    --tls)          TLS=1; shift ;;
    --lan)          LAN=1; TLS=1; shift ;;
    --test-db)      TEST_DB=1; shift ;;
    --add-camera)   ADD_CAMERA="${2:-}"; shift 2 || shift ;;
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
# --add-camera NAME: onboard a new capture client (camera). Mints a per-device
# bearer token, persists `NAME:token` into hushai-backend/.env's DEVICE_TOKENS,
# and prints the config card. Full runbook: docs/onboarding-a-camera.md.
# ---------------------------------------------------------------------------
detect_lan_ip() { hushai_first_lan_ip; }

add_camera() {
  local label="$1"
  local backend_env="$REPO_ROOT/hushai-backend/.env"
  local cert_dir="$SCRIPT_DIR/certs"
  [[ -n "$label" ]] || die "--add-camera needs a label, e.g. --add-camera garage-cam"
  [[ "$label" == *[:,]* ]] && die "camera label must not contain ':' or ',' (got '$label')"
  command -v openssl >/dev/null 2>&1 || die "--add-camera needs openssl to mint a token."
  [[ -f "$backend_env" ]] || die "missing $backend_env (copy hushai-backend/.env.example to it first)."

  local token; token="$(openssl rand -hex 32)"

  # Read the current DEVICE_TOKENS (if any). When first creating it, seed an `admin`
  # entry from the single DEVICE_TOKEN so the viewer proxy (BACKEND_TOKEN defaults to
  # DEVICE_TOKEN) keeps authenticating once per-device tokens supersede the single one.
  local current; current="$(grep -E '^DEVICE_TOKENS=' "$backend_env" | head -1 | cut -d= -f2- || true)"
  if [[ -z "$current" ]]; then
    local devtok; devtok="$(grep -E '^DEVICE_TOKEN=' "$backend_env" | head -1 | cut -d= -f2- || true)"
    [[ -n "$devtok" ]] && current="admin:$devtok"
  elif grep -qE "(^|,)${label}:" <<<"$current"; then
    die "a camera labelled '$label' already exists in DEVICE_TOKENS — pick another name or edit $backend_env."
  fi
  local updated; if [[ -n "$current" ]]; then updated="$current,$label:$token"; else updated="$label:$token"; fi

  # Rewrite hushai-backend/.env: drop any existing DEVICE_TOKENS line, append the new one.
  local tmp; tmp="$(mktemp)"
  grep -vE '^DEVICE_TOKENS=' "$backend_env" > "$tmp" || true
  echo "DEVICE_TOKENS=$updated" >> "$tmp"
  mv "$tmp" "$backend_env"

  local lan; lan="$(detect_lan_ip)"; [[ -n "$lan" ]] || lan="<this-host-LAN-IP>"
  local scheme="http"; [[ -f "$cert_dir/server.fullchain.crt" ]] && scheme="https"
  local fp="(no CA yet — run ./local_dev/gen_certs.sh)"
  [[ -f "$cert_dir/ca.crt" ]] && fp="$(openssl x509 -in "$cert_dir/ca.crt" -noout -fingerprint -sha256 2>/dev/null | cut -d= -f2)"

  cat <<EOF

  ┌─ Camera onboarded: $label ─────────────────────────────────────
  │  device token   →  $token
  │  backend ingest →  $scheme://$lan:8080   (POST /v1/segments, Bearer <token>)
  │  rag/assistant  →  $scheme://$lan:8090
  │  saved to       →  $backend_env  (DEVICE_TOKENS)
  │  LAN CA (trust) →  $cert_dir/ca.crt
  │  CA SHA-256     →  $fp
  └────────────────────────────────────────────────────────────────

  Next (full runbook: docs/onboarding-a-camera.md):
   1. (Re)start the backend so the token is live:   ./local_dev/run_stack.sh --tls
   2. Android over USB (debug/cleartext):
        ./local_dev/run_hushai_app.sh --token $token --rag-token "\${RAG_TOKEN:-}"
   3. Android over Wi-Fi (release/HTTPS): bundle the CA, build, set URL+token in-app:
        cp $cert_dir/ca.crt hushai-android/app/src/release/res/raw/hushai_lan_ca.pem
        (cd hushai-android && ./gradlew assembleRelease)   # then URL=$scheme://$lan:8080
   4. Any other conforming client: send 'Authorization: Bearer $token' over $scheme,
        trusting the CA above (e.g. feed_segments.py --cacert $cert_dir/ca.crt).
EOF
}

if [[ -n "$ADD_CAMERA" ]]; then
  add_camera "$ADD_CAMERA"
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

# Self-heal on start: a previous run may have left our services (or orphans whose pid
# files were lost) holding 8080/8090/8070. Instead of refusing to start, tear down our
# OWN prior stack first, then only give up if something UNRELATED still holds a port.
# (This is why a plain re-run — or onboard.sh — is always safe: no manual `--down`.)
is_ours() {  # does this command line belong to a Hushai stack binary we launched?
  case "$1" in
    *"$PROFILE_DIR/"*|*"$REPO_ROOT/target/"*|\
    *hushai-backend*|*hushai-worker*|*hushai-rag*|*hushai-viewer*) return 0 ;;
    *) return 1 ;;
  esac
}
reclaim_ports() {
  local p pid cmd killed=0 foreign=""
  # 1. TERM everything tracked from the last run (pid files under logs/).
  stop_from_pidfiles
  # 2. TERM any still-listening process on our ports that is one of OUR binaries.
  for p in 8080 8090 8070; do
    for pid in $(hushai_pids_on_port "$p"); do
      cmd="$(hushai_pid_command "$pid")"
      if is_ours "$cmd"; then
        log down "reclaiming :$p from stale hushai process (pid $pid)"
        kill -TERM "$pid" 2>/dev/null || true; killed=1
      fi
    done
  done
  if [[ "$killed" -eq 1 ]]; then sleep 1; fi
  # 3. Hard-kill any of ours that ignored TERM.
  for p in 8080 8090 8070; do
    for pid in $(hushai_pids_on_port "$p"); do
      cmd="$(hushai_pid_command "$pid")"
      if is_ours "$cmd"; then kill -KILL "$pid" 2>/dev/null || true; fi
    done
  done
  # 4. Anything still listening is NOT ours → refuse, naming the offender.
  for p in 8080 8090 8070; do
    for pid in $(hushai_pids_on_port "$p"); do
      foreign="$foreign  :$p pid $pid ($(hushai_pid_command "$pid" | awk '{print $1}'))"
    done
  done
  if [[ -n "$foreign" ]]; then
    die "port(s) held by a non-Hushai process:$foreign"$'\n'"Free them (or stop that app) and re-run."
  fi
  return 0   # NB: explicit — a bare trailing `&&` would return non-zero and trip `set -e`.
}
reclaim_ports

# ---------------------------------------------------------------------------
# Security: TLS (optional) + admin/rag credentials (always on, dev defaults).
# Vars exported here are inherited by every `launch` subshell → the services pick
# them up. dotenvy does NOT override a var already in the environment, so these win.
# ---------------------------------------------------------------------------
CERT_DIR="$SCRIPT_DIR/certs"
SCHEME="http"
CURL_CACERT=()
if [[ "$TLS" -eq 1 ]]; then
  command -v openssl >/dev/null 2>&1 || die "--tls needs openssl (for gen_certs.sh)."
  if [[ ! -f "$CERT_DIR/server.fullchain.crt" || ! -f "$CERT_DIR/server.pkcs8.key" ]]; then
    log infra "TLS requested but certs missing — generating (local_dev/gen_certs.sh)…"
    "$SCRIPT_DIR/gen_certs.sh" >/dev/null || die "gen_certs.sh failed."
  fi
  SCHEME="https"
  CURL_CACERT=(--cacert "$CERT_DIR/ca.crt")
  export TLS_CERT_PATH="$CERT_DIR/server.fullchain.crt"
  export TLS_KEY_PATH="$CERT_DIR/server.pkcs8.key"
  export VIEWER_COOKIE_SECURE=true
  # The siblings now serve https, so the viewer must proxy/probe them over https and
  # trust the LAN CA (its reqwest client otherwise rejects the self-signed cert).
  export RAG_BASE_URL="https://127.0.0.1:8090"
  export BACKEND_BASE_URL="https://127.0.0.1:8080"
  export VIEWER_UPSTREAM_CA="$CERT_DIR/ca.crt"
  log infra "TLS on — all services serve https (trust CA: $CERT_DIR/ca.crt)"
fi

# --lan: expose the viewer on the LAN so https://hushai.local/ works (run setup_hostname.sh
# for the Bonjour name + the 443->8070 redirect). Safe because every viewer route is gated by
# the IP allowlist + password. We bind 0.0.0.0 and allowlist THIS host's LAN IP(s): a host
# hitting its own hushai.local presents its LAN IP (not loopback). Add other admin machines'
# IPs by exporting VIEWER_ADMIN_IP_ALLOWLIST yourself (it wins — we only fill it when unset).
if [[ "$LAN" -eq 1 ]]; then
  export VIEWER_BIND_ADDR="0.0.0.0:8070"
  export VIEWER_HOSTNAME="hushai.local"   # cosmetic: the viewer's listening-log URL
  if [[ -z "${VIEWER_ADMIN_IP_ALLOWLIST:-}" ]]; then
    VIEWER_ADMIN_IP_ALLOWLIST="$(hushai_lan_ips | tr ' ' ',' | sed 's/,$//')"
    export VIEWER_ADMIN_IP_ALLOWLIST
  fi
  log infra "LAN mode — viewer binds 0.0.0.0:8070; admin IP allowlist: ${VIEWER_ADMIN_IP_ALLOWLIST:-<none detected>} (+loopback)"
fi

# rag bearer so rag isn't world-open on 0.0.0.0:8090 (the viewer proxy + phone present it).
if [[ -z "${RAG_TOKEN:-}" ]]; then
  if command -v openssl >/dev/null 2>&1; then RAG_TOKEN="$(openssl rand -hex 32)"; else RAG_TOKEN="dev-rag-token"; fi
fi
export RAG_TOKEN
# viewer admin password (the viewer IS the admin panel; loopback is always allowed).
: "${VIEWER_ADMIN_PASSWORD:=hushai-dev}"
export VIEWER_ADMIN_PASSWORD
# Stable session secret so logins survive restarts (persisted under logs/, gitignored).
SECRET_FILE="$LOG_DIR/session_secret"
if [[ -z "${VIEWER_SESSION_SECRET:-}" ]]; then
  if [[ -f "$SECRET_FILE" ]]; then
    VIEWER_SESSION_SECRET="$(cat "$SECRET_FILE")"
  elif command -v openssl >/dev/null 2>&1; then
    VIEWER_SESSION_SECRET="$(openssl rand -hex 32)"; echo "$VIEWER_SESSION_SECRET" >"$SECRET_FILE"
  else
    VIEWER_SESSION_SECRET="dev-session-secret-change-me-0123456789"
  fi
fi
export VIEWER_SESSION_SECRET

# --- Postgres -------------------------------------------------------------
log infra "checking Postgres on :5432…"
if pg_isready -q -h localhost -p 5432 2>/dev/null; then
  log infra "Postgres already up"
else
  log infra "Postgres down — attempting to start it ($HUSHAI_OS)…"
  hushai_pg_start || log warn "could not auto-start Postgres — start it yourself."
  for _ in $(seq 1 40); do pg_isready -q -h localhost -p 5432 2>/dev/null && break; sleep 0.5; done
  pg_isready -q -h localhost -p 5432 2>/dev/null || die "Postgres never became ready on :5432."
  log infra "Postgres up"
fi
# --test-db: source the hushai-eval determinism profile (hushai_test DB + locked-down worker knobs)
# BEFORE launching. The `launch` subshells inherit these exports; services use dotenvy, which does
# NOT override already-set env, so eval.env's DATABASE_URL=…/hushai_test wins over the dev .env.
if [[ "$TEST_DB" -eq 1 ]]; then
  [[ -f "$SCRIPT_DIR/eval.env" ]] || die "missing $SCRIPT_DIR/eval.env (the eval determinism profile)."
  log boot "eval mode: sourcing eval.env (hushai_test DB + determinism lockdown)"
  set -a; source "$SCRIPT_DIR/eval.env"; set +a
fi
# DB-exists hint (migrations auto-apply on startup, but the database must exist).
if command -v psql >/dev/null 2>&1; then
  DB_URL="${DATABASE_URL:-$(grep -E '^DATABASE_URL=' "$REPO_ROOT/.env" 2>/dev/null | head -1 | cut -d= -f2- || true)}"
  if [[ -n "$DB_URL" ]] && ! psql "$DB_URL" -tAc 'select 1' >/dev/null 2>&1; then
    log warn "cannot connect to \$DATABASE_URL ($DB_URL). If missing: createdb $(basename "$DB_URL")"
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
# Required models: worker embeddings (mxbai-embed-large) + sentiment (llama3.2:3b) + rag answer
# generation (qwen2.5:7b — the faithful attribution model; see RAG_LLM_MODEL).
if command -v ollama >/dev/null 2>&1; then
  HAVE_MODELS="$(ollama list 2>/dev/null || true)"
  for m in mxbai-embed-large llama3.2:3b qwen2.5:7b; do
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
  # Export the OS-appropriate dynamic-linker search path (DYLD_* on macOS — see the SIP
  # note above; LD_LIBRARY_PATH on Linux) so sherpa's bundled onnxruntime resolves.
  local libvar; libvar="$(hushai_lib_path_var)"
  ( cd "$cwd" && export SQLX_OFFLINE=true "$libvar=$DYLD_FB" \
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
    code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 ${CURL_CACERT[@]+"${CURL_CACERT[@]}"} "$url" 2>/dev/null)" || code="000"
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
wait_http "$SCHEME://localhost:8080/healthz" backend 40 \
  || { log warn "backend never answered :8080/healthz — last log lines:"; tail -n 20 "$LOG_DIR/backend.log" || true; die "backend failed to start."; }

# 2. worker (no port) — drains the queue; degraded-but-ok if a model is missing.
launch worker "$REPO_ROOT" hushai-worker
alive_or_report worker "$LAST_PID"

# 3. rag (:8090) — warn (not fatal) if unhealthy; backend+viewer still useful.
launch rag "$REPO_ROOT" hushai-rag
wait_http "$SCHEME://localhost:8090/healthz" rag 40 \
  || { log warn "rag never answered :8090/healthz — see $LOG_DIR/rag.log"; }

# 4. viewer (:8070) — the webapp; /healthz is open (the app routes are IP+password gated).
launch viewer "$REPO_ROOT" hushai-viewer
wait_http "$SCHEME://127.0.0.1:8070/healthz" viewer 30 \
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
WEBAPP_URL="$SCHEME://127.0.0.1:8070"
[[ "$LAN" -eq 1 ]] && WEBAPP_URL="https://hushai.local/  (no-port; after setup_hostname.sh) · $SCHEME://127.0.0.1:8070"
cat <<EOF

  ┌─ Hushai stack is up ($PROFILE${TLS:+, TLS}${LAN:+, LAN}) ──────────────────────────
  │  webapp (NVR + chat)   →  $WEBAPP_URL   (admin: IP-allowlist + password)
  │  rag api               →  $SCHEME://localhost:8090   (/v1/rag/query, /v1/rag/chat, /v1/tts — RAG_TOKEN required)
  │  backend ingest        →  $SCHEME://localhost:8080   (/v1/segments, /v1/speakers, /v1/persons)
  │  worker                →  draining segments (transcribe + embed + speaker + vision)
  │  admin login           →  password "$VIEWER_ADMIN_PASSWORD"  (set VIEWER_ADMIN_PASSWORD to change)
  │  rag token             →  $RAG_TOKEN
  │  logs                  →  local_dev/logs/<service>.log
  │  Ctrl-C                →  stop everything cleanly
  └───────────────────────────────────────────────────────────────
EOF
if [[ "$TLS" -eq 1 ]]; then
  echo "  TLS: trust the CA once for browsers — sudo security add-trusted-cert -d -r trustRoot \\"
  echo "       -k /Library/Keychains/System.keychain $CERT_DIR/ca.crt"
fi
if [[ "$LAN" -eq 1 ]]; then
  echo "  LAN: for the no-port name https://hushai.local/ run once — ./local_dev/setup_hostname.sh"
fi

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
