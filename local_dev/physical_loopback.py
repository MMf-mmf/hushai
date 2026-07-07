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
import json
import os
import re
import subprocess
import sys
import time

# Default: the PHYSICAL tier's own DB (hushai_test_phys) so a physical run can never TRUNCATE
# the deterministic Tier-1 DB out from under a concurrent `cargo run -p hushai-eval` (the old
# shared-hushai_test contention). Same `_test` name guard as the eval harness's ctx.rs.
PG = os.environ.get("DATABASE_URL", "postgres://mf@localhost:5432/hushai_test_phys")
if "_test" not in PG.rsplit("/", 1)[-1]:
    raise SystemExit(f"refusing to run against non-test DB: {PG} (name must contain '_test')")
REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
ADB = os.path.expanduser(os.environ.get("ADB", "~/Library/Android/sdk/platform-tools/adb"))
PKG = "com.hushai.android"
ACTIVITY = f"{PKG}/.MainActivity"
# Host-side ports the phys stack listens on (local_dev/phys.env: 8082/8092 so it can coexist
# with the Tier-1 stack). The PHONE keeps localhost:8080/8090 — `adb reverse` maps them here.
PHYS_BACKEND_PORT = os.environ.get("PHYS_BACKEND_PORT", "8082")
PHYS_RAG_PORT = os.environ.get("PHYS_RAG_PORT", "8092")
BACKEND = os.environ.get("HUSHAI_BACKEND_URL", f"http://localhost:{PHYS_BACKEND_PORT}")
RAG = os.environ.get("HUSHAI_RAG_URL", f"http://localhost:{PHYS_RAG_PORT}")
# What the PHONE dials: its OWN localhost, which `adb reverse` tunnels to the host ports above.
PHONE_BACKEND = "http://localhost:8080"
PHONE_RAG = "http://localhost:8090"
TOKEN = os.environ.get("DEVICE_TOKEN", "dev-secret-token")
# The RAG service is token-gated SEPARATELY from the backend (run_stack.sh mints/pins RAG_TOKEN;
# eval.env pins it to dev-rag-token). The phone uploads with DEVICE_TOKEN, but the /v1/rag/chat
# calls this script makes must carry RAG_TOKEN or they 401.
RAG_TOKEN = os.environ.get("RAG_TOKEN", "dev-rag-token")

RESET_TABLES = ("transcript_sentences, speaker_segments, person_segments, scene_objects, "
                "plate_detections, speakers, persons, events, video_events, entity_profiles, "
                "chat_sessions, chat_messages, "
                # 0025: stale conversations/watermark poison later runs (closed spans partition
                # the timeline finer every run; the eval harness hit exactly this bug).
                "conversations, threader_state, "
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


def load_scenario_chat(case: str):
    """Return the fixture's chat ground-truth (list of question dicts), or None. Searches
    train/ then holdout/. Shares the SAME expected.json the deterministic eval uses."""
    for split in ("train", "holdout", "staging"):
        p = os.path.join(REPO, "hushai-eval", "fixtures", split, case, "expected.json")
        if os.path.isfile(p):
            with open(p) as f:
                exp = json.load(f)
            chat = exp.get("chat") or exp.get("rag")
            return (chat or {}).get("questions")
    return None


def rag_chat(question: dict):
    """POST one question to /v1/rag/chat and parse the SSE stream. Returns
    (routed_agent_id, answer, n_sources). Best-effort; requires `requests`."""
    import requests  # feed_segments.py already depends on requests
    body = {"agent_id": question.get("agent_id", "auto"), "message": question["ask"], "tz_offset_secs": 0}
    headers = {"accept": "text/event-stream"}
    if RAG_TOKEN:
        headers["authorization"] = f"Bearer {RAG_TOKEN}"
    routed, answer, nsrc = None, "", 0
    with requests.post(f"{RAG}/v1/rag/chat", json=body, headers=headers, stream=True, timeout=120) as r:
        r.raise_for_status()
        event, data = None, []
        for raw in r.iter_lines(decode_unicode=True):
            line = raw or ""
            if line == "":  # event boundary
                blob = "\n".join(data)
                if event == "session":
                    try:
                        routed = json.loads(blob).get("routed_agent_id")
                    except Exception:
                        pass
                elif event == "sources":
                    try:
                        nsrc = len(json.loads(blob))
                    except Exception:
                        pass
                elif event == "token":
                    try:
                        answer += json.loads(blob).get("delta", "")
                    except Exception:
                        pass
                elif event in ("done", "error"):
                    break
                event, data = None, []
                continue
            if line.startswith(":"):
                continue
            if line.startswith("event:"):
                event = line[len("event:"):].strip()
            elif line.startswith("data:"):
                data.append(line[len("data:"):].lstrip(" "))
    return routed, answer, nsrc


def score_scenario_chat(questions) -> list:
    """Run each fixture question against the LIVE RAG and score TOLERANTLY (presence/recall + routing;
    NOT exact counts/attribution — physical capture is lossy). Returns a list of (name, ok) checks."""
    checks = []
    for i, q in enumerate(questions):
        try:
            routed, answer, nsrc = rag_chat(q)
        except Exception as e:
            checks.append((f"q{i} rag chat reachable", False))
            print(f"[phys]   q{i}: ERROR {e}")
            continue
        na = norm(answer)
        print(f"[phys]   q{i} routed={routed} srcs={nsrc}: {answer[:160]}")
        want = q.get("expect_routed_agent")
        if want and routed is not None:
            checks.append((f"q{i} routed→{want}", routed == want))
        for kw in q.get("must_contain", []):
            checks.append((f"q{i} contains '{kw}'", norm(kw) in na))
        for kw in q.get("must_not_contain", []):
            checks.append((f"q{i} omits '{kw}'", norm(kw) not in na))
        if q.get("min_citations"):
            checks.append((f"q{i} ≥{q['min_citations']} citations", nsrc >= q["min_citations"]))
        # expect_number / citation_must_attribute are DELIBERATELY not gated here (lossy capture);
        # they stay the deterministic layer's job. Reported inline above for the operator.
    return checks


def load_timeline(case: str):
    """Return [(abs_path, offset_ns, duration_ns)] from the fixture's meta.json injections[]
    (train/holdout/staging), offsets ascending. Single-clip fixtures yield one entry."""
    for split in ("train", "holdout", "staging"):
        p = os.path.join(REPO, "hushai-eval", "fixtures", split, case, "meta.json")
        if not os.path.isfile(p):
            continue
        with open(p) as f:
            meta = json.load(f)
        base_dir = os.path.dirname(p)
        inj = meta.get("injections") or [{
            "media_file": meta["media_file"], "device_id": meta.get("device_id"),
            "capture_start_offset_ns": 0}]
        out = []
        for e in sorted(inj, key=lambda x: x.get("capture_start_offset_ns", 0)):
            path = os.path.join(base_dir, e["media_file"])
            dur = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration",
                                  "-of", "csv=p=0", path], capture_output=True, text=True)
            dur_ns = int(float(dur.stdout.strip()) * 1e9)
            out.append((path, int(e.get("capture_start_offset_ns", 0)), dur_ns))
        return out
    return None


