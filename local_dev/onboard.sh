#!/usr/bin/env bash
#
# onboard.sh — ONE interactive command to stand Hushai up on a fresh machine.
#
# Takes a clean checkout to: dependencies installed → Postgres + Ollama up → models
# provisioned → .env written → per-device tokens minted → the whole stack running →
# your plugged-in device(s) streaming live video → the viewer/backend reachable.
#
# It ASKS what you need (how many devices, USB or WiFi, which AI features), then DOES it.
# Every system change is confirmed first. Safe to re-run — it skips whatever's already done.
#
#   ./local_dev/onboard.sh
#
# macOS (Apple Silicon) is the tested path; Linux (apt/dnf) is supported best-effort.
# Reuses the existing local_dev scripts (run_stack.sh, gen_certs.sh, run_hushai_app.sh,
# setup_hostname.sh, fetch_*.sh) rather than reimplementing them.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOG_DIR="$SCRIPT_DIR/logs"
CERT_DIR="$SCRIPT_DIR/certs"
BACKEND_ENV="$REPO_ROOT/hushai-backend/.env"
ROOT_ENV="$REPO_ROOT/.env"
MODELS_DIR="$REPO_ROOT/models"
WHISPER_MODEL="$MODELS_DIR/ggml-base.en.bin"
WHISPER_URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin"

# shellcheck source=lib_platform.sh
source "$SCRIPT_DIR/lib_platform.sh"
OS="$(hushai_os)"
mkdir -p "$LOG_DIR" "$MODELS_DIR"

# ---- pretty output ---------------------------------------------------------
if [[ -t 1 ]]; then B=$'\033[1m'; DIM=$'\033[2m'; GRN=$'\033[32m'; YEL=$'\033[33m'; RED=$'\033[31m'; RST=$'\033[0m'
else B=""; DIM=""; GRN=""; YEL=""; RED=""; RST=""; fi
say()  { echo "${B}▸${RST} $*"; }
ok()   { echo "  ${GRN}✓${RST} $*"; }
warn() { echo "  ${YEL}!${RST} $*" >&2; }
err()  { echo "${RED}✗${RST} $*" >&2; }
die()  { err "$*"; exit 1; }
hr()   { echo "${DIM}────────────────────────────────────────────────────────────${RST}"; }

# ---- prompts ---------------------------------------------------------------
ask() {  # ask "question" "default" -> echoes the answer (or default)
  local q="$1" def="${2:-}" ans
  if [[ -n "$def" ]]; then read -r -p "  $q [$def]: " ans || true; echo "${ans:-$def}"
  else read -r -p "  $q: " ans || true; echo "$ans"; fi
}
ask_yn() {  # ask_yn "question" "y|n" -> return 0 for yes
  local q="$1" def="${2:-y}" ans hint="[Y/n]"
  [[ "$def" == "n" ]] && hint="[y/N]"
  read -r -p "  $q $hint " ans || true; ans="${ans:-$def}"
  case "$ans" in [Yy]*) return 0 ;; *) return 1 ;; esac
}

# ---- dependency doctor -----------------------------------------------------
# need <display> <probe-cmd> <brew> <apt> <dnf>  — check, and offer to auto-install.
need() {
  local display="$1" probe="$2" brew_f="$3" apt_p="$4" dnf_p="$5"
  if command -v "$probe" >/dev/null 2>&1; then ok "$display present"; return 0; fi
  warn "$display not found."
  if ask_yn "install $display now?" y; then
    hushai_pkg_install "$display" "$brew_f" "$apt_p" "$dnf_p" \
      && ok "$display installed" || { warn "auto-install failed — install $display manually."; return 1; }
  else
    warn "skipped $display — some steps may fail."
    return 1
  fi
}

