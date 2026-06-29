# Onboarding a new camera

A **camera** is any client that conforms to `contracts/cameraToBackendContract.md` — the
Android app today; a webcam host, RTSP bridge, or the `feed_segments.py` replayer tomorrow.
The contract defines *how* a client talks to the backend; this runbook is the *operational*
counterpart it explicitly defers to: how a specific device gets onboarded onto your LAN.

To send segments a camera needs exactly three things:

1. **A per-device token** — its `Authorization: Bearer <token>` credential, revocable on its own.
2. **The backend URL** — `https://<host-LAN-IP>:8080` (the IP must be in the TLS cert's SAN).
3. **Trust of the LAN CA** — so it accepts the backend's self-signed certificate.

See `AGENTS.md` → "LAN security model" for the why; this is the step-by-step how.

---

## Prerequisites (one-time, on the host machine)

```bash
# 1. Generate the local CA + IP-SAN server cert (clients trust the CA, not the leaf).
./local_dev/gen_certs.sh                 # auto-detects this host's LAN IP(s)

# 2. Trust the CA in the host's browsers so the viewer loads without warnings.
sudo security add-trusted-cert -d -r trustRoot \
  -k /Library/Keychains/System.keychain local_dev/certs/ca.crt
```

> **If the host's LAN IP changes** (DHCP), re-run `./local_dev/gen_certs.sh`. It keeps the same
> CA and only re-mints the leaf with the new IP — no client needs to re-trust anything.

### Reaching the admin viewer — `https://hushai.local/`

Cameras talk to the backend by raw LAN IP (above). **Admins** reach the viewer by a friendly,
no-port name: run `./local_dev/setup_hostname.sh` once (sets this Mac's Bonjour name to `hushai` and
adds a `pf` redirect 443→8070), then start the stack with `./local_dev/run_stack.sh --lan` (binds
`0.0.0.0` + allowlists this host). The viewer is gated by an **IP allowlist + password** — add each
admin computer's IP to `VIEWER_ADMIN_IP_ALLOWLIST` (re-run on DHCP change, same as the cert). See
`hushai-viewer/README.md` → "Friendly URL".

---

## Step 1 — Mint the camera's token (the single build does it)

```bash
./local_dev/run_stack.sh --add-camera garage-cam
```

This **single command**:

- mints a random per-device token,
- saves `garage-cam:<token>` into `hushai-backend/.env`'s `DEVICE_TOKENS` (creating the line the
  first time, and seeding an `admin:<DEVICE_TOKEN>` entry so the viewer keeps working — see Gotchas),
- prints a **config card**: the token, the backend/rag HTTPS URLs at this host's LAN IP, the CA
  path, and the CA's SHA-256 fingerprint (read this aloud when trusting the cert on a device).

Then (re)start the backend so the new token is live:

```bash
./local_dev/run_stack.sh --tls
```

To **revoke** a camera later: delete its `name:token` entry from `DEVICE_TOKENS` in
`hushai-backend/.env` and restart. Every other camera keeps working.

---

## Step 2 — Configure the camera

### A) The Android app over Wi-Fi (the production path)

The **release** build is HTTPS-only and trusts the bundled LAN CA.

```bash
# 1. Bundle the LAN CA into the release build (gitignored; do this whenever the CA changes).
cp local_dev/certs/ca.crt hushai-android/app/src/release/res/raw/hushai_lan_ca.pem

# 2. Build the release APK (toolchain per AGENTS.md: JAVA_HOME, ANDROID_HOME, pinned ./gradlew).
cd hushai-android && ./gradlew assembleRelease
#    → app/build/outputs/apk/release/app-release-unsigned.apk  (sign before distributing)
```

Install it on the phone, then set in the app (Settings screen, or the launch Intent extras):

- **Backend URL**: `https://<host-LAN-IP>:8080`  (the IP from the config card; must be in the SAN)
- **Device token**: the token from the config card
- **RAG URL / token** (only if using the voice assistant): `https://<host-LAN-IP>:8090` + `RAG_TOKEN`

No cert install on the phone is needed — the CA is baked into the APK.

### B) The Android app over USB (the dev/debug path)

The **debug** build permits cleartext and tunnels everything over the cable, so no certs/LAN:

```bash
./local_dev/run_hushai_app.sh --token <token> --rag-token "$RAG_TOKEN"
# (defaults the URLs to localhost via `adb reverse`; pass --url/--rag-url for a LAN IP instead)
```

### C) Any other conforming client (webcam host, replayer, custom)

Present `Authorization: Bearer <token>` over HTTPS and trust the CA. Example with the replayer:

```bash
python local_dev/feed_segments.py \
  --url https://<host-LAN-IP>:8080/v1/segments \
  --token <token> \
  --cacert local_dev/certs/ca.crt
```

A client that can `POST /v1/segments` (multipart `manifest` + `body`) per the contract is a
conforming camera — nothing in the backend is per-camera-specific.

---

## Step 3 — Verify the camera is onboarded

- **Replayer smoke test** (fastest): the `feed_segments.py` command above should return `200`s.
  A wrong token returns `401`; the backend log shows the matched device label on success.
- **Real client**: start capture, then open the viewer's **▦ System** dashboard — the camera
  appears under "Cameras" as `connected` once its first upload lands, and its footage shows on the
  timeline. The backend log line for the upload carries the device label you chose.

---

## Gotchas

- **`DEVICE_TOKENS` supersedes the single `DEVICE_TOKEN`.** Once `DEVICE_TOKENS` is set, the old
  shared `dev-secret-token` is rejected. The viewer proxies `/v1/speakers*` + `/v1/persons*` to the
  backend using `BACKEND_TOKEN` (which defaults to `DEVICE_TOKEN`), so that value **must be one of
  the `DEVICE_TOKENS` entries** — `--add-camera` seeds `admin:<DEVICE_TOKEN>` automatically the
  first time. If you hand-edit `DEVICE_TOKENS`, keep an entry whose token equals the viewer's
  `BACKEND_TOKEN`/`DEVICE_TOKEN`, or the Voices/People admin pages will 401.
- **The LAN IP must be in the cert SAN.** If the camera can't reach `https://<ip>:8080` with a valid
  cert, the IP probably isn't in the SAN — re-run `gen_certs.sh` (it auto-detects current IPs, or
  pass `LAN_IPS="a.b.c.d"`).
- **Release APK without the bundled CA fails the TLS handshake** (by design — that's the trust
  anchor doing its job). Always copy `ca.crt` → `res/raw/hushai_lan_ca.pem` before `assembleRelease`.
- **`rag` requires `RAG_TOKEN`** when set (and warns if unset). The voice assistant must present the
  same token (`--rag-token` / the app's RAG token setting); the browser gets it via the viewer proxy.

---

## Reference

- Boundary/wire contract: `contracts/cameraToBackendContract.md`
- Security model + env vars: `AGENTS.md` → "LAN security model"
- Cert generation: `local_dev/gen_certs.sh` · Stack/runner: `local_dev/run_stack.sh`
- Android client: `hushai-android/README.md`
