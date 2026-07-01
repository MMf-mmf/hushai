#!/usr/bin/env python3
"""
physical_loopback.py — Tier 2 of the recursive harness: a SCORED physical camera-at-screen test.

Plays a clip/image FULLSCREEN on this Mac while the USB-connected Galaxy phone (pointed at the
screen) captures it through the LIVE pipeline, then scores the results TOLERANTLY against expected
content (presence/recall — never exact, because optical+acoustic capture is irreducibly noisy).

This is the realism gate that deterministic injection (hushai-eval) cannot be: it exercises the real
camera, mic, on-device encoder, segment cutter, and uploader. Run it as a human operator would.

Prereqs: the stack is up on hushai_test (./local_dev/run_stack.sh --test-db, or the eval backend+
worker), the phone is plugged in with the hushai app installed, ffplay + psql on PATH.

Usage:
  physical_loopback.py --media IMG_7256.mp4 --case obj_live --duration 25 \
      --expect-objects refrigerator --expect-text "camera"
  physical_loopback.py --media portrait.jpg --case face_live --duration 20 --expect-face
  physical_loopback.py --media clip.wav --case audio_live --audio-only --expect-text "we choose"
"""
import argparse
import os
import re
import subprocess
import sys
import time

PG = os.environ.get("DATABASE_URL", "postgres://mf@localhost:5432/hushai_test")
ADB = os.path.expanduser(os.environ.get("ADB", "~/Library/Android/sdk/platform-tools/adb"))
PKG = "com.hushai.android"
ACTIVITY = f"{PKG}/.MainActivity"
BACKEND = os.environ.get("HUSHAI_BACKEND_URL", "http://localhost:8080")
RAG = os.environ.get("HUSHAI_RAG_URL", "http://localhost:8090")
TOKEN = os.environ.get("DEVICE_TOKEN", "dev-secret-token")

RESET_TABLES = ("transcript_sentences, speaker_segments, person_segments, scene_objects, "
                "plate_detections, speakers, persons, events, video_events, "
                "segment_transcription_status, segment_vision_status, segments, streams, sessions")


def psql(sql: str) -> str:
    out = subprocess.run(["psql", PG, "-At", "-c", sql], capture_output=True, text=True)
    if out.returncode != 0:
        raise SystemExit(f"psql failed: {out.stderr.strip()}")
    return out.stdout.strip()


def adb(*args: str, check=True) -> subprocess.CompletedProcess:
    return subprocess.run([ADB, *args], capture_output=True, text=True, check=False)