# ---- .env helpers ----------------------------------------------------------
env_get() { grep -E "^$2=" "$1" 2>/dev/null | head -1 | cut -d= -f2- || true; }
ensure_kv() {  # ensure_kv <file> <KEY> <value> — set only if KEY is absent/empty
  local file="$1" key="$2" val="$3" tmp
  touch "$file"
  if grep -qE "^${key}=.+" "$file" 2>/dev/null; then return 0; fi
  tmp="$(mktemp)"; grep -vE "^${key}=" "$file" 2>/dev/null > "$tmp" || true
  echo "${key}=${val}" >> "$tmp"; mv "$tmp" "$file"
  ok "set ${key} in ${file#$REPO_ROOT/}"
}
# token_for <name> — read the minted per-device token back out of DEVICE_TOKENS.
token_for() {
  awk -v n="$1" -F= '/^DEVICE_TOKENS=/{
    s=substr($0,index($0,"=")+1); m=split(s,a,",");
    for(i=1;i<=m;i++){ if(index(a[i], n":")==1){ print substr(a[i], length(n)+2); break } } }' "$BACKEND_ENV"
}

# ===========================================================================
echo
echo "${B}Hushai onboarding${RST} ${DIM}($OS/$(hushai_arch))${RST}"
hr
[[ "$OS" == "unknown" ]] && die "Unsupported OS. macOS + Linux only (Windows: docs/friendly-url-windows.md)."
[[ -f "$REPO_ROOT/Cargo.toml" ]] || die "run me from inside the hushai checkout (Cargo.toml not found at $REPO_ROOT)."

# --- 1. What do you want to connect? ---------------------------------------
say "Devices"
NDEV="$(ask "How many devices do you want to connect now?" "1")"
[[ "$NDEV" =~ ^[0-9]+$ ]] || die "expected a number, got '$NDEV'."
DEV_KIND=(); DEV_NAME=()
ANY_USB=0; ANY_WIFI=0; ANY_IPCAM=0
i=1
while [[ "$i" -le "$NDEV" ]]; do
  echo "  Device $i — how does it connect?"
  echo "    1) Android phone over USB  (plug in the cable; simplest, no network)"
  echo "    2) Android phone over WiFi (release build over the LAN, HTTPS)"
  echo "    3) IP camera / other client (POSTs segments with a token over the LAN)"
  echo "    4) Browser capture         (this laptop's camera via the viewer, no token)"
  k="$(ask "choice for device $i" "1")"
  case "$k" in
    1) DEV_KIND+=("usb");     ANY_USB=1 ;;
    2) DEV_KIND+=("wifi");    ANY_WIFI=1 ;;
    3) DEV_KIND+=("ipcam");   ANY_IPCAM=1 ;;
    4) DEV_KIND+=("browser"); DEV_NAME+=("browser-$i"); i=$((i+1)); continue ;;
    *) warn "unrecognized choice '$k' — treating as USB Android."; DEV_KIND+=("usb"); ANY_USB=1 ;;
  esac
  dn="$(ask "  a short name for device $i (e.g. front-door, my-phone)" "cam-$i")"
  DEV_NAME+=("$dn")
  i=$((i+1))
done

# Access mode: any networked device forces LAN (bind 0.0.0.0 + HTTPS); else ask.
ACCESS="local"
if [[ "$ANY_WIFI" -eq 1 || "$ANY_IPCAM" -eq 1 ]]; then
  ACCESS="lan"
  say "A WiFi/IP device is present → the stack will bind to the LAN over HTTPS."
else
  echo
  say "Access mode"
  echo "    1) Local only  — reachable at http://127.0.0.1:8070 on this machine"
  echo "    2) LAN         — reachable by other machines/phones (HTTPS + admin gate)"
  [[ "$(ask "choice" "1")" == "2" ]] && ACCESS="lan"
fi
LAN=0; if [[ "$ACCESS" == "lan" ]]; then LAN=1; fi

