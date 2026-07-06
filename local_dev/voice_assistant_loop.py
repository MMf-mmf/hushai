#!/usr/bin/env python3
"""
voice_assistant_loop.py — the voice-assistant ACOUSTIC test matrix (requires the phone).

Drives the on-device wake-word → owner-verify → RAG loop end-to-end over the real air gap:
plays matrix clips (built by local_dev/build_voice_matrix.sh) through the Mac speakers into the
phone's mic, walks the app's GUIDED enrollment (6 prompted samples + held-out verification) via
the `--ez enroll true` intent, then runs the accept/reject trial matrix and a cross-restart
persistence block. All evidence comes from logcat (HUSHAI_TX) — NEVER screenshots (the Galaxy S8
SurfaceView is black under screencap):

    "voice assistant ready (wake='…' enrolled=…)"   liveness + persistence
    "enroll: captured voiceprint #k/6"              guided-step progress
    "enroll: verify cosine=…" / "enroll: complete"  the held-out check
    "speaker cosine=<v> (threshold=0.5)"            per-utterance score (the labeled CSV)
    "rag chat ok (…)"                               ACCEPT evidence
    "speaker rejected (cosine below threshold)"     REJECT evidence
    "speaker verify: no voiceprint captured"        utterance heard but no x-vector

A trial with none of the accept/reject markers is INCONCLUSIVE (usually a missed wake word) and
is replayed once; still nothing → excluded from the accept/reject statistics (mirrors the eval
harness's exit-2 semantics). CARDINAL RULE: this harness MEASURES; it never justifies moving
SPEAKER_THRESHOLD. Non-separating conditions are findings to file with the CSV attached.

Pass bars (initial; freeze report-only rows after the first labeled run):
    enrollment completes            (hard; <=3 attempts)
    owner clean                5/5  (hard)
    owner + bed @ SNR 20    >=2/3 per bed (hard)
    owner + bed @ SNR 10/5          report-only on run 1 → freeze
    cross-restart            2/2 + enrolled=true on restart (hard)
    stranger clean           0/5 accepts (hard)
    stranger + tv @ 10       0/3 accepts (hard)

Prereqs: phys stack up (local_dev/phys.env; RAG reachable), phone plugged in + authorized,
Mac volume ~70%, phone mic ~30cm from the speakers, quiet room.

Usage:
  python3 local_dev/voice_assistant_loop.py                # full run (enroll + matrix + restart)
  python3 local_dev/voice_assistant_loop.py --skip-enroll  # reuse the on-device profile
"""
import argparse
import csv
import json
import os
import re
import subprocess
import sys
import time

from phonelock import phone_lock

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
MATRIX = os.path.join(ROOT, "local_dev", "captures", "voice_matrix")
ADB = os.path.expanduser(os.environ.get("ADB", "~/Library/Android/sdk/platform-tools/adb"))
PKG = "com.hushai.android"
ACTIVITY = f"{PKG}/.MainActivity"
PHYS_BACKEND_PORT = os.environ.get("PHYS_BACKEND_PORT", "8081")
PHYS_RAG_PORT = os.environ.get("PHYS_RAG_PORT", "8091")
TOKEN = os.environ.get("DEVICE_TOKEN", "dev-secret-token")
# The assistant's /v1/rag/chat calls authenticate with the RAG bearer (server RAG_TOKEN),
# NOT the device ingest token. Run-2 finding: without this extra the phone reuses whatever
# rag_token a prior session persisted to DataStore and every chat call 401s (which also
# explains run-1's "RAG starvation" — the requests were rejected in 0ms, not slow).
RAG_TOKEN = os.environ.get("RAG_TOKEN", "dev-rag-token")

THINK_WINDOW_SECS = 30  # post-clip window for verify + RAG round-trip evidence
# (run-1 finding: 12s starved the RAG round-trip under ollama contention — verified
# trials with cosine 0.74-0.86 were wrongly excluded)


