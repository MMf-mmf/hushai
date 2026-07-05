#!/usr/bin/env bash
# gen_certs.sh — local CA + LAN server cert (IP-SAN) for Hushai HTTPS.
#
# A LAN has no public DNS, so the server cert carries IP SANs (localhost, ::1, the
# emulator alias 10.0.2.2, and the Mac's auto-detected LAN IPv4s). Browsers + the
# Android release build trust the **CA** (`ca.crt`), NOT the leaf — so when the Mac's
# DHCP IP changes you only re-run this to re-mint the leaf; the CA (and the APK that
# bundles it) keep working.
#
#   ./local_dev/gen_certs.sh                          # auto-detect LAN IP(s)
#   LAN_IPS="192.168.1.50 192.168.1.51" ./local_dev/gen_certs.sh
#
# Outputs (in local_dev/certs/, gitignored):
#   ca.crt                 — the local root CA. Trust this on browsers + Android.
#   server.fullchain.crt   — leaf + CA chain (point TLS_CERT_PATH here).
#   server.pkcs8.key       — PKCS#8 private key (point TLS_KEY_PATH here).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CERT_DIR="$SCRIPT_DIR/certs"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DAYS_CA=3650
DAYS_LEAF=825   # Apple caps server-leaf validity at 825 days; stay under it.

# shellcheck source=lib_platform.sh
source "$SCRIPT_DIR/lib_platform.sh"

mkdir -p "$CERT_DIR"
cd "$CERT_DIR"

# Auto-detect the host's LAN IPv4(s) if not provided (skip loopback + link-local).
# hushai_lan_ips uses `ifconfig` on macOS and `ip -4 addr` on Linux.
if [[ -z "${LAN_IPS:-}" ]]; then
  LAN_IPS="$(hushai_lan_ips || true)"
fi
echo "[gen_certs] LAN IPs in cert SAN: ${LAN_IPS:-<none detected>}"

# 1. Local CA (self-signed root). Created once; reused on re-runs so the trust anchor
#    (and the Android APK that bundles it) stays stable across leaf rotations.
if [[ ! -f ca.key || ! -f ca.crt ]]; then
  echo "[gen_certs] minting a new local CA"
  openssl genrsa -out ca.key 4096
  openssl req -x509 -new -nodes -key ca.key -sha256 -days "$DAYS_CA" \
    -subj "/CN=Hushai Local CA/O=Hushai" -out ca.crt
else
  echo "[gen_certs] reusing existing CA (ca.crt)"
fi

# 2. Server key + CSR.
openssl genrsa -out server.key 2048
openssl req -new -key server.key -subj "/CN=hushai.local/O=Hushai" -out server.csr

# 3. SAN extfile: localhost/hushai.local + 127.0.0.1 + ::1 + emulator + every LAN IP.
{
  echo "subjectAltName = @alt"
  echo "extendedKeyUsage = serverAuth"
  echo "[alt]"
  echo "DNS.1 = localhost"
  echo "DNS.2 = hushai.local"
  echo "IP.1 = 127.0.0.1"
  echo "IP.2 = ::1"
  echo "IP.3 = 10.0.2.2"
  i=4
  for ip in ${LAN_IPS:-}; do
    echo "IP.$i = $ip"
    i=$((i + 1))
  done
} > san.ext

# 4. Sign the leaf with the CA.
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -days "$DAYS_LEAF" -sha256 -extfile san.ext -out server.crt

# 5. Full chain (leaf + CA) — RustlsConfig::from_pem_file wants leaf-first.
cat server.crt ca.crt > server.fullchain.crt

# 6. PKCS#8 key (rustls reads PKCS#8 PEM cleanly).
openssl pkcs8 -topk8 -nocrypt -in server.key -out server.pkcs8.key

chmod 600 ca.key server.key server.pkcs8.key

echo
echo "[gen_certs] wrote:"
echo "  CA (trust this):        $CERT_DIR/ca.crt"
echo "  server chain (rustls):  $CERT_DIR/server.fullchain.crt"
echo "  server key   (rustls):  $CERT_DIR/server.pkcs8.key"
echo
echo "Export for the Rust services (or pass --tls to run_stack.sh):"
echo "  export TLS_CERT_PATH=$CERT_DIR/server.fullchain.crt"
echo "  export TLS_KEY_PATH=$CERT_DIR/server.pkcs8.key"
echo
echo "Trust the CA on this Mac's browsers (one-time):"
echo "  sudo security add-trusted-cert -d -r trustRoot \\"
echo "    -k /Library/Keychains/System.keychain $CERT_DIR/ca.crt"
echo
echo "Bundle the CA into the Android release build (so the phone trusts the LAN cert):"
echo "  cp $CERT_DIR/ca.crt $REPO_ROOT/hushai-android/app/src/release/res/raw/hushai_lan_ca.pem"
echo
echo "Optional CertificatePinner SPKI pin (sha256/<base64> of the CA public key):"
echo -n "  sha256/"
openssl x509 -in ca.crt -pubkey -noout \
  | openssl pkey -pubin -outform der 2>/dev/null \
  | openssl dgst -sha256 -binary | openssl base64