# --- 2. Dependency doctor (+ auto-install, confirmed) ----------------------
echo; say "Checking dependencies"
if ! command -v cargo >/dev/null 2>&1; then
  die "Rust/cargo not found. Install it, then re-run:
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi
ok "cargo present"
need "ffmpeg"  ffmpeg  ffmpeg  ffmpeg  ffmpeg  || true
need "openssl" openssl openssl openssl openssl || true
need "PostgreSQL client" psql postgresql@16 postgresql postgresql || true
[[ -d /opt/homebrew/opt/postgresql@16/bin ]] && PATH="/opt/homebrew/opt/postgresql@16/bin:$PATH"
# Ollama has its own installer on Linux (no apt/dnf package).
if command -v ollama >/dev/null 2>&1; then ok "ollama present"
elif ask_yn "install Ollama (local embeddings + LLM)?" y; then
  if [[ "$OS" == "macos" ]]; then hushai_pkg_install "ollama" ollama ollama ollama || warn "install ollama manually."
  else say "running the official Ollama installer…"; curl -fsSL https://ollama.com/install.sh | sh || warn "ollama install failed."; fi
else warn "skipped ollama — embeddings + RAG answers will not work."; fi
if [[ "$ANY_USB" -eq 1 ]]; then
  need "adb (android platform-tools)" adb android-platform-tools android-tools-adb android-tools || true
  if ! { [[ -n "${JAVA_HOME:-}" ]] || command -v javac >/dev/null 2>&1; }; then
    need "JDK 17 (to build the Android app)" javac openjdk@17 openjdk-17-jdk java-17-openjdk-devel || true
  fi
fi

# --- 3. Infra: Postgres + database, Ollama ---------------------------------
echo; say "Infrastructure"
if pg_isready -q -h localhost -p 5432 2>/dev/null; then ok "Postgres up on :5432"
else
  say "starting Postgres…"; hushai_pg_start || warn "could not auto-start Postgres."
  for _ in $(seq 1 40); do pg_isready -q -h localhost -p 5432 2>/dev/null && break; sleep 0.5; done
  pg_isready -q -h localhost -p 5432 2>/dev/null && ok "Postgres up" || warn "Postgres still down — start it, then re-run."
fi
DB_URL_DEFAULT="postgres://localhost/hushai"
DB_URL="$(env_get "$ROOT_ENV" DATABASE_URL)"; DB_URL="${DB_URL:-$DB_URL_DEFAULT}"
if command -v psql >/dev/null 2>&1; then
  if psql "$DB_URL" -tAc 'select 1' >/dev/null 2>&1; then ok "database reachable"
  else
    say "creating database 'hushai'…"
    if createdb hushai >/dev/null 2>&1; then ok "created database hushai"
    else warn "createdb failed. On Linux you may need: sudo -u postgres createdb -O \$USER hushai (and a role for \$USER)."; fi
  fi