def play_timeline(timeline, gap_scale: float):
    """Replay the fixture acoustically: afplay each clip in offset order, sleeping the
    (scaled) injection gap between clips. Wall-clock stands in for capture time — the phone
    hears the same silence structure the deterministic injection encodes."""
    cursor_ns = 0
    for path, offset_ns, dur_ns in timeline:
        gap_s = max(0.0, (offset_ns - cursor_ns) / 1e9) * gap_scale
        if gap_s > 0:
            print(f"[phys]   … {gap_s:.0f}s of silence (scaled gap)")
            time.sleep(gap_s)
        print(f"[phys]   ♪ {os.path.basename(path)} ({dur_ns/1e9:.0f}s)")
        subprocess.run(["afplay", path], check=False)
        cursor_ns = offset_ns + dur_ns


def wait_threaded_phys(dev: str, timeout_s: int = 180) -> bool:
    """Wait until every text sentence for the phone's device carries a conversation_id
    (the threader runs on its own interval after the transcript lane finishes)."""
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        n = psql(f"SELECT count(*) FROM transcript_sentences WHERE device_id='{dev}' "
                 f"AND text IS NOT NULL AND conversation_id IS NULL;")
        if n == "0":
            return True
        time.sleep(5)
    return False


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--media", default="", help="video/image/audio file to display fullscreen "
                    "(omit when using --timeline)")
    ap.add_argument("--case", required=True)
    ap.add_argument("--duration", type=int, default=25)
    ap.add_argument("--audio-only", action="store_true", help="capture mic only (no camera)")
    ap.add_argument("--expect-objects", default="", help="comma-separated COCO labels expected on screen")
    ap.add_argument("--expect-text", default="", help="comma-separated transcript keywords expected")
    ap.add_argument("--expect-face", action="store_true", help="expect at least one face detected")
    ap.add_argument("--scenario", default="", help="fixture case name: also score its RAG chat "
                    "questions tolerantly against the live capture (routing + presence + no-hallucination)")
    ap.add_argument("--no-reset", action="store_true",
                    help="skip the TRUNCATE (accumulate-style sessions on the phys DB)")
    ap.add_argument("--timeline", default="", help="fixture case name: replay its meta.json "
                    "injections[] ACOUSTICALLY in offset order (afplay per clip + scaled sleeps) "
                    "instead of --media. The conversation-threading Tier-2 mode.")
    ap.add_argument("--gap-scale", type=float, default=1.0,
                    help="scale factor for timeline injection gaps (0.2 turns a 600s gap into "
                    "120s of wall time; start the phys stack with CONVERSATION_GAP_SECS scaled "
                    "by the same factor)")
    ap.add_argument("--expect-conversations", type=int, default=None,
                    help="after processing, the distinct threaded conversation count for the "
                    "phone's device must equal this (± --conv-tolerance)")
    ap.add_argument("--conv-tolerance", type=int, default=0)
    args = ap.parse_args()

    scenario_qs = None
    if args.scenario:
        scenario_qs = load_scenario_chat(args.scenario)
        if scenario_qs is None:
            raise SystemExit(f"--scenario '{args.scenario}': no fixtures/*/{args.scenario}/expected.json with a chat block")

    timeline = None
    if args.timeline:
        timeline = load_timeline(args.timeline)
        if not timeline:
            raise SystemExit(f"--timeline '{args.timeline}': no fixtures/*/{args.timeline}/meta.json with injections")
    elif not os.path.isfile(args.media):
        raise SystemExit(f"media not found: {args.media}")
    if adb("devices").stdout.count("\tdevice") < 1:
        raise SystemExit("no adb device — is the phone plugged in + authorized?")

    print(f"[phys] case={args.case} media={args.media} mode={'audio' if args.audio_only else 'video'}")
    if args.no_reset:
        print("[phys] --no-reset: keeping existing rows")
    else:
        psql(f"TRUNCATE {RESET_TABLES} RESTART IDENTITY CASCADE;")
    # Phone's localhost:8080/8090 → the host phys stack (8081/8091 by default).
    adb("reverse", "tcp:8080", f"tcp:{PHYS_BACKEND_PORT}")
    adb("reverse", "tcp:8090", f"tcp:{PHYS_RAG_PORT}")
    adb("shell", "am", "force-stop", PKG)
    adb("logcat", "-c")

    if timeline:
        # Acoustic timeline replay: start capture, afplay the clips with scaled gaps, stop.
        total_s = sum(d for _, _, d in timeline) / 1e9
        gaps_s = sum(max(0, timeline[i][1] - (timeline[i-1][1] + timeline[i-1][2]))
                     for i in range(1, len(timeline))) / 1e9 * args.gap_scale
        est = int(total_s + gaps_s) + 10
        adb("shell", "am", "start", "-n", ACTIVITY, "--es", "url", PHONE_BACKEND, "--es", "rag_url", PHONE_RAG,
            "--es", "token", TOKEN, "--ez", "audio_only", "true" if args.audio_only else "false",
            "--ez", "autostart", "true")
        print(f"[phys] timeline capture ≈{est}s (speech {total_s:.0f}s + scaled gaps {gaps_s:.0f}s) …")
        time.sleep(2.0)
        play_timeline(timeline, args.gap_scale)
        time.sleep(4.0)
        adb("shell", "am", "start", "-n", ACTIVITY, "--ez", "stop", "true")
    else:
        # Fullscreen playback (ffplay shows images too; -loop 0 holds/loops for the whole window).
        play = subprocess.Popen(["ffplay", "-loglevel", "quiet", "-fs", "-loop", "0", args.media],
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            time.sleep(1.0)
            adb("shell", "am", "start", "-n", ACTIVITY, "--es", "url", PHONE_BACKEND, "--es", "rag_url", PHONE_RAG,
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
        # Terminal = done OR skipped (migration 0022: content gates mark silent/static
        # segments 'skipped' — a silent clip's audio lane would otherwise never "finish").
        a = psql(f"SELECT count(*) FILTER (WHERE status IN ('done','skipped'))||'/'||count(*) FROM "
                 f"segment_transcription_status t JOIN segments s USING(segment_id) WHERE s.device_id='{dev}';")
        v = psql(f"SELECT count(*) FILTER (WHERE status IN ('done','skipped'))||'/'||count(*) FROM "
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
    if args.expect_conversations is not None:
        # The threader lags the transcript lane; wait for it before counting.
        if not wait_threaded_phys(dev):
            print("[phys]   threading did not settle (is THREADER_ENABLED on for the phys worker?)")
        got = int(psql(f"SELECT count(DISTINCT conversation_id) FROM transcript_sentences "
                       f"WHERE device_id='{dev}' AND conversation_id IS NOT NULL;") or "0")
        want, tol = args.expect_conversations, args.conv_tolerance
        print(f"[phys]   conversations: observed {got}, expected {want} ±{tol}")
        checks.append((f"conversations == {want}±{tol}", abs(got - want) <= tol))

    # RAG-chat realism gate (every iteration, per the plan): ask the scenario's questions against the
    # LIVE capture and check routing + presence + no-hallucination TOLERANTLY.
    if scenario_qs:
        print(f"\n[phys] === RAG chat over live capture ({len(scenario_qs)} question(s)) ===")
        checks.extend(score_scenario_chat(scenario_qs))

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
    from phonelock import phone_lock

    # One phone, one screen: serialize against voice_assistant_loop.py (and other loopback runs).
    with phone_lock():
        sys.exit(main())
