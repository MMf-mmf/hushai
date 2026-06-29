#!/usr/bin/env bash
#
# setup_hostname.sh — make the viewer reachable at a friendly, no-port HTTPS URL:
#
#       https://hushai.local/
#
# instead of https://127.0.0.1:8070/. Two host-level pieces (one-time `sudo`):
#
#   1. Bonjour name — sets this Mac's LocalHostName to `hushai`, so macOS advertises
#      `hushai.local` -> the host's LAN IP. Works on the host AND any Bonjour device
#      on the LAN. (This is a machine-wide name; it's also the AirDrop/SSH display
#      name. The previous value is backed up and restored by `--remove`.)
#   2. Port drop — a `pf` redirect 443 -> 8070, so you can omit the port. The viewer
#      stays UNPRIVILEGED on 8070 and still terminates TLS there with the `hushai.local`
#      cert (gen_certs.sh); pf just forwards the encrypted bytes. Persisted across
#      reboots via a LaunchDaemon.
#
# This is the friendly-URL half of the LAN admin setup. The security half (TLS cert,
# IP allowlist, password) is gen_certs.sh + `run_stack.sh --lan` — see
# docs/onboarding-a-camera.md and AGENTS.md "LAN security model".
#
#   ./local_dev/setup_hostname.sh             # set up   (sudo once)
#   ./local_dev/setup_hostname.sh --remove    # tear everything down (restores hostname)
#   HUSHAI_HOSTNAME=hush VIEWER_PORT=8070 ./local_dev/setup_hostname.sh
#
# Re-run after a DHCP IP change (same as gen_certs.sh): it re-detects the LAN IP(s)
# and rewrites the redirect. The chosen name is `hushai` -> `hushai.local` because
# that is exactly what the TLS cert is issued for; a bare name would fail cert checks.
set -euo pipefail

HOSTNAME_LABEL="${HUSHAI_HOSTNAME:-hushai}"   # -> <label>.local
VIEWER_PORT="${VIEWER_PORT:-8070}"
PUBLIC_PORT="${PUBLIC_PORT:-443}"             # the port we let users omit from the URL

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ANCHOR=/etc/pf.anchors/hushai
ANCHOR_NAME="com.apple/hushai"                # nest under macOS's default rdr-anchor "com.apple/*"
PLIST=/Library/LaunchDaemons/com.hushai.portredirect.plist
LHN_BACKUP="$SCRIPT_DIR/.hostname_backup"     # prior LocalHostName, for --remove (gitignored)

log() { echo "[setup_hostname] $*"; }
die() { echo "[setup_hostname] ERROR: $*" >&2; exit 1; }

# All this host's LAN IPv4s (skip loopback + link-local), one per line.
detect_lan_ips() {
  ifconfig 2>/dev/null | awk '/inet /{print $2}' | grep -Ev '^127\.|^169\.254\.' || true
}

reload_pf() {
  # Order matters: load the system default ruleset first (it carries the
  # `rdr-anchor "com.apple/*"` that makes our nested anchor get evaluated), THEN load
  # our rules into that sub-anchor, THEN enable pf. Re-running /etc/pf.conf would flush
  # an unreferenced sub-anchor, so our load must come after it.
  sudo pfctl -f /etc/pf.conf 2>/dev/null || true
  sudo pfctl -a "$ANCHOR_NAME" -f "$ANCHOR" 2>/dev/null || true
  sudo pfctl -E 2>/dev/null || true
}

remove() {
  log "tearing down (sudo)…"
  # 1. pf: flush our anchor + drop the boot LaunchDaemon + anchor file.
  sudo pfctl -a "$ANCHOR_NAME" -F all 2>/dev/null || true
  sudo launchctl bootout system "$PLIST" 2>/dev/null || true
  sudo rm -f "$PLIST" "$ANCHOR"
  # 2. Restore the previous LocalHostName if we saved one.
  if [[ -f "$LHN_BACKUP" ]]; then
    local prior; prior="$(cat "$LHN_BACKUP" 2>/dev/null || true)"
    if [[ -n "$prior" ]]; then
      sudo scutil --set LocalHostName "$prior"
      log "restored LocalHostName -> '$prior'"
    else
      log "previous LocalHostName was unset; leaving '$HOSTNAME_LABEL' (change it in System Settings ▸ General ▸ Sharing if you like)"
    fi
    rm -f "$LHN_BACKUP"
  else
    log "no hostname backup found; LocalHostName left as-is"
  fi
  log "done. https://$HOSTNAME_LABEL.local/ will no longer resolve/redirect."
}

