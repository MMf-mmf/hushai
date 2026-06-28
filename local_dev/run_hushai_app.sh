#!/usr/bin/env bash
#
# run_hushai_app.sh — build, install, configure, and drive the hushai-android
# capture client against a backend, with NO manual UI taps. Lets an operator (or
# an agent) watch real footage flow via logcat, then stops cleanly.
#
# Usage:
#   run_hushai_app.sh [--url URL] [--rag-url URL] [--token TOK] [--device SERIAL]
#                     [--no-build] [--duration SECS] [--audio-only] [--stop]
#
# --audio-only drives the app's audio-only capture mode: no camera is opened and
# no video stream is uploaded (only cam0-audio), saving storage + battery.
#
# Connection: for a physical USB phone this runs entirely over the cable with NO
# shared network — it auto-creates `adb reverse` tunnels (tcp:8080 backend,
# tcp:8090 RAG/TTS) and defaults both URLs to localhost. For an emulator it
# defaults to 10.0.2.2 (the emulator's host alias) and skips the tunnels.
#
# Defaults (USB phone): --url http://localhost:8080, --rag-url http://localhost:8090,
# --token dev-secret-token. Pass --url/--rag-url to point at a LAN IP or remote
# backend instead (then the same network IS required).
#
# Re-pointing at a different backend/terminator = same script, different --url/--token.
set -euo pipefail

# Empty => resolved after device detection (localhost for USB, 10.0.2.2 for emulator).
URL=""
URL_SET=0
RAG_URL=""
RAG_URL_SET=0
TOKEN="dev-secret-token"
DEVICE=""
NO_BUILD=0
DURATION=120
STOP_ONLY=0
AUDIO_ONLY=0

PKG="com.hushai.android"
ACTIVITY="$PKG/.MainActivity"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
APP_DIR="$REPO_ROOT/hushai-android"
APK="$APP_DIR/app/build/outputs/apk/debug/app-debug.apk"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --url) URL="$2"; URL_SET=1; shift 2 ;;
    --rag-url) RAG_URL="$2"; RAG_URL_SET=1; shift 2 ;;
    --token) TOKEN="$2"; shift 2 ;;
    --device) DEVICE="$2"; shift 2 ;;
    --no-build) NO_BUILD=1; shift ;;
    --duration) DURATION="$2"; shift 2 ;;
    --audio-only) AUDIO_ONLY=1; shift ;;
    --stop) STOP_ONLY=1; shift ;;
    -h|--help) sed -n '3,23p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

# --- Locate adb (NOT on PATH in this env) -----------------------------------
find_adb() {
  if command -v adb >/dev/null 2>&1; then command -v adb; return; fi
  for base in "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}" "$HOME/Library/Android/sdk" "/usr/local/share/android-sdk"; do
    [[ -n "$base" && -x "$base/platform-tools/adb" ]] && { echo "$base/platform-tools/adb"; return; }
  done
  echo "ERROR: adb not found. Install platform-tools or set ANDROID_HOME." >&2
  exit 1
}
ADB_BIN="$(find_adb)"

# --- Resolve a single target device -----------------------------------------
adb() { "$ADB_BIN" ${DEVICE:+-s "$DEVICE"} "$@"; }

resolve_device() {
  "$ADB_BIN" start-server >/dev/null 2>&1 || true
  local devices
  devices="$("$ADB_BIN" devices | awk 'NR>1 && $2=="device" {print $1}')"
  local count; count="$(echo "$devices" | grep -c . || true)"
  if [[ -z "$DEVICE" ]]; then
    if [[ "$count" -eq 0 ]]; then
      echo "ERROR: no authorized device. Enable USB debugging + tap 'Allow' on the phone, then re-run." >&2
      "$ADB_BIN" devices >&2
      exit 1
    elif [[ "$count" -gt 1 ]]; then
      echo "ERROR: multiple devices; pass --device SERIAL:" >&2
      echo "$devices" >&2
      exit 1
    fi
    DEVICE="$(echo "$devices" | head -1)"
  fi
  echo "[device] $DEVICE"
}

# --- Clean stop (idempotent) ------------------------------------------------
send_stop() {
  echo "[stop] sending stop intent"
  adb shell am start -n "$ACTIVITY" --ez stop true >/dev/null 2>&1 || true
}

resolve_device

if [[ "$STOP_ONLY" -eq 1 ]]; then
  send_stop
  echo "[stop] done"
  exit 0
fi

