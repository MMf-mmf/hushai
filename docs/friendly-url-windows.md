# Friendly HTTPS URL on Windows — `https://hushai.local/`

Goal: same as macOS/Linux — reach the admin viewer at **`https://hushai.local/`** (no port), over
the LAN, gated by IP-allowlist + password, with a trusted local CA. This is the **Windows runbook**;
a future `local_dev/setup_hostname.ps1` would automate it.

> **Cross-platform (no Windows-specific work):** the viewer's bind, IP allowlist, password gate,
> and TLS env are identical everywhere — `VIEWER_BIND_ADDR=0.0.0.0:8070`,
> `VIEWER_ADMIN_IP_ALLOWLIST=<admin IPs>` (loopback always allowed), `VIEWER_ADMIN_PASSWORD`
> (or `VIEWER_ADMIN_PASSWORD_HASH`), `TLS_CERT_PATH`/`TLS_KEY_PATH`. Only **(1) name resolution,
> (2) port-drop, (3) CA trust** differ.

> **Shell note:** `gen_certs.sh` and `run_stack.sh` are bash. On Windows, run them under **WSL2**
> or **Git Bash**, or use the native equivalents below. Run the setup steps in an **elevated
> PowerShell (Run as Administrator)**.

---

## 1. TLS cert

- **Easiest:** run `./local_dev/gen_certs.sh` in **WSL2** or **Git Bash** (needs `openssl`) →
  `local_dev/certs/{ca.crt,server.fullchain.crt,server.pkcs8.key}`, SAN includes `hushai.local`
  + the LAN IP.
- **Native PowerShell alternative:** `New-SelfSignedCertificate -Type SSLServerAuthentication
  -DnsName "hushai.local","localhost","<LAN-IP>" -CertStoreLocation Cert:\LocalMachine\My` then
  export the leaf chain to leaf-first PEM (`TLS_CERT_PATH`) and the key to PKCS#8 PEM
  (`TLS_KEY_PATH`) — rustls wants those formats. This is fiddly; prefer the WSL/Git Bash route so
  the cert material is identical across machines.

## 2. Make `hushai.local` resolve

Two paths:

- **A. hosts file (single machine — simplest).** Unlike macOS, **Windows honors the hosts file for
  `.local` names**. As admin, append to `C:\Windows\System32\drivers\etc\hosts`:
  ```
  <LAN-IP>   hushai.local
  ```
  Use the **LAN IP** (not `127.0.0.1`) so it matches the cert SAN + the IP allowlist, and so the
  viewer (bound `0.0.0.0`) answers. Then `ipconfig /flushdns`.
- **B. mDNS for the whole LAN.** Windows 10/11 can *resolve* `.local` via its built-in mDNS client,
  but *advertising* `hushai.local` to other devices needs a responder: install **Apple Bonjour**
  (Bonjour Print Services) or publish via DNS-SD, or add a record on the router/DNS. For a single
  admin PC, (A) is enough; for many devices, (B) avoids editing every machine's hosts file.

## 3. Drop the port — 443 → 8070

- **A. Bind 443 directly (simplest).** Windows does **not** reserve ports <1024 for admin, so the
  viewer can usually bind 443: set `VIEWER_BIND_ADDR=0.0.0.0:443`. First check nothing else owns it
  (`netstat -ano | findstr :443` — IIS / http.sys reservations are the usual culprits).
- **B. `netsh` portproxy (keep the viewer on 8070).** As admin:
  ```
  netsh interface portproxy add v4tov4 listenaddress=0.0.0.0 listenport=443 connectaddress=127.0.0.1 connectport=8070
  ```
  Persists across reboots (requires the **IP Helper** / `iphlpsvc` service). Remove with
  `netsh interface portproxy delete v4tov4 listenaddress=0.0.0.0 listenport=443`.
  *Verify after enabling that direct `https://127.0.0.1:8070/` still answers* — on macOS a redirect
  whose target was a loopback port broke direct loopback (we target the LAN IP there to avoid it);
  confirm portproxy→127.0.0.1:8070 doesn't have an analogous quirk, and if it does, point
  `connectaddress` at the LAN IP instead.
- **C. Reverse proxy** (Caddy for Windows, or IIS ARR) on 443 → 8070.

## 4. Firewall

Allow the inbound port through Windows Defender Firewall (443, or 8070 if you didn't drop the port):
```powershell
New-NetFirewallRule -DisplayName "Hushai viewer (443)" -Direction Inbound -Protocol TCP -LocalPort 443 -Action Allow
```

## 5. Trust the CA

```powershell
# Edge/Chrome (Windows cert store), as admin:
certutil -addstore -f Root local_dev\certs\ca.crt
# remove later: certutil -delstore Root "Hushai Local CA"
```
**Firefox** uses its own store — import `ca.crt` via Settings → Privacy & Security → Certificates →
View Certificates → Authorities → Import (trust for websites).

## 6. Bind + allowlist + serve

Under WSL2/Git Bash, `./local_dev/run_stack.sh --lan` works as on macOS/Linux. Native (PowerShell),
set the same env the `--lan` flag sets, then run the viewer:
```powershell
$env:VIEWER_BIND_ADDR        = "0.0.0.0:8070"   # or 0.0.0.0:443 with option 3A
$env:VIEWER_ADMIN_IP_ALLOWLIST = "<this PC LAN IP>,<other admin IPs>"
$env:VIEWER_ADMIN_PASSWORD   = "..."
$env:TLS_CERT_PATH           = "local_dev\certs\server.fullchain.crt"
$env:TLS_KEY_PATH            = "local_dev\certs\server.pkcs8.key"
cargo run -p hushai-viewer
```
Open `https://hushai.local/`.

## Persistence

Run the viewer as a **Windows Service** (`nssm install` or `sc.exe create`) or a **Scheduled Task at
logon** so it survives reboots; the hosts entry, portproxy, firewall rule, and CA trust already
persist on their own.

## Future automation

`local_dev/setup_hostname.ps1` would, elevated: write the hosts entry (or install Bonjour), add the
`netsh` portproxy (or set the 443 bind), add the firewall rule, and `certutil -addstore` the CA —
the Windows twin of `local_dev/setup_hostname.sh`.
