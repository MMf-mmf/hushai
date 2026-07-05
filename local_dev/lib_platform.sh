#!/usr/bin/env bash
# lib_platform.sh — tiny cross-platform (macOS + Linux) adapter layer.
#
# SOURCE this; it only DEFINES functions (no side effects on source), so it is safe
# to `source` from run_stack.sh / serve.sh / gen_certs.sh / onboard.sh.
#
# Convention: query functions (hushai_os, hushai_arch, hushai_lan_ips,
# hushai_first_lan_ip, hushai_pids_on_port, hushai_pid_command, hushai_lib_path_var)
# print their RESULT to stdout and diagnostics (if any) to stderr, so callers can
# capture them with $(...). Action functions (hushai_pkg_install, hushai_pg_start,
# hushai_trust_ca) print progress to stderr and return 0/1.
#
# Written for macOS /bin/bash 3.2 (no `mapfile`, no associative arrays).

# ---- OS / arch -------------------------------------------------------------
hushai_os() {
  case "$(uname -s)" in
    Darwin) echo macos ;;
    Linux)  echo linux ;;
    *)      echo unknown ;;
  esac
}

hushai_arch() {
  case "$(uname -m)" in
    arm64|aarch64) echo arm64 ;;
    x86_64|amd64)  echo x86_64 ;;
    *)             uname -m ;;
  esac
}

# The dynamic-linker search-path env var name for the current OS. macOS strips DYLD_*
# under SIP unless the binary is exec'd directly (see run_stack.sh launch()).
hushai_lib_path_var() {
  case "$(hushai_os)" in
    macos) echo DYLD_FALLBACK_LIBRARY_PATH ;;
    *)     echo LD_LIBRARY_PATH ;;
  esac
}

# ---- LAN IPv4 detection (ifconfig on macOS, `ip` on Linux) -----------------
# Prints space-separated IPv4s, excluding loopback (127.) and link-local (169.254.).
hushai_lan_ips() {
  local ips=""
  if command -v ifconfig >/dev/null 2>&1; then
    ips="$(ifconfig 2>/dev/null | awk '/inet /{print $2}' \
          | grep -Ev '^127\.|^169\.254\.' | tr '\n' ' ' || true)"
  fi
  if [[ -z "${ips// /}" ]] && command -v ip >/dev/null 2>&1; then
    ips="$(ip -4 -o addr show scope global 2>/dev/null | awk '{print $4}' \
          | cut -d/ -f1 | grep -Ev '^127\.|^169\.254\.' | tr '\n' ' ' || true)"
  fi
  echo "$ips" | tr -s ' ' | sed 's/^ //; s/ $//'
}

hushai_first_lan_ip() { hushai_lan_ips | awk '{print $1}'; }

# ---- Port owners (for run_stack's self-teardown) ---------------------------
# Prints the LISTENING pids bound to a TCP port (one per line), via whichever of
# lsof / ss / fuser is available.
hushai_pids_on_port() {
  local port="$1"
  if command -v lsof >/dev/null 2>&1; then
    lsof -ti "tcp:$port" -sTCP:LISTEN 2>/dev/null || true
  elif command -v ss >/dev/null 2>&1; then
    ss -H -ltnp "sport = :$port" 2>/dev/null \
      | grep -oE 'pid=[0-9]+' | cut -d= -f2 | sort -u || true
  elif command -v fuser >/dev/null 2>&1; then
    fuser "$port/tcp" 2>/dev/null | tr -s ' ' '\n' | grep -E '^[0-9]+$' || true
  fi
}

# Prints the full command line of a pid (empty if gone). Works on macOS + Linux.
hushai_pid_command() {
  ps -p "$1" -o command= 2>/dev/null || ps -p "$1" -o args= 2>/dev/null || true
}

# ---- Package install (auto-install path; caller confirms first) ------------
# hushai_pkg_install <display-name> <brew-formula> <apt-package> <dnf-package>
# Detects the host package manager and installs. Returns non-zero if it can't.
hushai_pkg_install() {
  local display="$1" brew_f="$2" apt_p="$3" dnf_p="$4"
  echo "[platform] installing $display…" >&2
  case "$(hushai_os)" in
    macos)
      command -v brew >/dev/null 2>&1 || {
        echo "[platform] Homebrew not found — install it from https://brew.sh, then re-run." >&2
        return 1
      }
      brew install "$brew_f"
      ;;
    linux)
      if command -v apt-get >/dev/null 2>&1; then
        sudo apt-get update -y >/dev/null 2>&1 || true
        sudo apt-get install -y "$apt_p"
      elif command -v dnf >/dev/null 2>&1; then
        sudo dnf install -y "$dnf_p"
      elif command -v yum >/dev/null 2>&1; then
        sudo yum install -y "$dnf_p"
      else
        echo "[platform] no supported package manager (apt/dnf/yum) — install $display manually." >&2
        return 1
      fi
      ;;
    *)
      echo "[platform] unsupported OS for auto-install — install $display manually." >&2
      return 1
      ;;
  esac
}

# ---- Start Postgres --------------------------------------------------------
hushai_pg_start() {
  case "$(hushai_os)" in
    macos)
      brew services start postgresql@16 >/dev/null 2>&1 && return 0
      brew services start postgresql    >/dev/null 2>&1 && return 0
      ;;
    linux)
      sudo systemctl start postgresql >/dev/null 2>&1 && return 0
      sudo service postgresql start   >/dev/null 2>&1 && return 0
      if command -v pg_ctlcluster >/dev/null 2>&1; then
        local ver; ver="$(ls /etc/postgresql 2>/dev/null | sort -n | tail -1 || true)"
        [[ -n "$ver" ]] && sudo pg_ctlcluster "$ver" main start >/dev/null 2>&1 && return 0
      fi
      ;;
  esac
  return 1
}

# ---- Trust a local CA in the OS trust store --------------------------------
# hushai_trust_ca <path-to-ca.crt>. Best-effort on Linux (system store + NSS/Firefox).
hushai_trust_ca() {
  local ca="$1"
  [[ -f "$ca" ]] || { echo "[platform] CA not found: $ca" >&2; return 1; }
  case "$(hushai_os)" in
    macos)
      sudo security add-trusted-cert -d -r trustRoot \
        -k /Library/Keychains/System.keychain "$ca"
      ;;
    linux)
      local ok=1
      if [[ -d /usr/local/share/ca-certificates ]] && command -v update-ca-certificates >/dev/null 2>&1; then
        sudo cp "$ca" /usr/local/share/ca-certificates/hushai-local-ca.crt \
          && sudo update-ca-certificates >/dev/null 2>&1 && ok=0
      elif [[ -d /etc/pki/ca-trust/source/anchors ]] && command -v update-ca-trust >/dev/null 2>&1; then
        sudo cp "$ca" /etc/pki/ca-trust/source/anchors/hushai-local-ca.crt \
          && sudo update-ca-trust extract >/dev/null 2>&1 && ok=0
      fi
      # NSS (Firefox / Chromium) — best effort, non-fatal.
      if command -v certutil >/dev/null 2>&1; then
        local db
        for db in "$HOME/.pki/nssdb" "$HOME/.mozilla/firefox"/*.default*; do
          [[ -d "$db" ]] || continue
          certutil -A -n "Hushai Local CA" -t "C,," -i "$ca" -d "sql:$db" >/dev/null 2>&1 || true
        done
      fi
      return "$ok"
      ;;
    *)
      echo "[platform] don't know how to trust a CA on this OS — trust $ca manually." >&2
      return 1
      ;;
  esac
}
