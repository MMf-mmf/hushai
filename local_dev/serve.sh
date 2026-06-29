#!/usr/bin/env bash
#
# serve.sh — ONE command to serve the viewer at https://hushai.local/ (macOS).
#
# Does the whole first-run host setup (idempotent), then starts the stack — so a user
# never types the individual commands:
#   1. TLS cert (local CA + hushai.local leaf)        → local_dev/gen_certs.sh
#   2. Trust the CA in the System keychain            → sudo, once
#   3. Bonjour name `hushai` + pf 443→8070 redirect   → local_dev/setup_hostname.sh (sudo, once)
#   4. Start the stack bound to the LAN               → local_dev/run_stack.sh --lan
# Steps already done are detected and skipped, so re-running won't re-prompt for sudo.
#
#   ./local_dev/serve.sh                # set up (if needed) + serve; Ctrl-C stops the stack
#   ./local_dev/serve.sh --check        # report setup status only (no changes, no sudo, no start)
#   ./local_dev/serve.sh --down         # just stop a running stack (no setup)
#   ./local_dev/serve.sh --release      # any other flags are forwarded to run_stack.sh
#
# Undo the host changes (hostname + redirect): ./local_dev/setup_hostname.sh --remove
# Linux / Windows: see docs/friendly-url-linux.md and docs/friendly-url-windows.md.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CERT_DIR="$SCRIPT_DIR/certs"
CA_CN="Hushai Local CA"
HOSTNAME_LABEL="${HUSHAI_HOSTNAME:-hushai}"
ANCHOR=/etc/pf.anchors/hushai
PLIST=/Library/LaunchDaemons/com.hushai.portredirect.plist

log() { echo "[serve] $*"; }
die() { echo "[serve] ERROR: $*" >&2; exit 1; }

# Non-macOS → point at the platform runbooks (the cross-platform bits — bind 0.0.0.0, IP
# allowlist, password, TLS env — are identical; only mDNS + port-drop + CA-trust differ).
if [[ "$(uname -s)" != "Darwin" ]]; then
  die "this one-command setup is macOS-only. Linux: docs/friendly-url-linux.md · Windows: docs/friendly-url-windows.md"
fi

# --down: stop without any setup.
if [[ "${1:-}" == "--down" ]]; then shift; exec "$SCRIPT_DIR/run_stack.sh" --down "$@"; fi

first_lan_ip() { ifconfig 2>/dev/null | awk '/inet /{print $2}' | grep -Ev '^127\.|^169\.254\.' | head -1; }

# --- status probes (no side effects, no sudo) ------------------------------
have_cert()  { [[ -f "$CERT_DIR/server.fullchain.crt" && -f "$CERT_DIR/server.pkcs8.key" && -f "$CERT_DIR/ca.crt" ]]; }
ca_trusted() { security find-certificate -c "$CA_CN" /Library/Keychains/System.keychain >/dev/null 2>&1; }
host_ready() {
  local ip; ip="$(first_lan_ip)"
  [[ "$(scutil --get LocalHostName 2>/dev/null || true)" == "$HOSTNAME_LABEL" ]] \
    && [[ -f "$PLIST" ]] \
    && [[ -n "$ip" ]] && grep -q "to $ip port 443" "$ANCHOR" 2>/dev/null
}

if [[ "${1:-}" == "--check" ]]; then
  echo "Setup status for https://$HOSTNAME_LABEL.local/ (LAN IP: $(first_lan_ip || echo '?')):"
  have_cert  && echo "  [x] TLS cert present  ($CERT_DIR)"             || echo "  [ ] TLS cert missing            → gen_certs.sh"
  ca_trusted && echo "  [x] CA trusted in System keychain"            || echo "  [ ] CA not trusted              → add-trusted-cert"
  host_ready && echo "  [x] Bonjour name + 443→8070 redirect set"     || echo "  [ ] hostname/redirect not set   → setup_hostname.sh (or LAN IP changed)"
  exit 0
fi

# --- decide what's needed, so we only invoke sudo when something must change ---
NEED_CERT=0;  have_cert  || NEED_CERT=1
NEED_TRUST=0; ca_trusted || NEED_TRUST=1
NEED_HOST=0;  host_ready || NEED_HOST=1

if [[ "$NEED_TRUST" -eq 1 || "$NEED_HOST" -eq 1 ]]; then
  log "first-run host setup needed (trust=$NEED_TRUST hostname/redirect=$NEED_HOST) — you'll be asked for your password once…"
  sudo -v || die "sudo is required for first-run setup (CA trust + hostname/redirect). Re-run when ready."
fi

# 1. TLS cert (idempotent: gen_certs reuses the CA, re-mints the leaf).
if [[ "$NEED_CERT" -eq 1 ]]; then
  log "generating the local CA + hushai.local TLS cert…"
  "$SCRIPT_DIR/gen_certs.sh" >/dev/null || die "gen_certs.sh failed."
else
  log "TLS cert present — skipping."
fi

# 2. Trust the CA in the System keychain (browsers then show a valid lock).
if [[ "$NEED_TRUST" -eq 1 ]]; then
  log "trusting the local CA in the System keychain…"
  sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain "$CERT_DIR/ca.crt" \
    || die "trusting the CA failed."
else
  log "CA already trusted — skipping."
fi

# 3. Bonjour name + 443→8070 redirect (setup_hostname.sh is itself idempotent).
if [[ "$NEED_HOST" -eq 1 ]]; then
  log "configuring the Bonjour name + 443→8070 redirect…"
  "$SCRIPT_DIR/setup_hostname.sh" || die "setup_hostname.sh failed."
else
  log "Bonjour name + redirect already set for this LAN IP — skipping."
fi

# 4. Serve. run_stack --lan binds 0.0.0.0, allowlists this host, and serves HTTPS.
log "starting the stack → https://$HOSTNAME_LABEL.local/  (admin password from VIEWER_ADMIN_PASSWORD; Ctrl-C stops everything)"
exec "$SCRIPT_DIR/run_stack.sh" --lan "$@"