# --- Resolve connection ------------------------------------------------------
# Physical USB phone: drive the WHOLE loop over the cable with no shared network
# by tunnelling the device's localhost to this host (`adb reverse`) and pointing
# both services at localhost. Emulator: reach the host via its 10.0.2.2 alias
# (no tunnel needed). Explicit --url/--rag-url always win (e.g. a LAN/remote IP).
if [[ "$DEVICE" == emulator-* ]]; then
  [[ "$URL_SET" -eq 0 ]] && URL="http://10.0.2.2:8080"
  [[ "$RAG_URL_SET" -eq 0 ]] && RAG_URL="http://10.0.2.2:8090"
else
  [[ "$URL_SET" -eq 0 ]] && URL="http://localhost:8080"
  [[ "$RAG_URL_SET" -eq 0 ]] && RAG_URL="http://localhost:8090"
  echo "[usb] adb reverse tcp:8080 + tcp:8090 (full loop over the cable, no shared network)"
  adb reverse tcp:8080 tcp:8080 >/dev/null
  adb reverse tcp:8090 tcp:8090 >/dev/null
fi

# --- Build -------------------------------------------------------------------
if [[ "$NO_BUILD" -eq 0 ]]; then
  echo "[build] :app:assembleDebug"
  export JAVA_HOME="${JAVA_HOME:-/opt/homebrew/opt/openjdk@17}"
  export ANDROID_HOME="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
  ( cd "$APP_DIR" && ./gradlew :app:assembleDebug -q )
fi
[[ -f "$APK" ]] || { echo "ERROR: APK not found at $APK (build first)" >&2; exit 1; }

# --- Install + grant runtime permissions ------------------------------------
echo "[install] $APK"
adb install -r -g "$APK" >/dev/null
# Audio-only never opens the camera, so don't bother granting CAMERA.
PERMS=(RECORD_AUDIO POST_NOTIFICATIONS)
[[ "$AUDIO_ONLY" -eq 0 ]] && PERMS+=(CAMERA)
for perm in "${PERMS[@]}"; do
  adb shell pm grant "$PKG" "android.permission.$perm" >/dev/null 2>&1 || true
done

# --- Launch + configure + autostart via Intent extras (no UI taps) ----------
echo "[launch] url=$URL rag_url=$RAG_URL token=*** autostart=true audio_only=$AUDIO_ONLY"
AUDIO_ONLY_EXTRA=()
[[ "$AUDIO_ONLY" -eq 1 ]] && AUDIO_ONLY_EXTRA=(--ez audio_only true)
# ${arr[@]+...} guards against "unbound variable" when the array is empty under
# `set -u` on bash 3.2 (macOS default). rag_url forces the voice-assistant host
# (overrides any stale LAN-IP left in DataStore by a prior wireless session).
adb shell am start -n "$ACTIVITY" \
  --es url "$URL" --es rag_url "$RAG_URL" --es token "$TOKEN" \
  ${AUDIO_ONLY_EXTRA[@]+"${AUDIO_ONLY_EXTRA[@]}"} --ez autostart true >/dev/null

# --- Observe footage flow ----------------------------------------------------
echo "[observe] tailing HUSHAI_TX for ${DURATION}s (Ctrl-C to stop early)…"
adb logcat -c || true
adb logcat -s HUSHAI_TX:I &
LOGCAT_PID=$!
trap 'kill "$LOGCAT_PID" 2>/dev/null || true; send_stop' INT TERM
sleep "$DURATION"
kill "$LOGCAT_PID" 2>/dev/null || true
trap - INT TERM

# --- Summary from the captured window ---------------------------------------
echo ""
echo "[summary] accepted (status=200) per stream during the run:"
adb logcat -d -s HUSHAI_TX:I \
  | grep -oE 'stream=[^ ]+ seq=[0-9]+ .*status=200' \
  | sed -E 's/.*stream=([^ ]+).*/\1/' | sort | uniq -c || true

# --- Optional: confirm rows landed in the real backend ----------------------
if command -v psql >/dev/null 2>&1 && [[ -n "${DATABASE_URL:-}" ]]; then
  echo "[backend] segment rows (psql):"
  psql "$DATABASE_URL" -c \
    "SELECT device_id, stream_id, count(*), max(sequence)+1 AS expect FROM segments GROUP BY 1,2 ORDER BY 2;" || true
fi

# --- Clean stop --------------------------------------------------------------
send_stop
echo "[done] re-run with --stop to ensure stopped; script is idempotent."
