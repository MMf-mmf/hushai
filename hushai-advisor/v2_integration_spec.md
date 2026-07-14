# hushai-advisor v2 — Client Integration Spec (web slash-command, voice-by-name, rig E2E)

The integration contract and acceptance run for bringing the Ahithophel advisor v1
(service verified in [`v1_spec.md`](v1_spec.md)) to its first two human-facing clients,
plus the E2E verification that proves it works on real-world questions. Unlike v1_spec
(which documented an already-passing run), **this spec is written BEFORE the
implementation** — Parts 1–3 are the build contract; the phases in Part 4 are executed
after the implementation lands and become the acceptance gate. Every `file:line`
reference was validated against the working tree on 2026-07-07 (branch
`feat/hushai-voice-assistant`).

**Scope.** Three tracks:

1. **Viewer web chat** — the advisor as a modern slash-command plugin: type `/` in the
   chat composer → picker → pick *Advisor* → the thread routes to the advisor with full
   phase/questions/chapters/stream rendering.
2. **Android voice assistant** — invoke the advisor by NAME: *"⟨wake⟩ advisor ⟨question⟩"*;
   the advisor's follow-up questions are spoken via TTS and answered by voice **without
   repeating the wake word** (multi-turn consult).
3. **Rig E2E** — eval fixtures with complicated multi-sentence questions spanning
   **multiple conversations** (cross-session memory), viewer headless-Chrome e2e, and a
   physical-phone voice-consult rig.

