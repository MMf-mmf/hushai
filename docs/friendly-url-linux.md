# Friendly HTTPS URL on Linux — `https://hushai.local/`

Goal: same as macOS — reach the admin viewer at **`https://hushai.local/`** (no port), over
the LAN, gated by IP-allowlist + password, with a trusted local CA. On macOS this is one command
(`local_dev/serve.sh` → `gen_certs.sh` + CA-trust + `setup_hostname.sh` + `run_stack.sh --lan`).
This is the **Linux runbook** for the same outcome. It's manual today; a future
`local_dev/setup_hostname_linux.sh` would automate steps 2–4 and `serve.sh` would dispatch on
`uname -s`.

> **What is already cross-platform (no Linux-specific work):** the Rust viewer + `run_stack.sh`
> handle the bind, the IP allowlist, the password gate, and TLS the same on every OS:
> `VIEWER_BIND_ADDR=0.0.0.0:8070`, `VIEWER_ADMIN_IP_ALLOWLIST=<admin IPs>` (loopback always
> allowed), `VIEWER_ADMIN_PASSWORD` (or `VIEWER_ADMIN_PASSWORD_HASH`), `TLS_CERT_PATH`/`TLS_KEY_PATH`.
> `run_stack.sh --lan` already sets all of these. Only **three** things differ by OS:
> **(1) how `hushai.local` resolves, (2) how you drop the port, (3) how you trust the CA.**

---

## 1. TLS cert

`local_dev/gen_certs.sh` is portable bash + `openssl` and runs on Linux as-is, producing
`local_dev/certs/{ca.crt,server.fullchain.crt,server.pkcs8.key}` with SAN
`localhost, hushai.local, 127.0.0.1, ::1, <LAN IPs>`.

- It auto-detects LAN IPs with `ifconfig` (net-tools). If `ifconfig` is absent, either install
  net-tools or pass them explicitly: `LAN_IPS="192.168.1.50" ./local_dev/gen_certs.sh`.
- *Automation note:* add an `ip -4 addr` fallback to `detect_lan_ips` when scripting this for Linux.

## 2. Make `hushai.local` resolve — mDNS via Avahi

Linux uses **Avahi** (Bonjour-compatible mDNS).

```bash
# Debian/Ubuntu
sudo apt install -y avahi-daemon libnss-mdns
# Fedora/RHEL
sudo dnf install -y avahi nss-mdns
sudo systemctl enable --now avahi-daemon
```

`libnss-mdns` adds `mdns4_minimal [NOTFOUND=return]` to the `hosts:` line in `/etc/nsswitch.conf`
(verify it's there). Avahi advertises `<system-hostname>.local`. To serve the name `hushai.local`,
pick one:

- **Rename the host** (simplest): `sudo hostnamectl set-hostname hushai` → advertises `hushai.local`.
- **Publish an alias without renaming**: run `avahi-publish -a -R hushai.local <LAN-IP>` and persist
  it with a tiny systemd unit:
  ```ini
  # /etc/systemd/system/hushai-mdns.service
  [Unit] After=network-online.target avahi-daemon.service
  [Service] ExecStart=/usr/bin/avahi-publish -a -R hushai.local %i ; Restart=always
  [Install] WantedBy=multi-user.target
  ```
  (or use the community `avahi-aliases` helper).

Verify: `avahi-resolve -n hushai.local` and `getent hosts hushai.local`.

## 3. Drop the port — 443 → 8070

Pick **one** (recommended: A for a managed service, B to keep the viewer unprivileged):

- **A. Grant the binary the low-port capability (no redirect).** Unlike macOS, Linux can let a
  process bind <1024 directly. Cleanest in a systemd unit:
  ```ini
  [Service]
  AmbientCapabilities=CAP_NET_BIND_SERVICE
  Environment=VIEWER_BIND_ADDR=0.0.0.0:443
  ```
  (Ad-hoc equivalent: `sudo setcap 'cap_net_bind_service=+ep' target/release/hushai-viewer` — but
  that's on the binary inode, so **re-apply it after every rebuild**; `AmbientCapabilities` survives
  rebuilds.) Then the viewer serves HTTPS directly on 443; no firewall/redirect needed.

- **B. nftables redirect (viewer stays on 8070).**
  ```
  table inet hushai {
    chain prerouting { type nat hook prerouting priority dstnat;
      tcp dport 443 redirect to :8070 }          # remote LAN clients
    chain output     { type nat hook output priority -100;
      ip daddr <LAN-IP> tcp dport 443 redirect to :8070 }   # host hitting its own IP
  }
  ```
  Persist via `/etc/nftables.conf` + `sudo systemctl enable nftables`. The `output` chain rule is
  the Linux analogue of the macOS host-self redirect case.

- **C. iptables (older systems).** `sudo iptables -t nat -A PREROUTING -p tcp --dport 443 -j REDIRECT --to-ports 8070`
  plus a matching `OUTPUT` rule for host-self; persist with `iptables-persistent`.

- **D. Reverse proxy (Caddy/nginx) on 443 → 8070.** Heavier; the natural choice if you containerize.

## 4. Trust the CA

```bash
# System trust store
# Debian/Ubuntu:
sudo cp local_dev/certs/ca.crt /usr/local/share/ca-certificates/hushai.crt && sudo update-ca-certificates
# Fedora/RHEL:
sudo cp local_dev/certs/ca.crt /etc/pki/ca-trust/source/anchors/hushai.crt && sudo update-ca-trust
```

Chrome/Chromium and Firefox use their own NSS store, not the system one:
```bash
sudo apt install -y libnss3-tools      # provides certutil
certutil -d sql:$HOME/.pki/nssdb -A -t "C,," -n "Hushai Local CA" -i local_dev/certs/ca.crt
```
(Firefox is per-profile: `certutil -d sql:<profile-dir>` or import via Settings → Certificates.)

## 5. Bind + allowlist + serve

`run_stack.sh --lan` runs on Linux and already sets the bind, allowlist, and TLS env — so once
1–4 are in place:

```bash
./local_dev/run_stack.sh --lan      # then open https://hushai.local/
```

Or run the viewer alone with the same env the `--lan` flag sets:
```bash
VIEWER_BIND_ADDR=0.0.0.0:8070 \
VIEWER_ADMIN_IP_ALLOWLIST="<this host LAN IP>,<other admin IPs>" \
VIEWER_ADMIN_PASSWORD='...' \
TLS_CERT_PATH=local_dev/certs/server.fullchain.crt \
TLS_KEY_PATH=local_dev/certs/server.pkcs8.key \
cargo run -p hushai-viewer
```

## Persistence

Run the viewer as a **systemd service** (with `AmbientCapabilities` if you took option 3A) so it
survives reboots. The nftables rule, Avahi, and the CA trust already persist via their own units.

## Future automation

`local_dev/setup_hostname_linux.sh` would: detect apt/dnf, install + enable Avahi, set the
hostname or publish the alias, add the nftables redirect (or the systemd `AmbientCapabilities`),
and trust the CA — the Linux twin of `local_dev/setup_hostname.sh`. `serve.sh` would then branch
on `uname -s` and call it.
