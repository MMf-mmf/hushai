# hushai-advisor v1 — Live Real-Stack Verification Spec

Executable, end-to-end acceptance run for the Ahithophel advisor v1. Every step was
executed and passed on 2026-07-06 (macOS, Apple Silicon, branch
`feat/hushai-voice-assistant`); expected outputs below are from that run. Execute the
phases in order — later phases depend on earlier ones. Total wall-clock: ~25 min on a
cold corpus (~15 min of that is the one-time ingest), ~10 min on a warm one.

**Scope.** Verifies the advisor SERVICE against the real stack: real Postgres, real
Ollama models, real ingested book corpus, real SSE consultations. It does NOT cover the
Android client, the viewer, or advice *quality* beyond structural grounding checks
(quality is the eval harness's job, Phase H).

**Conventions.** Run everything from the repo root
(`/Users/mf/Documents/Rust_Code/Rig AI Agent` — adjust if relocated). Each phase ends
with explicit **PASS** criteria; a failed criterion stops the run. `psql` commands
assume `DATABASE_URL` in the root `.env`; extract it once:

```bash
cd "/Users/mf/Documents/Rust_Code/Rig AI Agent"
DBURL=$(grep -E "^DATABASE_URL" .env | cut -d= -f2-)
```

---

## Phase 0 — Preconditions

Required up and reachable BEFORE starting:

| Dependency | Check | Expected |
|---|---|---|
| Postgres w/ pgvector | `psql "$DBURL" -tAc "SELECT 1"` | `1` |
| Ollama | `curl -s localhost:11434/api/tags` | JSON model list |
| `qwen2.5:7b` pulled | model list contains it | present (`ollama pull qwen2.5:7b` if not) |
| `mxbai-embed-large` pulled | model list contains it | present |
| Book texts on disk | `ls "Agent Ahithophel/books/chapters_text/" \| wc -l` | `50` (files `1.txt`…`50.txt`) |
| Port 8095 free | `lsof -nP -iTCP:8095 -sTCP:LISTEN` | empty |

Notes:
- The rest of the Hushai stack (backend :8080, worker, rag :8090) may be running or
  not — the advisor is independent of them. Do NOT stop a running dev stack for this.
- pgvector must support `hnsw.iterative_scan` (≥ 0.8 — already true for this repo's
  stack; `hushai-rag` uses the same GUC).
- Migrations 0026/0027 auto-apply on first advisor/ingest start; they are additive-only
  (no existing table is touched), so running against the dev DB is safe.

**PASS 0:** all six checks green.

---

## Phase A — Build + unit gates

```bash
SQLX_OFFLINE=true cargo build -p hushai-advisor
SQLX_OFFLINE=true cargo test  -p hushai-advisor --lib
SQLX_OFFLINE=true cargo clippy -p hushai-advisor 2>&1 | grep -c "hushai-advisor/src"
SQLX_OFFLINE=true cargo build --workspace
```

**PASS A:**
- build: `Finished` with no errors
- tests: `test result: ok. 11 passed; 0 failed` (clean.rs heuristics, llm.rs parsers,
  pipeline truncation)
- clippy: `0` warnings in `hushai-advisor/src`
- workspace build: `Finished` (proves the new migrations don't break sibling crates'
  embedded `sqlx::migrate!`)

---

## Phase B — Corpus ingest (one-time, idempotent)

```bash
SQLX_OFFLINE=true cargo run -q -p hushai-advisor --bin ingest-book -- \
  --dir "Agent Ahithophel/books/chapters_text" \
  --slug yes-50-ways \
  --title "Yes!, 50 Scientifically Proven Ways to Be Persuasive" \
  --author "Goldstein, Martin, Cialdini" \
  --no-llm-clean
```

⚠️ **Keep the comma in the title.** The running-head strip patterns derive from the
title's comma/colon parts — without the comma, lone `Yes!` page-top lines survive as
junk paragraphs in the embedded corpus (this exact mistake happened during the original
run and required a re-ingest).

`--no-llm-clean` = heuristic-only cleaning, ~13 min (the per-chapter synopsis LLM call
dominates, ~15 s each). Dropping the flag adds a guarded LLM copy-edit pass per chapter
(fixes OCR letter-misreads like "Calleen"→"Colleen"; roughly triples runtime) — optional
for this spec, recommended before real use.

Expected final stdout line (cold corpus):

```
ingest complete: 50 chapters seen, 50 updated, 0 skipped (unchanged), ~255-260 chunks embedded, 0 LLM-clean fallbacks
```

DB assertions:

```bash
psql "$DBURL" -tAc "SELECT (SELECT count(*) FROM books),
                           (SELECT count(*) FROM book_chapters),
                           (SELECT count(*) FROM book_chunks);"
# expected: 1|50|~255-260

# No running-head junk survived cleaning:
psql "$DBURL" -tAc "SELECT count(*) FROM book_chapters WHERE clean_text LIKE E'%\n\nYes!\n\n%';"
# expected: 0

# Synopses exist for every chapter (the Traffic Controller's routing cards):
psql "$DBURL" -tAc "SELECT count(*) FROM book_chapters WHERE COALESCE(synopsis,'') = '';"
# expected: 0

# Spot-check quality by eye:
psql "$DBURL" -tAc "SELECT chapter_no, left(title,60), left(synopsis,90) FROM book_chapters WHERE chapter_no IN (1,7,50) ORDER BY chapter_no;"
```

Idempotency assertion — re-run the exact same ingest command:

```
ingest complete: 50 chapters seen, 0 updated, 50 skipped (unchanged), 0 chunks embedded, ...
```

and it completes in seconds (skip logic: unchanged clean_text + synopsis present +
chunks on the current embed model).

**PASS B:** counts as expected; running-head query returns 0; empty-synopsis query
returns 0; second run skips all 50.

Known accepted imperfection: a handful of chapters (e.g. 34, 44, 49, 50) have NULL
`title` — their opening question is OCR-mangled beyond the heuristic. Routing uses the
synopsis, so this does not fail the spec.

---

## Phase C — Service launch (incl. the fail-closed negative test)

**C1 — negative test first:** no token + non-loopback bind must REFUSE to start.
`env -u` strips any `ADVISOR_TOKEN` / `ADVISOR_ALLOW_INSECURE` the shell inherited
(e.g. from a sourced `eval.env` or a prior `run_stack.sh`) — otherwise the guard is
satisfied, the service *starts* world-bound on `0.0.0.0:8095`, and this negative test
would spuriously "fail" while leaving an unauthenticated advisor listening.

```bash
env -u ADVISOR_TOKEN -u ADVISOR_ALLOW_INSECURE \
  SQLX_OFFLINE=true ADVISOR_BIND_ADDR=0.0.0.0:8095 ./target/debug/hushai-advisor; echo "exit=$?"
```

Expected: exits non-zero quickly; log contains
`refusing to start: ADVISOR_TOKEN is unset while binding a non-loopback address`.

**C2 — real launch** (loopback + token; seed pinned for reproducible routing):

```bash
SQLX_OFFLINE=true \
ADVISOR_TOKEN=dev-advisor-token \
ADVISOR_BIND_ADDR=127.0.0.1:8095 \
ADVISOR_LLM_SEED=42 \
./target/debug/hushai-advisor > /tmp/advisor-test.log 2>&1 &
sleep 3
curl -s -o /dev/null -w "healthz: %{http_code}\n" http://127.0.0.1:8095/healthz   # 200
curl -s -o /dev/null -w "readyz:  %{http_code}\n" http://127.0.0.1:8095/readyz    # 200
curl -s http://127.0.0.1:8095/metrics | head -3                                    # Prometheus text
```

Startup log (`/tmp/advisor-test.log`) must show the redacted banner with
`llm_model=qwen2.5:7b embed_model=mxbai-embed-large num_ctx=16384` and NO token value.

**C3 — auth negative test:**

```bash
curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:8095/v1/advisor/sessions              # 401
curl -s -o /dev/null -w "%{http_code}\n" -H 'Authorization: Bearer wrong' \
  http://127.0.0.1:8095/v1/advisor/sessions                                                      # 401
```

**PASS C:** C1 refuses with that message; C2 healthz/readyz 200, banner clean;
C3 both 401.

---

## Phase D — Consultation flow (the core spec)

All requests: `-H 'content-type: application/json' -H 'Authorization: Bearer dev-advisor-token'`.
A full answering turn takes 30–90 s locally — use generous curl timeouts.

**D1 — insufficient input → the gate asks (Min-Info/Yenta path).**
The spec's own canonical example:

```bash
curl -sN --max-time 120 http://127.0.0.1:8095/v1/advisor/chat \
  -H 'content-type: application/json' -H 'Authorization: Bearer dev-advisor-token' \
  -d '{"message":"I walked into my house."}' | tee /tmp/turn1.sse
```

Required event sequence (assert PRESENCE and ORDER; question *wording* may vary):
1. `session` `{session_id, phase:"gathering"}` — first event; capture `session_id`
2. `phase` events
3. `questions` `{round:1, questions:[…]}` — at least one question, at most 3
4. `done` `{message_id}`
5. NO `token`, NO `chapters`, NO `error` events in this turn

**D2 — detailed follow-up → grounded streamed answer.**
Same `session_id` (replace `$SID`):

```bash
curl -sN --max-time 240 http://127.0.0.1:8095/v1/advisor/chat \
  -H 'content-type: application/json' -H 'Authorization: Bearer dev-advisor-token' \
  -d '{"session_id":"'$SID'","message":"I found my wife in bed with another man. We have been married 12 years and have two kids aged 8 and 10. Finances are stable, I run a small business. I am devastated but I want to understand my options and handle the next conversation with her without making things worse."}' \
  > /tmp/turn2.sse
grep -E "^event" /tmp/turn2.sse | sort | uniq -c
```

Required:
- exactly 1 `session`, ≥1 `chapters` (each with 1–6 `{no,title}` entries, `no` ∈ 1..50),
  many `token` events, exactly 1 `done`, 0 `error`
- `phase` heartbeats appear between stages (gathering/refining/recalling/routing/
  drafting/reviewing/polishing/memorizing) — the stream is never silent for a full stage
- ≤ 3 `chapters` events (the refine-loop cap; a second identical-set `chapters` event
  must NOT appear — set-growth convergence)
- concatenated `token` deltas form a coherent advice text (reference run: 383 tokens,
  opening "Given the complexity of your situation…"); it may cite chapters as `(ch. N)`

(If the gate asks ONE more round instead — allowed, `ADVISOR_MAX_FOLLOWUP_ROUNDS=2` —
answer its questions in a third turn; the final turn must then meet the criteria above.
The cap guarantees an answer by turn 3.)

**D3 — sufficient single message → NO questions.**
New session (omit `session_id`):

```bash
curl -sN --max-time 240 http://127.0.0.1:8095/v1/advisor/chat \
  -H 'content-type: application/json' -H 'Authorization: Bearer dev-advisor-token' \
  -d '{"message":"A longtime client keeps delaying signing our renewal contract. I want to persuade him to commit this week without damaging the relationship. He responds well to social cues and hates feeling pressured. What should I do?"}' \
  > /tmp/turn_direct.sse
grep -c "^event: questions" /tmp/turn_direct.sse   # expected: 0
grep -c "^event: token" /tmp/turn_direct.sse       # expected: > 50
```

**D4 — concurrent turn on a busy session → 409.**
Start a long turn in the background and capture its session id from the SSE `session`
event (the id only exists in-stream — a sessionless request mints it server-side):

```bash
curl -sN --max-time 240 http://127.0.0.1:8095/v1/advisor/chat \
  -H 'content-type: application/json' -H 'Authorization: Bearer dev-advisor-token' \
  -d '{"message":"A longtime client keeps delaying signing our renewal contract. I want to persuade him to commit this week without damaging the relationship. He responds well to social cues and hates feeling pressured. What should I do?"}' \
  > /tmp/turn_busy.sse &
# wait for the session event, then extract the id:
until grep -q '"session_id"' /tmp/turn_busy.sse 2>/dev/null; do sleep 0.3; done
BUSY_SID=$(grep -m1 '"session_id"' /tmp/turn_busy.sse | sed 's/.*"session_id":"\([^"]*\)".*/\1/')
```

Then, while that turn is still streaming (within ~8 s), fire the concurrent request:

```bash
curl -s -o /dev/null -w "%{http_code}\n" --max-time 10 http://127.0.0.1:8095/v1/advisor/chat \
  -H 'content-type: application/json' -H 'Authorization: Bearer dev-advisor-token' \
  -d '{"session_id":"'$BUSY_SID'","message":"hello again"}'
# expected: 409
```

The busy turn itself must still complete normally (its stream unaffected).

**D5 — input validation:**

```bash
# empty message
curl -s -o /dev/null -w "%{http_code}\n" -H 'Authorization: Bearer dev-advisor-token' \
  -H 'content-type: application/json' -d '{"message":"   "}' http://127.0.0.1:8095/v1/advisor/chat   # 400
# unknown session
curl -s -o /dev/null -w "%{http_code}\n" -H 'Authorization: Bearer dev-advisor-token' \
  -H 'content-type: application/json' \
  -d '{"session_id":"00000000-0000-7000-8000-000000000000","message":"hi"}' \
  http://127.0.0.1:8095/v1/advisor/chat                                                              # 404
```

**PASS D:** D1 questions-only turn; D2 grounded streamed answer with converging chapter
sets; D3 zero questions; D4 409 while busy + busy turn completes; D5 400/404.

---

## Phase E — Persistence + phase machine

Using D1/D2's `$SID`:

```bash
# Session listing shows the consultation, newest first, phase done:
curl -s -H 'Authorization: Bearer dev-advisor-token' http://127.0.0.1:8095/v1/advisor/sessions | python3 -m json.tool | head -12

# Transcript re-renders the FLOW, not just text:
curl -s -H 'Authorization: Bearer dev-advisor-token' \
  http://127.0.0.1:8095/v1/advisor/sessions/$SID/messages | python3 -c "
import json,sys
for m in json.load(sys.stdin):
    print(m['role'], m['kind'], [c['no'] for c in m['chapters']], m['content'][:60].replace(chr(10),' '))"
```

Expected message sequence for the D1+D2 session, in order:
```
user      message             []           I walked into my house.
assistant followup_questions  []           1. <question…>
user      message             []           I found my wife in bed…
assistant final_answer        [N, N, …]    <answer head…>
```

DB-level checks:

```bash
# Phase machine landed on done; round counter counted the Yenta round:
psql "$DBURL" -tAc "SELECT phase, followup_rounds, refined_question IS NOT NULL FROM advisor_sessions WHERE session_id = '$SID';"
# expected: done|1|t

# Memory row written (spec agent 9), embedded on the current model:
psql "$DBURL" -tAc "SELECT count(*) FROM advisor_memories WHERE session_id = '$SID' AND embedding IS NOT NULL AND embedding_model = 'mxbai-embed-large';"
# expected: 1

# Gap-free seq (0008 contract):
psql "$DBURL" -tAc "SELECT count(*) = max(seq) + 1 FROM advisor_messages WHERE session_id = '$SID';"
# expected: t
```

**Post-done reuse:** send one more message on `$SID` — it must start a FRESH gathering
cycle (a `session` event, then either `questions` or an answer; never an error about
the finished state).

**PASS E:** transcript sequence exact; phase `done` + rounds `1` + refined question
stored; memory row present; seq gap-free; post-done turn accepted.

---

## Phase F — Memory recall (observational)

New session, related topic:

```bash
curl -sN --max-time 240 http://127.0.0.1:8095/v1/advisor/chat \
  -H 'content-type: application/json' -H 'Authorization: Bearer dev-advisor-token' \
  -d '{"message":"Following up on my marriage situation - my wife wants to talk tonight. How should I approach the conversation? I want honesty without escalation."}' > /tmp/turn_mem.sse
```

Then check the service log (`/tmp/advisor-test.log`) shows the `recalling` phase ran,
and the answer does not contradict the earlier consultation. Retrieval is
cosine-gated (`ADVISOR_MEMORY_DISTANCE_THRESHOLD=0.6`), so injection is expected but
not hard-guaranteed for arbitrary phrasings — treat "no contradiction + recalling phase
ran" as PASS. (Deterministic memory assertions belong to the eval fixtures.)

**PASS F:** turn completes; `recalling` phase observed; no contradiction.

---

## Phase G — Restart durability

```bash
pkill -f "target/debug/hushai-advisor"; sleep 1
# relaunch exactly as C2, then:
curl -s -H 'Authorization: Bearer dev-advisor-token' http://127.0.0.1:8095/v1/advisor/sessions/$SID/messages | python3 -c "import json,sys; print(len(json.load(sys.stdin)), 'messages survive restart')"
# expected: same message count as Phase E
```

**PASS G:** transcript identical after restart (everything is DB-backed; no in-memory
state matters except the in-flight guard, which is intentionally per-process).

---

## Phase H — Eval harness (unit now; live gated)

Unit gate (no services needed):

```bash
cargo test -p hushai-eval
# expected: test result: ok. 23 passed (17 pre-existing + 6 advisor)
```

Live fixtures (`advisor_followup`, `advisor_direct`) run only against the TEST stack:

```bash
./local_dev/run_stack.sh --test-db          # brings the whole stack up on hushai_test
# then re-run Phase B's ingest against the test DB, then:
cargo run -p hushai-eval -- run --fixtures staging
# expected: advisor cases score (exit 0) — or INCONCLUSIVE (exit 2) with a
# "run ingest-book" hint if the test-DB corpus wasn't ingested
```

⚠️ **Do not run `--test-db` while a dev stack you care about is up**: run_stack's port
reclaim (8080/8090/8070/8095) tears down existing Hushai processes on those ports.
Schedule this on a free rig. Absent advisor/corpus must yield INCONCLUSIVE, never FAIL —
that itself is an assertion.

**PASS H:** 23/23 unit tests; live staging run exits 0 (or 2 with the ingest hint —
then ingest and re-run).

---

## Cleanup

```bash
pkill -f "target/debug/hushai-advisor"
# Test consultations in the dev DB are additive rows; remove if desired:
#   psql "$DBURL" -c "DELETE FROM advisor_sessions; DELETE FROM advisor_memories;"
# The ingested corpus (books/book_chapters/book_chunks) is durable state — keep it.
```

---

## Result matrix (reference run 2026-07-06)

| Phase | Check | Result |
|---|---|---|
| A | build / 11 unit tests / clippy 0 / workspace | ✅ |
| B | 50 chapters, 259 chunks, 0 running heads, 0 empty synopses, idempotent re-run | ✅ |
| C | fail-closed refusal · healthz 200 · redacted banner · 401s | ✅ |
| D1 | bare message → 1 question, no tokens | ✅ |
| D2 | detail → chapters (2 iterations, converged) + 383 tokens + done | ✅ |
| D3 | sufficient message → 0 questions, 278 tokens | ✅ |
| D4 | concurrent turn → 409; busy turn unaffected | ✅ |
| D5 | 400 empty / 404 unknown session | ✅ |
| E | transcript kinds+citations · phase done · rounds 1 · memory row · gap-free seq | ✅ |
| F | recalling phase ran, no contradiction | ✅ |
| G | transcript survives restart | ✅ |
| H | 23/23 eval unit tests · live staging | ✅ unit · ⏳ live (needs free test rig) |

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| every request 401 | `ADVISOR_TOKEN` mismatch between service env and curl header |
| `error` event: "no book has been ingested yet" | Phase B not run against THIS `DATABASE_URL` |
| service refuses to start | that's Phase C1 working — bind 127.0.0.1 or set the token |
| answers slow / Ollama OOM | `num_ctx=16384` KV cache ≈ 2–3 GB on 7B q4; other models loaded concurrently — reduce `ADVISOR_NUM_CTX` or free RAM |
| gate PROCEEDs on obviously thin input | judge quality; set `ADVISOR_JUDGE_MODEL=qwen2.5:14b` (pull it first) |
| routing feels off / wrong chapters | inspect synopses (Phase B spot-check); consider full `--llm-clean` ingest |
| turn hangs >3 min | check `/tmp/advisor-test.log`; Ollama queue shared with worker/rag — a busy worker delays advisor calls (by design, sequential) |