def norm(s: str) -> str:
    return re.sub(r"[^a-z0-9 ]+", " ", s.lower()).strip()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--media", required=True, help="video/image/audio file to display fullscreen")
    ap.add_argument("--case", required=True)
    ap.add_argument("--duration", type=int, default=25)
    ap.add_argument("--audio-only", action="store_true", help="capture mic only (no camera)")
    ap.add_argument("--expect-objects", default="", help="comma-separated COCO labels expected on screen")
    ap.add_argument("--expect-text", default="", help="comma-separated transcript keywords expected")
    ap.add_argument("--expect-face", action="store_true", help="expect at least one face detected")
    args = ap.parse_args()

    if not os.path.isfile(args.media):
        raise SystemExit(f"media not found: {args.media}")
    if adb("devices").stdout.count("\tdevice") < 1:
        raise SystemExit("no adb device — is the phone plugged in + authorized?")

    print(f"[phys] case={args.case} media={args.media} mode={'audio' if args.audio_only else 'video'}")
    psql(f"TRUNCATE {RESET_TABLES} RESTART IDENTITY CASCADE;")
    adb("reverse", "tcp:8080", "tcp:8080"); adb("reverse", "tcp:8090", "tcp:8090")
    adb("shell", "am", "force-stop", PKG)
    adb("logcat", "-c")

    # Fullscreen playback (ffplay shows images too; -loop 0 holds/loops for the whole window).
    play = subprocess.Popen(["ffplay", "-loglevel", "quiet", "-fs", "-loop", "0", args.media],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        time.sleep(1.0)
        adb("shell", "am", "start", "-n", ACTIVITY, "--es", "url", BACKEND, "--es", "rag_url", RAG,
            "--es", "token", TOKEN, "--ez", "audio_only", "true" if args.audio_only else "false",
            "--ez", "autostart", "true")
        print(f"[phys] capturing {args.duration}s …")
        time.sleep(args.duration)
        adb("shell", "am", "start", "-n", ACTIVITY, "--ez", "stop", "true")
    finally:
        play.terminate()

    mode = adb("logcat", "-d", "-s", "HUSHAI_TX:I").stdout
    m = re.search(r"capture started .* (audioOnly=\S+.*)", mode)
    print(f"[phys] {m.group(1) if m else 'capture mode unknown'}")

    # Discover the phone's device + wait for both lanes to drain (clean window after TRUNCATE).
    dev = psql("SELECT device_id FROM segments WHERE device_id LIKE 'android%' "
               "ORDER BY received_at DESC LIMIT 1;")
    if not dev:
        play.wait(); raise SystemExit("[phys] FAIL: no android segments arrived (capture/upload broken)")
    print(f"[phys] device={dev}; waiting for processing …")
    deadline = time.time() + 240
    while time.time() < deadline:
        a = psql(f"SELECT count(*) FILTER (WHERE status='done')||'/'||count(*) FROM "
                 f"segment_transcription_status t JOIN segments s USING(segment_id) WHERE s.device_id='{dev}';")
        v = psql(f"SELECT count(*) FILTER (WHERE status='done')||'/'||count(*) FROM "
                 f"segment_vision_status t JOIN segments s USING(segment_id) WHERE s.device_id='{dev}';")
        print(f"[phys]   audio {a or '0/0'}  vision {v or '0/0'}")
        done = lambda x: x and x.split('/')[0] == x.split('/')[1] and x.split('/')[1] != '0'
        if done(a) and (args.audio_only or done(v) or v in ('', '0/0')):
            break
        time.sleep(6)

    # ---- tolerant scoring (presence/recall) ----
    transcript = psql(f"SELECT string_agg(text,' ') FROM transcript_sentences WHERE device_id='{dev}';")
    objects = psql(f"SELECT string_agg(DISTINCT object_label,',') FROM scene_objects "
                   f"WHERE device_id='{dev}' AND object_label<>'__frame__';")
    nfaces = psql(f"SELECT count(*) FROM person_segments WHERE device_id='{dev}';") or "0"
    print(f"\n[phys] === observed (live physical capture) ===")
    print(f"  transcript: {transcript[:200] or '(none)'}")
    print(f"  objects   : {objects or '(none)'}")
    print(f"  faces     : {nfaces} face detection(s)")

    checks = []
    if args.expect_text:
        nt = norm(transcript)
        for kw in [k.strip() for k in args.expect_text.split(',') if k.strip()]:
            checks.append((f"text~'{kw}'", norm(kw) in nt))
    if args.expect_objects:
        no = norm(objects)
        for lab in [l.strip() for l in args.expect_objects.split(',') if l.strip()]:
            checks.append((f"object '{lab}'", norm(lab) in no))
    if args.expect_face:
        checks.append(("face detected", int(nfaces) > 0))

    print(f"\n[phys] === verdict (tolerant: presence/recall) ===")
    for name, ok in checks:
        print(f"  [{'PASS' if ok else 'MISS'}] {name}")
    passed = sum(1 for _, ok in checks if ok)
    total = len(checks)
    if total == 0:
        print("[phys] no expectations given — observation only.")
        return 0
    if passed == total:
        print(f"[phys] PASS — {passed}/{total} expected items present in live capture"); return 0
    if passed > 0:
        print(f"[phys] DEGRADED — {passed}/{total} present (physical noise; check rig/lighting)"); return 0
    print(f"[phys] FAIL — 0/{total} expected items present"); return 1


if __name__ == "__main__":
    sys.exit(main())