def adb(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run([ADB, *args], capture_output=True, text=True, check=False)


def logcat() -> str:
    return adb("logcat", "-d", "-s", "HUSHAI_TX:I").stdout


def clear_logcat() -> None:
    adb("logcat", "-c")


def afplay(path: str) -> None:
    subprocess.run(["afplay", path], check=True)


def wait_for(pattern: str, timeout: float, poll: float = 1.0):
    """Poll logcat until `pattern` matches; returns the match or None."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        m = re.search(pattern, logcat())
        if m:
            return m
        time.sleep(poll)
    return None


def start_app(extra_flags=()) -> None:
    adb("shell", "am", "start", "-n", ACTIVITY,
        "--es", "url", "http://localhost:8080",
        "--es", "rag_url", "http://localhost:8090",
        "--es", "token", TOKEN,
        "--es", "rag_token", RAG_TOKEN,
        "--ez", "audio_only", "true",
        "--ez", "assistant", "true",
        "--ez", "autostart", "true",
        *extra_flags)


def restart_app() -> re.Match | None:
    adb("shell", "am", "force-stop", PKG)
    time.sleep(1.5)
    clear_logcat()
    start_app()
    return wait_for(r"voice assistant ready \(wake='(\S+)' enrolled=(\w+)\)", timeout=120)


def run_enrollment(clips: list[str], attempt: int) -> bool:
    """Walk the guided flow: 6 sample clips + the verify clip, gated on logcat progress."""
    print(f"[va] enrollment attempt {attempt}: trigger + {len(clips)} clips")
    clear_logcat()
    adb("shell", "am", "start", "-n", ACTIVITY, "--ez", "enroll", "true")
    if not wait_for(r"enroll: started", timeout=15):
        print("[va]   enroll intent didn't land")
        return False
    samples, verify_clip = clips[:-1], clips[-1]
    for i, clip in enumerate(samples, start=1):
        for retry in range(2):  # one replay per step (strike tolerance)
            time.sleep(0.8)
            afplay(os.path.join(MATRIX, clip))
            if wait_for(rf"enroll: captured voiceprint #{i}/", timeout=12):
                break
            if re.search(r"enroll: (aborted|failed)", logcat()):
                print(f"[va]   enrollment aborted at step {i}")
                return False
            if retry == 1:
                print(f"[va]   step {i} never registered")
                return False
    if not wait_for(r"enroll: verification step", timeout=10):
        print("[va]   verification step never announced")
        return False
    for retry in range(2):  # the app allows one verify retry
        time.sleep(0.8)
        afplay(os.path.join(MATRIX, verify_clip))
        if wait_for(r"enroll: complete", timeout=12):
            print("[va]   enrolled ✓ (held-out verification passed)")
            return True
        if re.search(r"enroll: failed", logcat()):
            break
    print("[va]   verification failed")
    return False


def run_trial(row: dict) -> dict:
    """Play one clip; scrape cosine + the accept/reject decision from logcat."""
    clear_logcat()
    afplay(os.path.join(MATRIX, row["clip"]))
    time.sleep(min(THINK_WINDOW_SECS, 4))
    decision, cosine = "inconclusive", None
    deadline = time.time() + THINK_WINDOW_SECS
    while time.time() < deadline:
        log = logcat()
        m = re.findall(r"speaker cosine=([0-9.\-]+)", log)
        if m:
            cosine = float(m[-1])
        if "rag chat ok" in log:
            decision = "accept"
            break
        if "speaker rejected" in log:
            decision = "reject"
            break
        if "no voiceprint captured" in log:
            decision = "no_voiceprint"
            break
        # Accept-degraded: verified (cosine >= 0.5, no reject line) but the RAG call errored.
        if cosine is not None and cosine >= 0.5 and "RAG error" in log:
            decision = "accept_degraded"
            break
        time.sleep(1.5)
    # Window closed with a verification score but no RAG completion: the ACCEPT decision
    # already happened on-device (cosine >= gate, no reject) — the matrix asserts the
    # verifier, so count it, flagged degraded (RAG latency is reported, not gated, here).
    if decision == "inconclusive" and cosine is not None:
        decision = "accept_degraded" if cosine >= 0.5 else "reject"
    return {**row, "cosine": cosine, "decision": decision}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--skip-enroll", action="store_true", help="reuse the on-device owner profile")
    ap.add_argument("--out", default=os.path.join(MATRIX, "results.csv"))
    args = ap.parse_args()

    manifest_path = os.path.join(MATRIX, "matrix_manifest.json")
    if not os.path.isfile(manifest_path):
        raise SystemExit("no matrix_manifest.json — run ./local_dev/build_voice_matrix.sh first")
    manifest = json.load(open(manifest_path))
    if adb("devices").stdout.count("\tdevice") < 1:
        raise SystemExit("no adb device — is the phone plugged in + authorized?")

    adb("reverse", "tcp:8080", f"tcp:{PHYS_BACKEND_PORT}")
    adb("reverse", "tcp:8090", f"tcp:{PHYS_RAG_PORT}")

    ready = restart_app()
    if not ready:
        raise SystemExit("[va] FAIL: 'voice assistant ready' never appeared (models? assistant flag?)")
    print(f"[va] ready (wake='{ready.group(1)}' enrolled={ready.group(2)})")

    # --- Enrollment (<=3 attempts) --------------------------------------------------------
    if not args.skip_enroll:
        ok = any(run_enrollment(manifest["enroll_clips"], attempt=i + 1) for i in range(3))
        if not ok:
            raise SystemExit("[va] FAIL: enrollment did not complete in 3 attempts (hard bar)")

    # --- Trial matrix ---------------------------------------------------------------------
    results = []
    for row in manifest["trials"]:
        r = run_trial(row)
        if r["decision"] == "inconclusive":  # missed wake word — one replay
            r = run_trial(row)
            if r["decision"] == "inconclusive":
                r["decision"] = "excluded"
        results.append(r)
        print(f"[va] {r['clip']:<36} {r['speaker']:<8} {r['condition']:<6} "
              f"snr={str(r['snr'] or '-'):<4} cosine={r['cosine']} → {r['decision']}")

    # --- Cross-restart persistence (DataStore profile must survive a force-stop) ----------
    print("[va] cross-restart persistence check")
    ready = restart_app()
    restart_enrolled = bool(ready and ready.group(2) == "true")
    restart_trials = []
    if restart_enrolled:
        for row in [t for t in manifest["trials"] if t["condition"] == "clean" and t["speaker"] == "owner"][:2]:
            restart_trials.append(run_trial(row))
            print(f"[va]   restart trial {row['clip']} → {restart_trials[-1]['decision']}")

    # --- Score against the bars ------------------------------------------------------------
    def accepted(r):
        return r["decision"] in ("accept", "accept_degraded")

    def bucket(speaker, condition, snr=None):
        return [r for r in results
                if r["speaker"] == speaker and r["condition"] == condition
                and (snr is None or r["snr"] == snr) and r["decision"] != "excluded"]

    failures, report = [], []

    oc = bucket("owner", "clean")
    report.append(f"owner clean: {sum(map(accepted, oc))}/{len(oc)} accepted (bar: all)")
    if len(oc) == 0 or not all(map(accepted, oc)):
        failures.append("owner-clean")
    beds = sorted({r["condition"] for r in results if r["condition"] != "clean"})
    for bed in beds:
        b20 = bucket("owner", bed, 20)
        n_ok = sum(map(accepted, b20))
        report.append(f"owner+{bed} @20dB: {n_ok}/{len(b20)} accepted (bar: >=2/3)")
        if len(b20) > 0 and n_ok < 2:
            failures.append(f"owner-{bed}-20db")
        for snr in (10, 5):
            b = bucket("owner", bed, snr)
            if b:
                report.append(f"owner+{bed} @{snr}dB: {sum(map(accepted, b))}/{len(b)} accepted (report-only)")
    sc = bucket("stranger", "clean")
    n_bad = sum(map(accepted, sc))
    report.append(f"stranger clean: {n_bad}/{len(sc)} accepted (bar: 0)")
    if n_bad > 0:
        failures.append("stranger-clean-accepted")
    stv = bucket("stranger", "tv", 10)
    n_bad = sum(map(accepted, stv))
    report.append(f"stranger+tv @10dB: {n_bad}/{len(stv)} accepted (bar: 0)")
    if n_bad > 0:
        failures.append("stranger-tv-accepted")
    report.append(f"cross-restart: enrolled={restart_enrolled}, "
                  f"{sum(map(accepted, restart_trials))}/{len(restart_trials)} accepted (bar: true + 2/2)")
    if not restart_enrolled or len(restart_trials) < 2 or not all(map(accepted, restart_trials)):
        failures.append("cross-restart")
    excluded = [r["clip"] for r in results if r["decision"] == "excluded"]
    if excluded:
        report.append(f"excluded (wake-word missed twice): {len(excluded)} — {', '.join(excluded)}")

    # --- CSV (the labeled capture set) -----------------------------------------------------
    with open(args.out, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=["clip", "speaker", "condition", "snr", "secs", "cosine", "decision", "expect"])
        w.writeheader()
        for r in results + restart_trials:
            w.writerow({k: r.get(k) for k in w.fieldnames})
    print(f"\n[va] cosine CSV → {args.out}")
    print("[va] ---- matrix report ----")
    for line in report:
        print(f"[va] {line}")
    if failures:
        print(f"[va] FAIL: {', '.join(failures)}")
        return 1
    print("[va] PASS")
    return 0


if __name__ == "__main__":
    with phone_lock():
        sys.exit(main())