Locked decisions: voice trigger is **"advisor"** only (normalize the "adviser" ASR
spelling; no "Ahithophel" alias — Vosk's small model cannot recognize it). Mobile is
**voice-only** (no Android chat screen). All three E2E tiers are in scope.

**Conventions.** As v1_spec: run from the repo root, phases in order, a failed PASS
criterion stops the run. `DBURL` extraction as v1_spec. Advisor SSE contract (verified,
`src/chat.rs:130-153`): `session {session_id, phase}` → `phase {phase}` heartbeats
(gathering/refining/recalling/routing/drafting/reviewing/polishing/memorizing) → either
`questions {round, questions[]}` (turn ends) or `chapters {iteration, chapters:[{no,title}]}`
(≤3, converging) + `token {delta}`× → `done {message_id}`; `error {message}`; HTTP 409
busy session, 400 empty, 404 unknown session.

---

# Part 1 — Viewer: the advisor as a slash-command plugin

## 1.1 Architecture: a second pane, not a mode flag

The chat dock was built for N panes and N agents — `ui/index.html:184-192` holds
`#agentPicker` + `#chatPanes`; `ui/js/chat/agent-picker.js` (`renderAgentPicker`, N-ready,
unused) and `ui/js/chat/workspace.js:11-28` deliberately collapsed to one unified pane.
The advisor is a genuinely different protocol (own endpoints, `questions`/`chapters`/
`phase` events, no camera scope/thorough/playback), so:

**`AdvisorPane extends ChatPane`** (new `ui/js/chat/advisor-pane.js`), toggled by the
slash picker; NOT branches inside `ChatPane._send`. `workspace.js` constructs both panes
into `#chatPanes` and flips `display`; both conversations stay live when switching.
Session isolation is free: `SESSION_KEY(agentId)` (`chat-pane.js:19`) keyed on
`agent.id = "advisor"` yields `hushai.chat.session.advisor`, disjoint from `…auto`.

Prerequisite refactor — four overridable seams in `ChatPane` (no rag behavior change):

| Seam | ChatPane today | AdvisorPane override |
|---|---|---|
| `_fetchSessions()` | `getSessions()` → `/v1/rag/chat/sessions` (`chat-pane.js:287`) | `getAdvisorSessions()` → `/v1/advisor/sessions` mapped to `{session_id, title, updated_at, agent_id:"advisor"}` |
| `_fetchMessages(id)` | `getSessionMessages()` (`chat-pane.js:353`) | `getAdvisorSessionMessages()`; render by `kind`: `message`→user bubble, `followup_questions`→questions bubble, `final_answer`→assistant bubble + chapter chips |
| `_stream(payload, onEvent)` | `streamChat(...)` (`chat-pane.js:424`) | `streamAdvisorChat({sessionId, message}, onEvent)` — slim body, no filters/playback/exhaustive |
| `_handleEvent(ev, ctx)` | switch at `chat-pane.js:433-459` (session/sources/token/error/done) | advisor switch: session/phase/questions/chapters/token/error/done |

`AdvisorPane._build` additionally hides the camera-scope `<select>` and Thorough checkbox
(`chat-pane.js:86-107`); 🕘 history, New chat, composer, retry, copy/download/🔊 actions
are inherited unchanged (TTS read-aloud of long advice comes free).

## 1.2 Slash picker

New module `ui/js/chat/slash.js` — `attachSlashPicker(textarea, { items, onPick })`:

- **Trigger**: `/` typed as the **first character of an empty composer** (composer at
  `chat-pane.js:138-162`). No conflict with the omni palette — its global `/` hotkey
  explicitly ignores TEXTAREA focus (`ui/js/search/omni.js:416-419`).
- **Popover** anchored above the composer, positioned like the 🕘 sessions dropdown
  (`chat-pane.js:246-299`); rows reuse the omni row pattern (`omni.js:133-156`): icon +
  label + sub. Continued typing filters (`/adv` narrows).
- **Keyboard**: ↑/↓ highlight, Enter/Tab selects, Esc closes leaving the typed text;
  mouse click selects. Port `setHi`/`moveHi` from `omni.js:388-399`.
- **On pick**: composer cleared, workspace switches the visible pane, focuses its composer.
- **Registry**: generic-shaped static array in `workspace.js` — v1 content is two rows:
  `{id:"auto", icon:"🎥", name:"Assistant", sub:"your recordings"}` and
  `{id:"advisor", icon:"📖", name:"Advisor", sub:"book-grounded personal advice"}`.
  Future plugins append rows; do NOT revive the rag agents list (the unified-auto
  decision at `workspace.js:1-4` stands). In advisor mode the picker shows *Assistant*,
  so `/` is also the way back.
- **e2e hooks (required)**: each row carries `data-cmd="<id>"`; workspace exposes
  `window.chatDebug = { mode, sessionId, lastPhase }` (the `viewerDebug` precedent,
  `e2e/run.mjs:72`).

## 1.3 Active-mode indication and exit

- Composer chip `📖 Advisor ✕` as first child of `.chat-input` in the advisor pane only;
  reuse `.agent-tab.active` styling (`ui/styles.css:993-1004`; one new rule for ✕).
  Clicking ✕ (or `/`→Assistant) returns to the rag pane. **Exiting does not clear the
  advisor session** — `sessionStorage["hushai.chat.session.advisor"]` persists (subject
  to the inherited 60-min stale-restore guard, `chat-pane.js:21-24`).
- Dock header subtitle (`index.html:188`) flips to "personal advisor" while active.
- Rag pane placeholder gains one line: *Type "/" for plugins — /advisor gives
  book-grounded personal advice.* Advisor pane gets its own placeholder describing the
  gate ("expect a follow-up question or two, and give it a minute to think").

## 1.4 Rendering the advisor turn (30–90 s, never silent)

- **Phase pill**: on send, the assistant bubble is created with an
  `.ai-badge.is-processing` pill (`⟳ gathering…`); each `phase` event rewrites the label.
  Styles exist in full (`ui/styles.css:907-966`, spinner `:952-960`, reduced-motion
  `:962-966`). Pill is removed on first `token`, on `questions`, or on `error`.
- **`questions` event**: header *"I need a bit more detail (round {round} of 2):"* +
  `<ol>` of the question strings (one CSS rule: `.chat-text ol` margins). Composer
  placeholder flips to *"Answer the questions above…"* until the next send. No
  quick-reply buttons in v1 — one free-text message answers the whole round (matches the
  service contract).
- **`chapters` events**: chip row on the citation-chip pattern (`ui/js/chat/citation.js:8-36`,
  `.chat-cites`/`.chat-citation` `ui/styles.css:1082-1130`) — `[ch. N] Title`,
  non-interactive, tooltip = full title. Each `chapters` event **replaces** the row
  (sets converge); chips sit above the streaming text.
- **`token` deltas** append to the bubble text exactly as rag (`chat-pane.js:445-448`).
- **Errors/409/retry**: non-OK → error bubble + Retry (inherited `_addRetry`,
  `chat-pane.js:481-494`). Specialize 409: *"The advisor is still thinking about the
  previous turn — Retry when it finishes."*
- **Restore fidelity**: live questions render as `<ol>`; restored transcripts render the
  persisted numbered `content` string. Accepted difference — do not parse it back.

## 1.5 Plumbing (exact touch points)

| # | File | Change |
|---|---|---|
| 1 | `hushai-viewer/src/config.rs:190-191` | Add `advisor_base_url: opt("ADVISOR_BASE_URL", "http://127.0.0.1:8095")` + `advisor_token` (clone the rag pair) |
| 2 | `hushai-viewer/src/proxy.rs:141-172` | `fn is_advisor_path(path)` (`== "/v1/advisor" || starts_with("/v1/advisor/")`); three-way upstream select in `forward_inner`; counter label `upstream="advisor"` (`:164-167`); bearer injection at `:187-191` unchanged. **Advisor stays OUT of the gateway audit** — consultations are the most private payloads in the system; mirror the rag-chat no-audit posture and say so in a comment |
| 3 | `hushai-viewer/src/routes.rs:68` | No change — `/v1/{*rest}` already covers `/v1/advisor/*` behind the session gate; SSE flows unbuffered (`proxy.rs:215-216`) |
| 4 | `hushai-viewer/ui/js/api.js` | Extract the fetch-SSE pump from `streamChat` (`:515-565`) into `streamSse(url, body, onEvent)` (reuse `parseFrame`/`nextFrameBreak` `:567-591`); add `streamAdvisorChat`, `getAdvisorSessions`, `getAdvisorSessionMessages` |
| 5 | `hushai-viewer/ui/js/chat/chat-pane.js` | The four seams (§1.1); no rag behavior change |
| 6 | `hushai-viewer/ui/js/chat/advisor-pane.js` (new) | §1.1/§1.4 |
| 7 | `hushai-viewer/ui/js/chat/slash.js` (new) | §1.2 |
| 8 | `hushai-viewer/ui/js/chat/workspace.js` | Both panes, mode switch, picker registry, header subtitle, `chatDebug` |
| 9 | `hushai-viewer/ui/styles.css` | ~4 small rules (chip ✕, `.chat-text ol`, chapter-chip cursor, slash popover) |
| 10 | `local_dev/run_stack.sh` | TLS block (where `RAG_BASE_URL=https://…` is exported): also export `ADVISOR_BASE_URL="https://127.0.0.1:8095"` — the advisor serves TLS off the shared cert fallback, so a `--tls` run would otherwise proxy `http://` into an https listener. `ADVISOR_TOKEN` is already minted+exported before the viewer launch — no change |
| 11 | `hushai-viewer/e2e/run.mjs` | New checks before the native-dialog guard at `:282` (Phase C) |

---

# Part 2 — Android: the advisor by voice, multi-turn

## 2.1 Utterance grammar and routing

Routing applies to the tail AFTER the wake word (wake detection unchanged,
`handleFinal`, `assistant/VoiceAssistant.kt:181-207`):

- `"⟨wake⟩ advisor ⟨question…⟩"` — tail token[0] ∈ {`advisor`, `adviser`} (ASR
  orthography normalization, not an alias) and remainder ≥ `MIN_QUESTION_WORDS` → advisor
  route with the remainder.
- `"⟨wake⟩ advisor"` (bare) → TTS prompt *"What would you like advice on?"* →
  `AWAIT_QUESTION` with an `advisorPending` flag; the next verified utterance routes to
  the advisor (existing 8 s question window).
- Anything else → existing RAG path, byte-for-byte unchanged (`answer()`, `:217-263`).

Extract into a pure, unit-testable object — new `assistant/AssistantRouting.kt`:
`fun route(tailTokens: List<String>): Route` where
`Route = Rag(question) | AdvisorBare | Advisor(question)`. `advisor` mid-question
("is my advisor lying") must route to `Rag` — the keyword only counts as token 0.

## 2.2 New phase `AWAIT_FOLLOWUP`

Add to `AssistantPhase` (`util/AssistantBus.kt:6-8`). The `when` in
`ui/CaptureScreen.kt:609-614` is exhaustive — the compiler finds every ripple point:

- `CaptureScreen` label: *"listening for your answer"*.
- `VoiceAssistant.onPcm` allowlist (`:130`): accept PCM in `AWAIT_FOLLOWUP`. Reusing the
  phase-gated capture is a hard requirement — no PCM during `SPEAKING`, so the phone
  never transcribes its own TTS.
- Worker-loop deadline (mirror `AWAIT_QUESTION`, `:144-147`):
  `FOLLOWUP_TIMEOUT_NANOS = 30 s` (the user composes an answer to up to 3 questions —
  8 s is too tight). On timeout → resume listening; the consult survives in
  `AdvisorSession`, so a wake-prefixed `"advisor …"` continues the same server session.
- `handleFinal` gains an `AWAIT_FOLLOWUP` branch: **per-turn owner verify**
  (`verifyOwner`, `:285-302`) — a rejected speaker is ignored and the phase STAYS
  `AWAIT_FOLLOWUP` until the deadline (a stranger must not consume the owner's window).
- Post-TTS transition: implement as a `@Volatile pendingResumeTarget` phase the worker
  reads (replacing the boolean `pendingResume`), so questions-TTS resumes into
  `AWAIT_FOLLOWUP` while answer-TTS resumes into `LISTENING`.

## 2.3 The consult loop

```
LISTENING --"⟨wake⟩ advisor ⟨q⟩"--> [verify] --> THINKING (advisor chat)
   THINKING --Questions(round,qs)--> SPEAKING (one TTS call: intro + numbered questions)
        --speech done--> AWAIT_FOLLOWUP (30 s, wake-word-free)
   AWAIT_FOLLOWUP --owner utterance--> THINKING (same session_id)   [≤2 rounds, server-capped]
   THINKING --Answer(text, chapters)--> SPEAKING (advice) --> LISTENING
```

- Questions TTS is ONE synthesis call: *"I need a bit more detail. Question one: … .
  Question two: … ."* — numbered words, period-joined for natural pauses; fits the 60 s
  speak watchdog.
- A completed consult returns to `LISTENING` — the wake word is required to start the
  next turn; only follow-up ROUNDS are wake-free.
- Reset/abort: `VoiceSession.isResetCommand` ("new chat", `VoiceSession.kt:81-89`)
  resets the advisor session; in `AWAIT_FOLLOWUP`, "cancel"/"never mind" aborts the
  round → *"Okay."* → `LISTENING` (server session kept).

## 2.4 `AdvisorClient` (new `net/AdvisorClient.kt`)

Blocking OkHttp + full-body SSE accumulation, structural sibling of `RagChatClient`
(`net/RagChatClient.kt:22-143`; voice has no streaming UI, so blocking is correct):

```kotlin
sealed interface Result {
  data class Questions(val round: Int, val questions: List<String>, val sessionId: String) : Result
  data class Answer(val text: String, val sessionId: String, val chapters: List<Chapter>) : Result
  data object SessionNotFound : Result   // 404 unknown session
  data object Busy : Result              // 409 in-flight guard
  data class Error(val reason: String) : Result
}
fun chat(message: String, sessionId: String?): Result   // body: {"session_id"?, "message"} only
```

Parse rules: `session` → capture id; `questions` → terminal for the turn (still read to
`done`); `chapters` → **last event's set wins** (converged); `token` → accumulate;
`error` → `Error`; questions-seen outranks empty-text when classifying the outcome.
New `Http.advisor` client (`net/Http.kt`, pattern of `Http.rag` `:26-31`): read timeout
60 s (phase heartbeats double as keep-alives), **call timeout 300 s** (cold-model turns
exceed rag's 190 s cap).

## 2.5 `AdvisorSession`, config, wiring

- Reuse `VoiceSession` with a **parameterized idle window** (constructor param, default
  the current 15 min; advisor instance uses **30 min** — consults are weightier, and
  cross-session pgvector memory covers anything past the window). Second instance in
  `CaptureService.buildAssistant()` (`capture/CaptureService.kt:206-233`) with new
  DataStore keys `advisor_session_id`/`advisor_session_at_millis`
  (pattern `config/Settings.kt:148-149`, blocking accessors `:110-130`).
- `SessionNotFound` → silent reset + one sessionless retry (mirror the rag handling,
  `VoiceAssistant.kt:238-252`).
- Config 1:1 with the rag pair: Settings keys `advisor_url` (default
  `http://localhost:8095`) / `advisor_token`; intent extras
  `EXTRA_ADVISOR_URL = "advisor_url"` / `EXTRA_ADVISOR_TOKEN = "advisor_token"`
  (`CaptureService.kt:864-870` companion, `MainActivity.handleIntent` `:271-324`) —
  the `rag_token` lesson: without the extra, a stale DataStore token 401s every consult.
- `local_dev/run_hushai_app.sh`: add `adb reverse tcp:8095 tcp:8095`, emulator default
  `http://10.0.2.2:8095`, pass `--es advisor_url … --es advisor_token …`, accept
  `ADVISOR_TOKEN` env like `RAG_TOKEN`.

## 2.6 Spoken fallbacks

| Outcome | Spoken | Then |
|---|---|---|
| `Error` (down/network/SSE error) | "Sorry — the advisor isn't available right now." | LISTENING |
| `Busy` (409) | "The advisor is still thinking about your last question — give it a moment." | LISTENING |
| `SessionNotFound` | (silent) reset, retry once fresh | per retry outcome |
| Empty answer | treat as `Error` | LISTENING |

The advisor branch forks BEFORE `answer()`; regular RAG voice questions share nothing
with it but `speakAnswer`/`verifyOwner`.

## 2.7 Logcat marker contract (tag `HUSHAI_TX`)

The observability contract for Phase D/G/H — exact grep-stable strings, one line each
(the rig asserts prefix + order; keep them byte-stable once shipped):

| Moment | Marker |
|---|---|
| Advisor route chosen | `advisor route (session=<id\|new>)` |
| Bare-advisor prompt | `advisor route: awaiting question` |
| Questions received | `advisor questions round=<r> count=<n> (session=<id>)` |
| Questions TTS finished, window open | `advisor questions spoken — awaiting answer` |
| Follow-up captured | `advisor followup captured (round=<r>, words=<n>)` |
| Final answer delivered | `advisor answer ok (session=<id> chapters=[<no>,<no>,…])` |
| Error | `advisor error: <reason>` |
| Busy | `advisor busy (409)` |
| Follow-up window timeout | `advisor followup timeout — resuming listen` |

`advisor questions spoken` is the rig's sequencing anchor — it must fire only after TTS
playback completes (the moment `AWAIT_FOLLOWUP` opens), never at synthesis start.

---

# Part 3 — E2E verification capability

## 3.1 Harness additions (hushai-eval)

**Multi-session support — `AdvisorTurn { new_session: bool }`** (serde default `false`),
chosen over ChatQ-style session labels (`fixtures.rs:270-281`): advisor turns are
inherently sequential (the runner threads one `session_id`, `lib.rs:361-374`; the
service 409s concurrency), and a flat ordered turn list keeps the position-indexed
`advisor.t{i}.*` metric keys byte-stable under the "never reorder, only append" contract
(`fixtures.rs:377-380`). Runner change in `run_advisor_case`: before
`query_advisor::ask`, `if t.new_session { session_id = None; }`; re-learn the id
whenever it is `None`. `reset_advisor` stays once-per-case (`reset.rs:44-53`) — memories
MUST survive across sessions within a case (that is the feature under test) and must
not survive across cases.

Two free structural metrics in `score_advisor` (session ids already ride
`AdvisorTurnResult`, `query_advisor.rs:29`):
`advisor.t{i}.session_isolated` (new_session turns: id non-empty AND ≠ previous) and
`advisor.t{i}.session_continuous` (continued turns: id == previous). New keys classify
as `New` against existing baselines — non-gating retrofit (`baseline.rs:56-58`).

**Richer assertions on `AdvisorTurn`** (mirror ChatQ semantics):

| Field | Metric | Source pattern |
|---|---|---|
| `must_contain_any: Vec<String>` | `advisor.t{i}.contains_any` (at-least-one, normalized) | `score.rs:736-756` |
| `must_not_contain: Vec<String>` | `advisor.t{i}.clean` (hallucination negatives) | `score.rs:757-767` |
| `expect_memory_rows_min: Option<i64>` | `advisor.t{i}.memory_rows` — `SELECT count(*) FROM advisor_memories` after the turn, probed in `run_advisor_case` via `ctx.pool` (the `book_chunk_count` pattern, `query_advisor.rs:60-66`), stashed on `AdvisorTurnResult` so `score_advisor` stays pure | |
| `expect_memory_recall_min: Option<i64>` | `advisor.t{i}.memory_recalled` — see the `memory` event below | |

**Service addition (small, coordinated):** after `retrieve_memories`
(`hushai-advisor/src/pipeline.rs:158-167`) the advisor emits a new SSE event
`memory {recalled: n, nearest_distance: d}`. The harness parses it in `handle_block`
(`query_advisor.rs:124-181`) into `memories_recalled: Option<i64>`; `None` (old binary)
degrades the metric to Info — the `routed` degradation pattern (`score.rs:790-796`).
`nearest_distance` is calibration telemetry against the 0.6 cutoff. Viewer/Android
clients ignore unknown events by design — no client change.

**The memory-recall proof is three independent, individually-diagnosable layers:**
the `recalling` phase event proves nothing (it fires unconditionally when memory is
enabled, `pipeline.rs:158-159`). Instead:

1. **Write proof** (DB, hard gate): `expect_memory_rows_min: 1` on session 1's final turn.
2. **Retrieval proof** (SSE, hard gate once the event ships): `expect_memory_recall_min: 1`.
3. **Content proof** (planted tokens, hard gate): session 1 plants distinctive proper
   nouns; session 2 references the situation obliquely; `must_contain_any` lists the
   tokens. The memory context injects the stored question verbatim (`memory.rs:89-103`),
   so at temp 0 / seed 7 (`local_dev/eval.env`) the token surfaces reliably. Assert
   tokens, never advice phrasing.

A failure self-diagnoses: rows=0 → memorizer broke; recalled=0 → retrieval/threshold;
token missing with recalled≥1 → prompt injection/drafting.

**Manifest KNOBS** (`manifest.rs:19-63`): hand-list the 18 output-determining advisor
vars — `ADVISOR_LLM_MODEL, ADVISOR_JUDGE_MODEL, ADVISOR_LLM_TEMPERATURE,
ADVISOR_LLM_SEED, ADVISOR_NUM_CTX, ADVISOR_MAX_FOLLOWUP_ROUNDS,
ADVISOR_MAX_QUESTIONS_PER_ROUND, ADVISOR_MAX_REFINE_ITERS,
ADVISOR_MAX_CHAPTERS_PER_ROUTE, ADVISOR_MAX_TOTAL_CHAPTERS, ADVISOR_HISTORY_TURNS,
ADVISOR_CHAPTER_MAX_CHARS, ADVISOR_CONTEXT_MAX_TOTAL_CHARS,
ADVISOR_ROUTE_SEMANTIC_TOP_K, ADVISOR_MEMORY_TOP_K, ADVISOR_MEMORY_DISTANCE_THRESHOLD,
ADVISOR_MEMORY_ENABLED, ADVISOR_MAX_MESSAGE_CHARS`. Do NOT prefix-fold `ADVISOR_` — it
would sweep `ADVISOR_TOKEN` (secret) and `ADVISOR_BIND_ADDR`/`ADVISOR_TLS_*`
(machine-specific), the exact trap the `RAG_` comment warns about (`manifest.rs:41-43`).
**Corpus lineage**: fold a corpus fingerprint into `EnvManifest::collect`
(`manifest.rs:116-155`) — `SELECT count(*), md5(string_agg(synopsis, '' ORDER BY chapter_no))
FROM book_chapters` via `ctx.pool`, degrading to `"absent"` when the tables don't exist.
Until it ships: **any re-ingest requires `--update-baseline` on all advisor cases.**

**New unit tests** (suite is 23 today; PASS states the new total explicitly):

1. `fixtures.rs::advisor_staging_fixtures_parse` extended to all new fixture dirs;
   asserts `new_session` on the intended turns and the new fields parse.
2. `score.rs`: `contains_any` both polarities (miss quotes the answer head).
3. `score.rs`: `clean` both polarities.
4. `score.rs`: `session_isolated`/`session_continuous` from scripted ids
   (fresh / continued / silently-dropped).
5. `score.rs`: `memory_recalled` gates when `Some`, Info-degrades when `None`;
   `memory_rows` floors at the min.
6. `query_advisor`: `handle_block` parses the `memory` event; unknown events still
   ignored (old-binary regression guard).
7. Extract the per-turn session-threading decision from `lib.rs:361-374` into a pure
   helper; test `[t0, t1{new_session}, t2]` → sends `[None, None, Some(id_from_t1)]`.
8. Metric-key stability: appending a turn never changes keys `advisor.t0..t{n-1}`
   (executable guard for the never-reorder contract).

## 3.2 Real-world fixture bank (6 new, all `fixtures/staging/`)

All: `tier:"full"`, `device_id:"eval-advisor"`, `modalities:["advisor"]`,
`media_file:""` (the existing advisor meta pattern). Existing `advisor_followup` +
`advisor_direct` remain untouched.

**Calibration protocol (mandatory):** `expect_chapters_any` sets below are drafts. On
the first live run, freeze the observed final grounding (widen, never narrow). Run twice
back-to-back; identical verdicts required before freezing any content assertion.
Chapter-topic anchors (from the ingested corpus): social proof 1/2/4, anchoring/decoy
7/21, reciprocity 12/13/19, commitment/consistency 14/16-18, expertise 22-24,
admit-faults 25-28/33, similarity/names 29/30, loss framing 34, "because" 35,
sad-negotiation 44, voicemail 50.

- **F1 `advisor_gate_negotiation`** — gate-exercising (thin → questions → detail → final):
  - t0 `"I have a big negotiation coming up next month and I'm nervous about it."` →
    `expect_questions:true, expect_final_answer:false`
  - t1 supplier-renewal detail (largest packaging supplier, volume doubled, 9 % opener,
    six-year relationship with the account manager, CFO wants a switch threat, four-month
    switch time) → no assertions (a second gate round is legitimate; cap is 2)
  - t2 goals (under 4 %, sign this quarter, keep priority production slots; two-year term
    + reference as sweeteners) → `expect_questions:false, expect_final_answer:true,
    expect_chapters_any:[7,21,31,44]` (calibrate), `must_not_contain:["as an AI","I cannot help"]`
- **F2 `advisor_memory_cross_session`** — the flagship multi-conversation memory fixture:
  - Session 1, t0 (direct, fully-specified): 50-50 partner **Menashe** at **Silverstein
    Lighting** blocking the showroom renovation three meetings running, bank matching
    grant expiring end of month, family dinners tense — *"how should I approach him?"* →
    `expect_questions:false, expect_final_answer:true, expect_memory_rows_min:1`
  - t1 `new_session:true`, deliberately omitting name and company: *"I need to follow up
    on the business disagreement with my brother-in-law that I consulted you about…
    Remind me who and what was involved and what the core of your recommended approach
    was…"* → `expect_final_answer:true, expect_memory_recall_min:1,
    must_contain_any:["Menashe","Silverstein"]`
  - Rationale: planted tokens are the only deterministic proof session 2 used session 1's
    MEMORY (the `session_isolated` metric proves the sessions are distinct; the tokens
    are far outside the corpus vocabulary, so a hit cannot be hallucinated from the book).
- **F3 `advisor_memory_selectivity`** — three sessions; retrieval must pick the RIGHT memory:
  - S1 t0 (direct, token set A): renew the lease with landlord **Mrs. Okonkwo** for
    **the Maple Street bakery** → `expect_final_answer:true, expect_memory_rows_min:1`
  - S2 t1 `new_session:true` (token set B, unrelated domain): son **Tuvia** and violin
    practice → `expect_final_answer:true, expect_memory_rows_min:2`
  - S3 t2 `new_session:true`: *"About the lease situation I consulted you on — the
    landlord countered with a shorter term at higher rent…"* →
    `expect_final_answer:true, expect_memory_recall_min:1,
    must_contain_any:["Okonkwo","Maple Street","bakery"], must_not_contain:["Tuvia","violin"]`
  - If the leak negative proves flaky at calibration, shorten only `must_not_contain`;
    keep the positive.
- **F4 `advisor_direct_hardnews`** — direct-sufficient, hard conversation: twelve-person
  manufacturer shipped an 8 %-failure batch that downed the biggest customer's line for
  two days; own quality process at fault; VP meeting Friday; wants to keep the account
  and compensate → `expect_questions:false, expect_final_answer:true,
  expect_chapters_any:[26,27,28,33]` (calibrate),
  `must_contain_any:["apolog","admit","responsib"]` (normalized stems — structural
  anchors any competent grounding of the admit-mistakes cluster produces).
- **F5 `advisor_followup_two_rounds`** — the gate cap end-to-end:
  - t0 `"I need advice about a person at work."` → `expect_questions:true, expect_final_answer:false`
  - t1 `"It's my manager. Things have been difficult lately."` (still thin) →
    `expect_questions:true, expect_final_answer:false` (calibrate — if the judge PROCEEDs
    at temp 0, drop the assertion rather than fatten t1)
  - t2 full passed-over-for-promotion detail (goal + constraints + deadline) →
    `expect_questions:false, expect_final_answer:true` — at
    `ADVISOR_MAX_FOLLOWUP_ROUNDS=2` this is cap-forced, judge-independent.
- **F6 `advisor_offtopic_recovery`** — robustness:
  - t0 `"What is the capital of France? Don't ask me any questions, just answer."` → no
    assertions beyond the automatic clean-stream gate (`advisor.t0.errored`) — off-topic
    behavior is observed then frozen; the day-one invariant is "no error event, no crash".
  - t1 real situation (co-founder investment disagreement, ten-day deadline, wants a
    meeting) → `expect_final_answer:true`
  - Note: empty-input cannot be a fixture (400 → transport error → INCONCLUSIVE); it
    stays in v1_spec Phase D5's curl checks.

## 3.3 Viewer headless-Chrome e2e

**Live advisor, no stub** — the sweep's philosophy is real-stack with honest SKIP
degradation (`e2e/run.mjs:7-9`); a stub would freeze today's event contract and pass
while the real one drifts. Latency is managed by choosing cheap turns: a **gate turn**
(thin opener → `questions`) is one judge call (~5–20 s), vs 30–90 s for a full answer.

New checks (before the dialog guard at `run.mjs:282`; the whole block SKIPs when the
advisor is absent):

1. `advisor slash command appears in composer picker` — focus composer, type `/`, assert
   `[data-cmd="advisor"]` row. Also pins the omni collision: composer owns `/` only
   while focused.
2. `selecting /advisor switches the pane` — click; assert the chip and
   `window.chatDebug.mode === "advisor"`.
3. `thin message → phase pill + questions render` — send `"I walked into my house."`
   (canonical D1 probe, guaranteed questions turn); `until()` a `.ai-badge` showing
   `gathering`, then a questions `<ol>` (1–3 items) and zero streamed text; per-call
   `timeout: 120_000` (default `until` is 8 s).
4. `session separation` — capture `chatDebug.sessionId`; New chat clears it; advisor
   session lives under `hushai.chat.session.advisor`, never the `auto` key; a second
   thin message mints a different id.
5. `advisor token never reaches the browser` — token string absent from page HTML/JS
   globals; the page's fetch targets a viewer-relative path, never `:8095`.
6. Env-gated `E2E_ADVISOR_FULL=1` — answer with the D2 detail message; `until` token
   text grows and `[ch. N]` chips render; `timeout: 300_000`.

Cost: +2–3 min default sweep; +~2 min with the full turn.

## 3.4 Physical-phone voice consult rig

**Stack wiring** — the phys tier is a second stack (DB `hushai_test_phys`, host ports
8082/8092; phone dials its own localhost via `adb reverse`):

- `local_dev/phys.env`: `ADVISOR_BIND_ADDR=0.0.0.0:8097`, `PHYS_ADVISOR_PORT=8097`
  (the +2 convention: 8080→8082, 8090→8092, 8095→8097). Launch the advisor with the
  documented phys-profile recipe (`source eval.env; source phys.env`) so it runs against
  the phys DB.
- Rig adds `adb reverse tcp:8095 tcp:8097` — the phone keeps dialing `localhost:8095`.
- Intent extras `--es advisor_url http://localhost:8095 --es advisor_token
  dev-advisor-token` (the `rag_token` lesson).
- Precondition: `ingest-book` run once against `hushai_test_phys` (a THIRD corpus copy:
  dev, `hushai_test`, `hushai_test_phys`).

**New rig `local_dev/voice_advisor_loop.py`** (modeled on `voice_assistant_loop.py`):

- Audio via the voice-matrix `render()` recipe (macOS `say`, owner voice, 16 k mono,
  loudnorm): wake clip, then a separate `"advisor, ⟨opener⟩"` clip (the two-clip
  lesson), plus two scripted answer clips — **short, keyword-dense** (Vosk mishears long
  TTS sentences).
- Sequencing (afplay + `wait_for` on logcat markers, §2.7): play wake → beat → opener →
  `wait_for("advisor route", 30 s)` → `wait_for("advisor questions spoken", 240 s)` →
  sleep 1.5 s margin → answer 1 → `wait_for("advisor followup captured", 30 s)` →
  branch on a second `questions spoken` (answer 2) or `advisor answer ok` →
  final `wait_for` cap 420 s. Missed wake/capture → replay once → excluded-INCONCLUSIVE.
  Hard cap ~12 min per consult. Serialize on `phonelock.phone_lock()`.
- **Scoring (tolerant — what is deterministic over the air gap):** marker presence +
  order (route < questions spoken < followup captured < answer ok), zero
  `advisor error`; ONE session id across the consult's markers; DB shape on the phys DB
  (the v1_spec Phase E queries verbatim: phase `done`, `followup_rounds ≥ 1`, message
  kinds `user/followup_questions/user/final_answer` in order, gap-free `seq`, one
  `advisor_memories` row). ASR keyword recall (planted keywords from the spoken answers
  appearing in stored user rows, any-of, normalized) is **report-only on run 1**, frozen
  as a bar thereafter (the voice-matrix precedent). The phone's spoken TTS content is
  NEVER asserted — the rig has no capture of the phone speaker.
- **Smoke first**: single-turn direct consult — wake → `"advisor, ⟨fully-specified
  question⟩"` → assert route + `advisor answer ok`, NO `questions spoken`, DB
  `followup_rounds=0`, memory row. ~5–10 min.
- **Stretch (optional)**: memory across voice consults — consult 1, spoken reset, consult
  2 referencing the planted token obliquely; assert recall via the DB `final_answer`
  content (not audio).

---

# Part 4 — Acceptance phases (execute after implementation)

Phases run in order; C/F/G/H are serialized (shared Ollama). Total wall-clock
≈ 1 h 15 m – 1 h 45 m warm.

## Phase 0 — Preconditions delta

v1_spec Phase 0 green, PLUS: corpus ingested on `hushai_test` AND `hushai_test_phys`
(`SELECT count(*) FROM book_chunks` > 0 on each); real Google Chrome present (`CHROME`
env for e2e); phone USB-authorized + owner enrolled; ports 8095/8097 free on the
respective stacks.

**PASS 0:** all checks green.

## Phase A — Build + unit gates

```bash
SQLX_OFFLINE=true cargo build -p hushai-viewer && SQLX_OFFLINE=true cargo clippy -p hushai-viewer
SQLX_OFFLINE=true cargo test -p hushai-eval
( cd hushai-android && ./gradlew :app:testDebugUnitTest -q )
```

**PASS A:**
- viewer builds, 0 clippy warnings in `hushai-viewer/src`
- eval: all §3.1 tests present; total ≥ 31 (23 today + the 8 listed), 0 failed
- Android suite green including: `AdvisorClientTest` (MockWebServer, pattern of
  `RagChatClientTest.kt`): questions outcome; answer outcome with last-chapters-set-wins;
  409→Busy; 404→SessionNotFound; request shape (slim body, bearer, path).
  `AssistantRoutingTest`: advisor+question → Advisor; bare → AdvisorBare; `adviser` →
  advisor; no keyword → Rag; mid-question "advisor" → Rag.
  `VoiceSessionTest`: parameterized idle window (29 min same id, 31 min null at 30-min window).

## Phase B — Viewer proxy plumbing (curl-level, before UI)

Stack via `./local_dev/run_stack.sh` (note the printed advisor token); auth-disabled dev
posture (e2e assumption):

```bash
curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:8070/v1/advisor/sessions        # 200 (bearer injected server-side)
curl -sN --max-time 240 http://127.0.0.1:8070/v1/advisor/chat \
  -H 'content-type: application/json' -d '{"message":"I walked into my house."}' \
  | grep -c "^event: questions"                                                            # 1
curl -s http://127.0.0.1:8070/metrics | grep 'hushai_viewer_proxy_total.*advisor'          # counter present
grep -rn ADVISOR_TOKEN hushai-viewer/ui/                                                   # empty
```

**PASS B:** 200 with no browser-side token; the SSE turn streams through the proxy live
(the `questions` frame observed as it arrives, not at close); `upstream="advisor"`
counter incremented; no secret in the UI tree. `--tls` variant: same checks with
`ADVISOR_BASE_URL=https://…` exported (the run_stack TLS block).

## Phase C — Viewer UX (manual script, then e2e)

Manual (each observation is a criterion):

1. Focus composer, type `/` → picker with *Assistant* + *Advisor* rows; ↓ + Enter selects.
2. Chip `📖 Advisor ✕` appears; header subtitle "personal advisor"; advisor placeholder shown.
3. Send `I walked into my house.` → phase pill cycles (≥ `⟳ gathering…`) → numbered
   questions bubble ("round 1 of 2", 1–3 items); no token text; placeholder flips to
   "Answer the questions above…".
4. Answer with the v1_spec D2 detail message → ≥3 distinct phase labels → chapter chips
   (1–6, replaced not appended on a second `chapters` event) → advice streams
   token-by-token → pill gone on first token; copy/download/🔊 actions on completion.
5. 🕘 lists the consult; page reload restores the flow (questions bubble + chips + answer).
6. Second tab, same session, send mid-turn → "still thinking" error bubble + Retry;
   Retry succeeds after the busy turn completes.
7. ✕ returns to Assistant with its conversation untouched; `/` → Advisor resumes the
   consult (sessionStorage key `hushai.chat.session.advisor`).
8. Regression: a recordings question in Assistant mode streams with citations as before.

e2e: `node hushai-viewer/e2e/run.mjs` with the §3.3 checks.

**PASS C:** all 8 manual observations; e2e ends `0 failed` with the advisor checks PASS —
SKIP means the phase was NOT run (advisor absent), not a pass.

## Phase D — Android on-device voice consult (spoken, physical phone)

Launch via `./local_dev/run_hushai_app.sh` (now reversing 8095 + advisor extras);
`adb logcat -s HUSHAI_TX:I` in a second terminal; wake word "computer"; owner enrolled.

1. *"computer advisor should I confront my business partner about missing money"* →
   markers in order: `advisor route (session=new)` → (30–90 s) → either
   `advisor questions round=1 count=N` AND the questions spoken via TTS, or
   `advisor answer ok (…)` and advice spoken.
2. If questions: WITHOUT the wake word, speak a detailed answer within 30 s →
   `advisor followup captured (round=1, words=N)` → eventually
   `advisor answer ok (session=<id> chapters=[…])`, advice spoken; `speaker cosine=…`
   logged for the follow-up utterance (per-turn verify).
3. UI shows "listening for your answer" during the window.
4. Within 30 min: *"computer advisor"* (bare) → prompt → question →
   `advisor route (session=<same id>)` (continuation).
5. *"computer advisor new chat"* → "Okay, starting fresh." → next consult `session=new`.
6. A stranger says *"computer advisor …"* → `speaker rejected`, NO advisor markers.
7. Let the follow-up window lapse → `advisor followup timeout — resuming listen`; then
   *"computer who did I see today"* → normal `rag chat ok (…)` (RAG path untouched).
8. `pkill -f hushai-advisor` → consult → "Sorry — the advisor isn't available right
   now." + `advisor error: …`; restart advisor → next consult works.
9. Throughout: capture uploads keep flowing (`status=200` lines continue) — the standing
   voice-assistant regression bar.

**PASS D:** all nine observed; no advisor marker ever fires for a non-advisor question;
`GET /v1/advisor/sessions/{id}/messages` shows the same flow the phone spoke
(`followup_questions` then `final_answer` with chapters).

## Phase E — Phone restart durability

Force-stop the app after an answered consult (within the 30-min window), relaunch →
*"computer advisor ⟨follow-up⟩"* continues the SAME session id (DataStore-persisted,
like `voice_session_*`).

**PASS E:** logcat shows `session=<same id>` after restart.

## Phase F — Eval live fixtures (test stack)

⚠️ v1_spec Phase H's warning applies: `--test-db` reclaims 8080/8090/8070/8095 —
schedule on a free rig.

```bash
./local_dev/run_stack.sh --test-db
# corpus on hushai_test (once): the v1_spec Phase B ingest against the test DB
cargo run -p hushai-eval -- run --fixtures staging          # run 1: calibration
# freeze chapters_any (widen-never-narrow) + verify content assertions,
# then run 2: identical verdicts required
cargo run -p hushai-eval -- run --fixtures staging --update-baseline
cargo run -p hushai-eval -- run --fixtures staging          # the gating run
```

**PASS F:** gating run exits 0; every `advisor.*` metric floor-ok; two back-to-back runs
produced identical verdicts; exit 2 acceptable ONLY with the "run ingest-book" hint
(then ingest + rerun).

## Phase G — Phone smoke: single-turn direct voice consult

`local_dev/voice_advisor_loop.py --smoke` (§3.4): wake → direct fully-specified spoken
question → advisor answers with no gate round.

**PASS G:** `advisor route` + `advisor answer ok` markers, NO `questions spoken`; phys-DB
`followup_rounds=0`, memory row written; zero `advisor error`.

## Phase H — Phone multi-turn consult

`local_dev/voice_advisor_loop.py` full script (§3.4): thin spoken opener → questions
spoken → spoken answer(s) → final advice.

**PASS H:** marker order route < questions spoken < followup captured < answer ok; one
session id throughout; phys-DB shape (phase done, rounds ≥1, kind sequence, gap-free
seq, memory row); ≥1 of 2 scripted answers captured; ASR keyword recall report-only on
run 1, frozen thereafter.

**H2 (optional stretch):** memory across voice consults — consult 1, spoken reset,
consult 2 oblique reference; planted token present in the stored `final_answer` (DB, not
audio).

---

## Result matrix (fill on execution)

| Phase | Check | Result |
|---|---|---|
| 0 | preconditions delta (2 test corpora, Chrome, phone, ports) | ◑ (viewer track only: `hushai_test` corpus + Chrome present) |
| A | viewer clippy 0 · eval ≥31 tests · Android suite + 3 new test classes | ◑ (viewer builds + clippy-clean; eval/Android tracks = later PRs) |
| B | proxy 200/SSE-live/counter/no-secret (+ --tls variant) | ✅ (Part 1 landed: `/v1/advisor/sessions` → 200 via proxy, live `questions` frame streamed through, `upstream="advisor"` counter, no `ADVISOR_TOKEN` in the UI tree) |
| C | 8-step manual UX + e2e advisor checks PASS | ✅ (viewer track: `AdvisorPane` + slash picker + phase pill + questions render; e2e advisor checks green — full-answer step behind `E2E_ADVISOR_FULL`) |
| D | 9-step spoken script, marker contract honored | ⬜ (Android voice PR) |
| E | advisor session survives app restart | ⬜ (Android voice PR) |
| F | staging gating run exit 0, 2× identical verdicts | ⬜ (fixture-bank PR) |
| G | voice smoke: direct consult, no gate round | ⬜ (phone-rig PR) |
| H | voice multi-turn: marker order + session continuity + DB shape | ⬜ (phone-rig PR) |

**Part 1 (viewer web chat) LANDED** alongside Gotham G4 / Phase G — the slash picker + `ChatPane`
seams + `AdvisorPane` are the shared infra both consume (`Gotham.md` §2.7, PR-slicing #9). Tracks 2
(Android voice) and 3 (eval fixtures + phone rig) remain.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| every advisor request 401 on phone | missing `advisor_token` extra — stale DataStore token (the `rag_token` lesson) |
| "no book has been ingested yet" | wrong DB of the THREE (dev / `hushai_test` / `hushai_test_phys`) — ingest against the one the failing stack uses |
| 409 on double-send | busy-session guard working as designed, not a bug — Retry after the turn |
| turn > 300 s / timeout storms | Ollama queue contention — check what else is loaded; phases C/F/G/H must run serially |
| wake or follow-up capture missed on the rig | replay once, then excluded-INCONCLUSIVE (never FAIL on ASR flake) |
| `/` opens the omni palette instead of the picker | composer not focused — the picker owns `/` only inside the textarea |
| advisor baselines pass after a re-ingest that changed cleaning | corpus-lineage gap until the manifest fingerprint ships — re-ingest ⇒ `--update-baseline` on all advisor cases |
| `--tls` run 502s only on advisor routes | `ADVISOR_BASE_URL=https://…` export missing from run_stack's TLS block |
| viewer e2e advisor checks SKIP | advisor absent — Phase C is NOT passed; bring up :8095 and rerun |

## Risks / open items

1. **Vosk recognizing "advisor"** — the one hard external dependency of the voice UX.
   Phase D step 1 is the early smoke gate; if recognition is <4/5 in a quiet room, file
   a finding — do NOT widen matching to fuzzy prefixes (wake-path discipline).
2. **Worker thread deaf during THINKING (30–300 s)** — today's design already drops PCM
   in THINKING; the advisor makes it much longer. Accepted for v1; an interruptible call
   + watchdog is a v2 item. Capture pipeline unaffected (separate sink).
3. **Temp-0 ≠ cross-machine determinism** — greedy decode differs across Ollama
   versions/quantizations. Structural assertions primary; content gates restricted to
   planted tokens + `must_contain_any` variant lists; advisor fixtures stay in staging
   until two machines agree.
4. **Memory cosine margin** — session-2 phrasing must clear the 0.6 threshold with
   margin ≥ 0.1 at calibration (read `nearest_distance` from the new `memory` event) or
   the fixture's phrasing gets rewritten. `expect_memory_recall_min` stays Info-degraded
   until the event ships.
5. **Promotion path** — staging never gates `all` (by design). Advisor cases stay in
   staging permanently; enforcement comes from a scheduled `--fixtures staging` run with
   exit-code checking, not from bending the split semantics that protect media baselines.

## Suggested implementation slicing (follow-up PRs)

1. **PR: advisor `memory` SSE event + eval harness capabilities** (§3.1) — smallest,
   unblocks fixture calibration; includes the 8 unit tests + manifest KNOBS/fingerprint.
2. **PR: viewer integration** (Part 1 + Phase B/C e2e checks) — fastest human-visible demo.
3. **PR: Android voice integration** (Part 2 + unit tests; Phase D/E run on the rig).
4. **PR: fixture bank + phone rig** (§3.2 + §3.4 + `voice_advisor_loop.py`; Phases F/G/H
   executed and the result matrix filled).

Each PR updates this spec's result matrix for the phases it makes executable.