fi
if command -v ollama >/dev/null 2>&1; then
  curl -sf -o /dev/null --max-time 2 http://localhost:11434/api/tags 2>/dev/null \
    || { say "starting 'ollama serve'…"; ( exec ollama serve ) >"$LOG_DIR/ollama.log" 2>&1 & echo $! >"$LOG_DIR/ollama.pid"
         for _ in $(seq 1 40); do curl -sf -o /dev/null --max-time 2 http://localhost:11434/api/tags 2>/dev/null && break; sleep 0.5; done; }
  curl -sf -o /dev/null --max-time 2 http://localhost:11434/api/tags 2>/dev/null && ok "Ollama up on :11434" || warn "Ollama not responding."
fi

# --- 4. .env files (authoritative, non-destructive) ------------------------
echo; say "Configuration (.env)"
if [[ ! -f "$BACKEND_ENV" ]]; then
  cp "$REPO_ROOT/hushai-backend/.env.example" "$BACKEND_ENV"; ok "created hushai-backend/.env from example"
fi
ensure_kv "$BACKEND_ENV" DATABASE_URL "$DB_URL"
ensure_kv "$BACKEND_ENV" BLOB_DIR "./data"
if [[ -z "$(env_get "$BACKEND_ENV" DEVICE_TOKEN)" ]]; then
  ensure_kv "$BACKEND_ENV" DEVICE_TOKEN "$(openssl rand -hex 32 2>/dev/null || echo dev-secret-token)"
fi
DEVICE_TOKEN="$(env_get "$BACKEND_ENV" DEVICE_TOKEN)"
# Root .env feeds worker/rag/viewer (CWD=repo root). BLOB_DIR must resolve to the same
# physical dir the backend uses (hushai-backend/data); DEVICE_TOKEN must match backend.
ensure_kv "$ROOT_ENV" DATABASE_URL "$DB_URL"
ensure_kv "$ROOT_ENV" BLOB_DIR "./hushai-backend/data"
ensure_kv "$ROOT_ENV" DEVICE_TOKEN "$DEVICE_TOKEN"
if [[ -z "$(env_get "$ROOT_ENV" VIEWER_ADMIN_PASSWORD)" ]]; then
  ADMIN_PW="$(ask "choose a viewer admin password" "hushai-$(openssl rand -hex 3 2>/dev/null || echo dev)")"
  ensure_kv "$ROOT_ENV" VIEWER_ADMIN_PASSWORD "$ADMIN_PW"
fi
ADMIN_PW="$(env_get "$ROOT_ENV" VIEWER_ADMIN_PASSWORD)"

# --- 5. AI lanes (minimal default; opt-in the rest) ------------------------
echo; say "AI features"
say "streaming + transcription are always set up; the rest are optional (large downloads)."
# Whisper (transcription) — no fetch script exists for it, so we curl it here.
if [[ -f "$WHISPER_MODEL" ]]; then ok "whisper model present"
elif ask_yn "download the whisper transcription model (~150MB)?" y; then
  say "downloading ggml-base.en.bin…"
  curl -fL --progress-bar -o "$WHISPER_MODEL" "$WHISPER_URL" && ok "whisper model ready" || warn "download failed — transcription will be off."
fi
pull_model() { command -v ollama >/dev/null 2>&1 || return 0
  ollama list 2>/dev/null | grep -q "$1" && { ok "ollama model $1 present"; return 0; }
  say "pulling ollama model $1…"; ollama pull "$1" && ok "$1 ready" || warn "pull of $1 failed."; }
pull_model mxbai-embed-large   # embeddings — needed for search/RAG grounding
RAG_LANE=0
if ask_yn "enable RAG chat + reflection (answers over your history)?" n; then RAG_LANE=1; pull_model qwen2.5:7b; pull_model llama3.2:3b; fi
if ask_yn "enable speaker identification (who is talking)?" n; then "$SCRIPT_DIR/fetch_vad_model.sh" || warn "VAD fetch failed."
  [[ -f "$MODELS_DIR/nemo_en_titanet_large.onnx" ]] || warn "speaker-ID also needs models/nemo_en_titanet_large.onnx (provide it manually)."; fi
if ask_yn "enable vision (faces / objects / image search)?" n; then
  "$SCRIPT_DIR/fetch_onnxruntime.sh" || warn "onnxruntime fetch failed."
  "$SCRIPT_DIR/fetch_scrfd.sh" || warn "scrfd fetch failed (non-commercial license)."
  "$SCRIPT_DIR/provision_vision.sh" || warn "vision export failed — see output above."; fi
if ask_yn "enable license-plate recognition (ALPR)?" n; then "$SCRIPT_DIR/fetch_plate_detector.sh" || warn "plate detector fetch failed."
  warn "ALPR OCR also comes from provision_vision.sh (the vision lane)."; fi
VOICE_LANE=0
if ask_yn "enable the on-device voice assistant on the phone (wake word + Q&A)?" n; then VOICE_LANE=1
  "$SCRIPT_DIR/fetch_vosk_models.sh" || warn "vosk model fetch failed."; pull_model qwen2.5:7b; RAG_LANE=1; fi
# A stable RAG token so the phone/voice assistant can authenticate (rag binds 0.0.0.0).
if [[ "$RAG_LANE" -eq 1 || "$VOICE_LANE" -eq 1 ]]; then
  [[ -z "$(env_get "$ROOT_ENV" RAG_TOKEN)" ]] && ensure_kv "$ROOT_ENV" RAG_TOKEN "$(openssl rand -hex 32 2>/dev/null || echo dev-rag-token)"
fi
RAG_TOKEN="$(env_get "$ROOT_ENV" RAG_TOKEN)"

# --- 6. TLS / LAN prep (foreground, so any sudo prompt has a terminal) ------
if [[ "$LAN" -eq 1 ]]; then
  echo; say "LAN / HTTPS setup"
  [[ -f "$CERT_DIR/ca.crt" ]] || { say "generating local CA + TLS cert…"; "$SCRIPT_DIR/gen_certs.sh" >/dev/null && ok "certs ready" || warn "gen_certs failed."; }
  if ask_yn "trust the local CA on THIS machine (viewer loads without warnings)?" y; then
    # No output redirect: the sudo password prompt must be visible on the terminal.
    hushai_trust_ca "$CERT_DIR/ca.crt" && ok "CA trusted" || warn "CA trust failed (non-fatal)."
  fi
  if [[ "$OS" == "macos" ]] && ask_yn "set up the friendly URL https://hushai.local/ (Bonjour + 443→8070, needs sudo)?" y; then
    "$SCRIPT_DIR/setup_hostname.sh" && ok "hushai.local configured" || warn "setup_hostname failed — use https://<LAN-IP>:8070 instead."
  fi
fi

# --- 7. Mint per-device tokens (reuses run_stack.sh --add-camera) ----------
echo; say "Minting device tokens"
idx=0
for kind in ${DEV_KIND[@]+"${DEV_KIND[@]}"}; do
  name="${DEV_NAME[$idx]}"
  if [[ "$kind" != "browser" ]]; then
    if [[ -n "$(token_for "$name")" ]]; then ok "$name already has a token"
    else "$SCRIPT_DIR/run_stack.sh" --add-camera "$name" >/dev/null 2>&1 && ok "minted token for $name" || warn "could not mint token for $name"; fi
  fi
  idx=$((idx+1))
done

# --- 8. Launch the stack (background; we hold the foreground at the end) ----
echo; say "Launching the stack"
STACK_ARGS=(); [[ "$LAN" -eq 1 ]] && STACK_ARGS+=(--lan)
STACK_LOG="$LOG_DIR/onboard-stack.log"
"$SCRIPT_DIR/run_stack.sh" ${STACK_ARGS[@]+"${STACK_ARGS[@]}"} >"$STACK_LOG" 2>&1 &
STACK_PID=$!
shutdown() { echo; say "stopping the stack…"; kill -TERM "$STACK_PID" 2>/dev/null || true
             "$SCRIPT_DIR/run_stack.sh" --down >/dev/null 2>&1 || true; }
trap shutdown INT TERM
SCHEME="http"; [[ "$LAN" -eq 1 ]] && SCHEME="https"
CURL_K=(); [[ "$LAN" -eq 1 ]] && CURL_K=(--cacert "$CERT_DIR/ca.crt")
say "waiting for the viewer to come up (logs: ${STACK_LOG#$REPO_ROOT/})…"
UP=0
for _ in $(seq 1 120); do
  kill -0 "$STACK_PID" 2>/dev/null || { err "the stack exited during startup — last lines:"; tail -n 20 "$STACK_LOG"; exit 1; }
  code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 ${CURL_K[@]+"${CURL_K[@]}"} "$SCHEME://127.0.0.1:8070/healthz" 2>/dev/null)" || code="000"
  [[ "$code" != "000" ]] && { UP=1; break; }
  sleep 1