case "${1:-}" in
  --remove|-r) remove; exit 0 ;;
  -h|--help)   awk 'NR>2 && /^[^#]/{exit} NR>2{print}' "$0"; exit 0 ;;
  "")          : ;;
  *) die "unknown arg: $1 (try --help)" ;;
esac

command -v scutil  >/dev/null 2>&1 || die "scutil not found (this script is macOS-only)."
command -v pfctl   >/dev/null 2>&1 || die "pfctl not found (this script is macOS-only)."

# --- 1. Bonjour name: set LocalHostName=hushai (back up the prior value once). -----
CURRENT_LHN="$(scutil --get LocalHostName 2>/dev/null || true)"
if [[ "$CURRENT_LHN" == "$HOSTNAME_LABEL" ]]; then
  log "LocalHostName already '$HOSTNAME_LABEL' — Bonjour name set."
else
  [[ -f "$LHN_BACKUP" ]] || printf '%s' "$CURRENT_LHN" > "$LHN_BACKUP"
  log "setting LocalHostName '$CURRENT_LHN' -> '$HOSTNAME_LABEL' (sudo); advertises $HOSTNAME_LABEL.local via Bonjour"
  sudo scutil --set LocalHostName "$HOSTNAME_LABEL"
fi

# --- 2. pf redirect PUBLIC_PORT(443) -> VIEWER_PORT(8070) for each LAN IP. ----------
# (Fill the array the bash 3.2 way — macOS /bin/bash has no `mapfile`.)
LAN_IPS=()
while IFS= read -r _ip; do [[ -n "$_ip" ]] && LAN_IPS+=("$_ip"); done < <(detect_lan_ips)
if [[ "${#LAN_IPS[@]}" -eq 0 ]]; then
  log "WARN: no LAN IP detected (offline?). Writing no redirect rules; re-run when on a network."
fi
{
  echo "# hushai-viewer: redirect $PUBLIC_PORT -> $VIEWER_PORT so https://$HOSTNAME_LABEL.local/ needs no port."
  echo "# Regenerated by local_dev/setup_hostname.sh; re-run after a DHCP IP change."
  # Redirect to the SAME LAN IP (the viewer binds 0.0.0.0 so it listens there too), NOT to
  # 127.0.0.1 — on macOS a `rdr` whose target is a loopback port drops *direct* connections to
  # that port, which would break `127.0.0.1:$VIEWER_PORT` (the run_stack health check + curl).
  # Targeting the LAN IP keeps loopback:$VIEWER_PORT clean.
  for ip in "${LAN_IPS[@]}"; do
    echo "rdr pass inet proto tcp from any to $ip port $PUBLIC_PORT -> $ip port $VIEWER_PORT"
  done
} | sudo tee "$ANCHOR" >/dev/null
log "wrote $ANCHOR (${#LAN_IPS[@]} LAN IP(s): ${LAN_IPS[*]:-none})"

reload_pf
log "pf redirect active ($PUBLIC_PORT -> $VIEWER_PORT)"

# --- 3. Persist the pf redirect across reboots via a LaunchDaemon. -----------------
log "installing boot LaunchDaemon $PLIST (sudo)…"
sudo tee "$PLIST" >/dev/null <<PLISTEOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.hushai.portredirect</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string><string>-c</string>
    <string>pfctl -f /etc/pf.conf; pfctl -a $ANCHOR_NAME -f $ANCHOR; pfctl -E</string>
  </array>
  <key>RunAtLoad</key><true/>
</dict></plist>
PLISTEOF
sudo launchctl bootout system "$PLIST" 2>/dev/null || true
sudo launchctl bootstrap system "$PLIST" 2>/dev/null || true

sudo dscacheutil -flushcache 2>/dev/null || true

cat <<EOF

[setup_hostname] done. Friendly URL: https://$HOSTNAME_LABEL.local/

Next:
  1. Generate + trust the TLS cert (valid for $HOSTNAME_LABEL.local):
       ./local_dev/gen_certs.sh
       sudo security add-trusted-cert -d -r trustRoot \\
         -k /Library/Keychains/System.keychain local_dev/certs/ca.crt
  2. Start the stack bound to the LAN with the IP allowlist + password gate:
       ./local_dev/run_stack.sh --lan
  3. Open https://$HOSTNAME_LABEL.local/ in the browser (admin login: VIEWER_ADMIN_PASSWORD).

Undo everything (restores the prior hostname): ./local_dev/setup_hostname.sh --remove
EOF
