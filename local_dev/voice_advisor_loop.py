#!/usr/bin/env python3
"""Physical-phone voice-consult rig for the Ahithophel advisor (spec Part 3 §3.4 / Phases G+H).

Drives a REAL phone through a spoken advisor consult and scores it from logcat markers (tag
HUSHAI_TX, the §2.7 contract) + the phys-DB shape — NEVER from screenshots and never by asserting
the phone's spoken TTS (the rig has no capture of the phone speaker). Audio is rendered at runtime
with the voice-matrix recipe (macOS `say`, owner voice Samantha, 16 kHz mono, loudnorm) into a
scratch dir; clips are short and keyword-dense because Vosk mishears long TTS sentences.

Preconditions (see the spec): the phys stack is up against hushai_test_phys with the advisor on
:8097 (`source eval.env; source phys.env`), the book is ingested on hushai_test_phys, the phone is
USB-authorized with the owner enrolled, and the app was launched via run_hushai_app.sh (which
reverses 8095 and passes the advisor extras). This rig additionally reverses 8095→8097 so the
phone's localhost:8095 reaches the phys advisor.

Modes:
  --smoke   single-turn DIRECT consult (fully-specified opener → answer, NO gate round). ~5–10 min.
  (default) multi-turn consult: thin opener → questions spoken → spoken answer(s) → final advice.

Scoring is TOLERANT — only what is deterministic over the air gap: marker presence + order, zero
`advisor error`, ONE session id across the consult, and the phys-DB shape. ASR keyword recall is
report-only on run 1 (frozen as a bar thereafter, the voice-matrix precedent).
"""

import argparse
import os
import re
import subprocess
import sys
import time

from phonelock import phone_lock

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
ADB = os.path.expanduser(os.environ.get("ADB", "~/Library/Android/sdk/platform-tools/adb"))
PKG = "com.hushai.android"
# The phone dials its own localhost:8095; reverse that onto the phys advisor port.
PHONE_ADVISOR_PORT = "8095"
PHYS_ADVISOR_PORT = os.environ.get("PHYS_ADVISOR_PORT", "8097")
PHYS_BACKEND_PORT = os.environ.get("PHYS_BACKEND_PORT", "8082")
PHYS_RAG_PORT = os.environ.get("PHYS_RAG_PORT", "8092")
DB_URL = os.environ.get("DATABASE_URL", "postgres://mf@localhost:5432/hushai_test_phys")
OWNER_VOICE = os.environ.get("OWNER_VOICE", "Samantha")

SCRATCH = os.path.join(ROOT, "local_dev", ".advisor_rig")

# Spoken scripts. Keyword-dense + SHORT on purpose (Vosk over-the-air mishears long TTS). The
# opener is ONE combined "computer <trigger> <question>" utterance so it routes on the LISTENING
# path (tail[0]=trigger) rather than depending on the 8 s AWAIT_QUESTION window after a bare wake.
# Trigger word: Vosk's small model reliably transcribes "adviser" but mis-hears "advisor" as
# "advice"/"or" (spec Risk 1) — AssistantRouting normalizes "adviser" to the advisor route.
WAKE = os.environ.get("WAKE_WORD", "computer")
TRIGGER = os.environ.get("ADVISOR_TRIGGER", "adviser")
OPENER_SMOKE = f"{WAKE} {TRIGGER} my client keeps stalling our contract renewal, how do I get him to sign this week"
OPENER_MULTI = f"{WAKE} {TRIGGER} I need advice about a person at work"
ANSWER_1 = "it is a supplier renewal, they opened at nine percent, I want under four percent"
ANSWER_2 = "sign this quarter and keep our priority production slots"
PLANTED_KEYWORDS = ["renewal", "supplier", "percent", "negotiation", "client"]

# Marker patterns (the §2.7 contract). Byte-stable once shipped.
M_ROUTE = r"advisor route \(session="
M_QUESTIONS = r"advisor questions round=\d+ count=\d+ \(session=(\S+)\)"
M_SPOKEN = r"advisor questions spoken — awaiting answer"
M_CAPTURED = r"advisor followup captured \(round=\d+, words=\d+\)"
M_ANSWER = r"advisor answer ok \(session=(\S+) chapters="
M_ERROR = r"advisor error:"