done
[[ "$UP" -eq 1 ]] && ok "stack is up" || warn "viewer didn't answer yet — check $STACK_LOG"

# --- 9. Connect the plugged-in USB phones now ------------------------------
if [[ "$ANY_USB" -eq 1 ]]; then
  echo; say "Connecting USB device(s)"
  echo "  ${DIM}Plug the phone in, enable USB debugging, and tap 'Allow' on the RSA prompt.${RST}"
  idx=0
  for kind in ${DEV_KIND[@]+"${DEV_KIND[@]}"}; do
    if [[ "$kind" == "usb" ]]; then
      name="${DEV_NAME[$idx]}"; tok="$(token_for "$name")"; tok="${tok:-$DEVICE_TOKEN}"
      say "building + launching the app for '$name' (streams over the cable)…"
      APP_ARGS=(--token "$tok" --duration 25)
      [[ "$VOICE_LANE" -eq 1 && -n "$RAG_TOKEN" ]] && APP_ARGS+=(--rag-token "$RAG_TOKEN")
      "$SCRIPT_DIR/run_hushai_app.sh" ${APP_ARGS[@]+"${APP_ARGS[@]}"} \
        && ok "'$name' launched — it keeps streaming after this script" \
        || warn "couldn't drive '$name' (device connected + authorized?). Run later: ./local_dev/run_hushai_app.sh --token $tok"
    fi
    idx=$((idx+1))
  done
fi

# --- 10. Summary ("here's everything you need") ----------------------------
LAN_IP="$(hushai_first_lan_ip || true)"
echo; hr
echo "${B}${GRN}Hushai is running.${RST}"
hr
echo "${B}Viewer (NVR + chat):${RST}"
echo "   local:  http://127.0.0.1:8070/"
if [[ "$LAN" -eq 1 ]]; then
  [[ "$OS" == "macos" ]] && echo "   friendly: https://hushai.local/   (if hushai.local setup succeeded)"
  echo "   LAN:    https://${LAN_IP:-<LAN-IP>}:8070/"
fi
echo "   admin password: ${B}${ADMIN_PW}${RST}"
echo
echo "${B}Backend ingest:${RST} ${SCHEME}://${LAN_IP:-127.0.0.1}:8080  (POST /v1/segments, Bearer <token>)"
[[ "$RAG_LANE" -eq 1 || "$VOICE_LANE" -eq 1 ]] && echo "${B}RAG / assistant:${RST} ${SCHEME}://${LAN_IP:-127.0.0.1}:8090   token: ${RAG_TOKEN:-<none>}"
if [[ "$LAN" -eq 1 && -f "$CERT_DIR/ca.crt" ]]; then
  echo "${B}LAN CA (trust on devices):${RST} $CERT_DIR/ca.crt"
  echo "   SHA-256: $(openssl x509 -in "$CERT_DIR/ca.crt" -noout -fingerprint -sha256 2>/dev/null | cut -d= -f2)"
fi
echo
echo "${B}Devices:${RST}"
idx=0
for kind in ${DEV_KIND[@]+"${DEV_KIND[@]}"}; do
  name="${DEV_NAME[$idx]}"; tok="$(token_for "$name" 2>/dev/null)"
  case "$kind" in
    usb)   echo "   • ${B}$name${RST} (USB): streaming now. Re-run: ./local_dev/run_hushai_app.sh --token $tok" ;;
    wifi)  echo "   • ${B}$name${RST} (WiFi): bundle the CA + build the release APK, then set URL+token in-app:"
           echo "        cp $CERT_DIR/ca.crt hushai-android/app/src/release/res/raw/hushai_lan_ca.pem"
           echo "        (cd hushai-android && ./gradlew assembleRelease)"
           echo "        in-app → URL https://${LAN_IP:-<LAN-IP>}:8080  · token $tok" ;;
    ipcam) echo "   • ${B}$name${RST} (client): POST /v1/segments with 'Authorization: Bearer $tok' over ${SCHEME}, trusting the CA."
           echo "        smoke test: python local_dev/feed_segments.py --url ${SCHEME}://${LAN_IP:-127.0.0.1}:8080/v1/segments --token $tok --cacert $CERT_DIR/ca.crt" ;;
    browser) echo "   • ${B}$name${RST} (browser): open the viewer and use the ● Capture button (Chrome/Edge/Safari)." ;;
  esac
  idx=$((idx+1))
done
echo
echo "${B}Verify:${RST} open the viewer → ▦ System dashboard → your device shows ${GRN}connected${RST} once its first upload lands."
echo "${B}Stop:${RST}  press Ctrl-C here (or run ./local_dev/run_stack.sh --down)."
hr

# Hold the foreground on the running stack; Ctrl-C triggers the trap above.
wait "$STACK_PID"