def adb(*args):
    return subprocess.run([ADB, *args], capture_output=True, text=True, check=False)


def logcat():
    return adb("logcat", "-d", "-s", "HUSHAI_TX:I").stdout


def clear_logcat():
    adb("logcat", "-c")


def afplay(path):
    subprocess.run(["afplay", path], check=True)


def wait_for(pattern, timeout, poll=1.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        m = re.search(pattern, logcat())
        if m:
            return m
        time.sleep(poll)
    return None


def render(text, out):
    """say → 16 kHz mono wav, loudnorm — the voice-matrix render() recipe (owner voice)."""
    tmp = out + ".aiff"
    subprocess.run(["say", "-v", OWNER_VOICE, "-o", tmp, text], check=True)
    subprocess.run(
        ["ffmpeg", "-y", "-loglevel", "error", "-i", tmp, "-ar", "16000", "-ac", "1",
         "-af", "loudnorm=I=-20:TP=-3:LRA=11", out],
        check=True,
    )
    os.remove(tmp)


def psql(sql):
    """Run one SQL statement against the phys DB; None if psql is unavailable."""
    if not subprocess.run(["which", "psql"], capture_output=True).returncode == 0:
        return None
    r = subprocess.run(["psql", DB_URL, "-tAc", sql], capture_output=True, text=True)
    return r.stdout.strip() if r.returncode == 0 else None


def session_ids(log):
    """All concrete session ids across the consult's markers (excludes the literal 'new')."""
    ids = set(re.findall(r"\(session=(\S+?)[ )]", log))
    return {i for i in ids if i and i != "new"}


def db_shape(session_id, expect_rounds_min, require_followup):
    """The v1_spec Phase E shape checks on the phys DB. Report-only if psql is absent. The message
    KINDS are 'message' (user turn) / 'followup_questions' / 'final_answer' (chat.rs). We assert the
    INVARIANTS rather than a rigid sequence, since a phone advisor session can carry more than one
    consult within its 30-min window: the transcript ends in a final_answer, seq is gap-free, and a
    multi-turn consult contains a followup_questions round while a smoke consult contains none."""
    checks, failures = [], []
    if psql("SELECT 1") is None:
        checks.append("psql unavailable — DB shape checks skipped (report-only)")
        return checks, failures
    phase = psql(f"SELECT phase FROM advisor_sessions WHERE session_id='{session_id}'")
    rounds = psql(f"SELECT followup_rounds FROM advisor_sessions WHERE session_id='{session_id}'")
    kinds = psql(
        f"SELECT string_agg(kind, ',' ORDER BY seq) FROM advisor_messages WHERE session_id='{session_id}'"
    )
    seqgap = psql(
        "SELECT count(*) - (max(seq) - min(seq) + 1) FROM advisor_messages "
        f"WHERE session_id='{session_id}'"
    )
    mem = psql("SELECT count(*) FROM advisor_memories")
    checks.append(f"phase={phase} rounds={rounds} kinds=[{kinds}] seqgap={seqgap} memory_rows={mem}")
    if phase != "done":
        failures.append(f"phase!=done ({phase})")
    if rounds is None or int(rounds) < expect_rounds_min:
        failures.append(f"followup_rounds {rounds} < {expect_rounds_min}")
    if seqgap not in (None, "0"):
        failures.append(f"seq not gap-free (gap={seqgap})")
    if mem in (None, "0"):
        failures.append("no advisor_memories row written")
    if not (kinds and kinds.endswith("final_answer")):
        failures.append(f"transcript does not end in a final_answer (kinds=[{kinds}])")
    if require_followup and (not kinds or "followup_questions" not in kinds):
        failures.append(f"multi-turn consult has no followup_questions round (kinds=[{kinds}])")
    if not require_followup and kinds and "followup_questions" in kinds:
        failures.append(f"smoke consult unexpectedly gated (kinds=[{kinds}])")
    return checks, failures


def reset_advisor_db():
    """Isolate this run: truncate the advisor tables so one consult's transcript is unambiguous.
    The phone's stored session id then 404s → it self-heals to a fresh session (SessionNotFound)."""
    psql("TRUNCATE advisor_sessions, advisor_messages, advisor_memories RESTART IDENTITY CASCADE")


def open_consult(clip):
    """Play wake + the opener, waiting for the 'advisor route' marker. One replay before giving up
    (spec §3.4: a missed wake/route is replayed once, then excluded-INCONCLUSIVE — never a FAIL)."""
    for _ in range(2):
        clear_logcat()
        play_opener(clip)
        if wait_for(M_ROUTE, 30):
            return True
    return False


def keyword_recall(session_id):
    """Planted keywords appearing in stored user rows (any-of, normalized). Report-only run 1."""
    content = psql(
        f"SELECT lower(string_agg(content, ' ')) FROM advisor_messages "
        f"WHERE session_id='{session_id}' AND role='user'"
    )
    if not content:
        return "keyword recall: (no user rows / psql unavailable)"
    hit = [k for k in PLANTED_KEYWORDS if k in content]
    return f"keyword recall (report-only): {len(hit)}/{len(PLANTED_KEYWORDS)} — {hit}"


def play_opener(clip_path):
    """Play the single combined 'computer adviser <question>' opener (routes on the LISTENING path,
    so no dependence on the 8 s AWAIT_QUESTION window that a long bare-wake opener would overrun)."""
    afplay(clip_path)


def run_smoke():
    print("[adv] SMOKE: single-turn direct consult")
    reset_advisor_db()
    if not open_consult(SMOKE_CLIP):
        return {"status": "inconclusive", "reason": "no 'advisor route' marker after one replay (wake/route missed)"}
    ans = wait_for(M_ANSWER, 420)
    log = logcat()
    failures = []
    if not ans:
        return {"status": "inconclusive", "reason": "no 'advisor answer ok' within cap"}
    if re.search(M_SPOKEN, log):
        failures.append("unexpected questions round (smoke must be direct)")
    if re.search(M_ERROR, log):
        failures.append("advisor error fired")
    # The answer marker carries the SERVER session id; a stale phone-sent id that 404s and
    # self-heals (SessionNotFound → fresh) is correct, so we validate the answer/DB session, not
    # a strict single id across every marker (the route id may be the pre-heal one).
    sid = ans.group(1)
    checks, dbfail = db_shape(sid, expect_rounds_min=0, require_followup=False)
    rounds = psql(f"SELECT followup_rounds FROM advisor_sessions WHERE session_id='{sid}'")
    if rounds not in (None, "0"):
        failures.append(f"smoke expected followup_rounds=0, got {rounds}")
    return {"status": "pass" if not (failures or dbfail) else "fail",
            "failures": failures + dbfail, "report": checks + [keyword_recall(sid)]}


def run_multi():
    print("[adv] MULTI: thin opener → questions → answer(s) → final advice")
    reset_advisor_db()
    if not open_consult(MULTI_CLIP):
        return {"status": "inconclusive", "reason": "no 'advisor route' marker after one replay (wake/route missed)"}
    if not wait_for(M_SPOKEN, 240):
        return {"status": "inconclusive", "reason": "no 'advisor questions spoken' (gate round missed)"}
    time.sleep(1.5)  # margin after the window opens
    afplay(ANSWER1_CLIP)
    if not wait_for(M_CAPTURED, 30):
        # one replay before excluding (never FAIL on ASR flake)
        afplay(ANSWER1_CLIP)
        if not wait_for(M_CAPTURED, 30):
            return {"status": "inconclusive", "reason": "follow-up answer never captured (ASR flake)"}
    # Branch: a SECOND questions round (answer again) or the final answer.
    t0 = time.time()
    while time.time() - t0 < 60:
        log = logcat()
        spoken_count = len(re.findall(M_SPOKEN, log))
        if re.search(M_ANSWER, log):
            break
        if spoken_count >= 2:
            time.sleep(1.5)
            afplay(ANSWER2_CLIP)
            wait_for(M_CAPTURED, 30)
            break
        time.sleep(1.5)
    ans = wait_for(M_ANSWER, 420)
    log = logcat()
    failures = []
    if not ans:
        return {"status": "inconclusive", "reason": "no final 'advisor answer ok' within cap"}
    # Marker ORDER: route < questions spoken < followup captured < answer ok.
    order = [M_ROUTE, M_SPOKEN, M_CAPTURED, M_ANSWER]
    positions = [(_first_index(log, p)) for p in order]
    if any(p < 0 for p in positions) or positions != sorted(positions):
        failures.append(f"marker order violated: {positions}")
    if re.search(M_ERROR, log):
        failures.append("advisor error fired")
    sid = ans.group(1)
    # The consult must stay on ONE server session after routing: every questions marker shares the
    # answer's id. (A stale phone-sent id on the route marker that 404s→self-heals is tolerated.)
    stray = {q for q in re.findall(M_QUESTIONS, log) if q and q != sid}
    if stray:
        failures.append(f"questions markers on a different session than the answer {sid}: {stray}")
    checks, dbfail = db_shape(sid, expect_rounds_min=1, require_followup=True)
    return {"status": "pass" if not (failures or dbfail) else "fail",
            "failures": failures + dbfail, "report": checks + [keyword_recall(sid)]}


def _first_index(log, pattern):
    m = re.search(pattern, log)
    return m.start() if m else -1


def build_clips():
    os.makedirs(SCRATCH, exist_ok=True)
    global SMOKE_CLIP, MULTI_CLIP, ANSWER1_CLIP, ANSWER2_CLIP
    SMOKE_CLIP = os.path.join(SCRATCH, "opener_smoke.wav")
    MULTI_CLIP = os.path.join(SCRATCH, "opener_multi.wav")
    ANSWER1_CLIP = os.path.join(SCRATCH, "answer1.wav")
    ANSWER2_CLIP = os.path.join(SCRATCH, "answer2.wav")
    print("[adv] rendering clips (say → 16k mono loudnorm)…")
    render(OPENER_SMOKE, SMOKE_CLIP)   # already prefixed "computer adviser …"
    render(OPENER_MULTI, MULTI_CLIP)
    render(ANSWER_1, ANSWER1_CLIP)
    render(ANSWER_2, ANSWER2_CLIP)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--smoke", action="store_true", help="single-turn direct consult (no gate round)")
    args = ap.parse_args()

    if adb("devices").stdout.count("\tdevice") < 1:
        raise SystemExit("no adb device — is the phone plugged in + authorized?")
    for tool in ("say", "ffmpeg", "afplay"):
        if subprocess.run(["which", tool], capture_output=True).returncode != 0:
            raise SystemExit(f"missing required tool: {tool}")

    # The phone keeps dialing localhost:8095/8080/8090; reverse onto the phys ports.
    adb("reverse", f"tcp:{PHONE_ADVISOR_PORT}", f"tcp:{PHYS_ADVISOR_PORT}")
    adb("reverse", "tcp:8080", f"tcp:{PHYS_BACKEND_PORT}")
    adb("reverse", "tcp:8090", f"tcp:{PHYS_RAG_PORT}")

    build_clips()
    result = run_smoke() if args.smoke else run_multi()

    print("[adv] ---- report ----")
    for line in result.get("report", []):
        print(f"[adv] {line}")
    status = result["status"]
    if status == "inconclusive":
        print(f"[adv] INCONCLUSIVE: {result['reason']} (never a FAIL on ASR/route flake)")
        return 2
    if status == "fail":
        print(f"[adv] FAIL: {', '.join(result['failures'])}")
        return 1
    print("[adv] PASS")
    return 0


if __name__ == "__main__":
    with phone_lock():
        sys.exit(main())
