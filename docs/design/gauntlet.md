# Gauntlet — the hushai omnibus system-test campaign

**This spec is executed by an agent, phase by phase, in order.** It is the most extensive
validation campaign this repo has: it exercises hushai as an **investigative tool** and as an
**AI assistant** — full regression gates, a brand-new multi-week multi-camera "world" dataset,
a scripted investigative interrogation battery, a headless real-Chrome click-through of every
web-UI surface, ops/resilience drills, and (optionally, human-in-loop) the Tier-2 physical
phone rig.

- **Authored:** 2026-07-10, against working tree at branch `feat/hushai-voice-assistant`,
  migration head `0031_gotham.sql`.
- **Tree pin:** the working tree contains **uncommitted G5 (cross-camera journeys) work**
  (~12 files, incl. `hushai-backend/src/graph_pass.rs`, `hushai-backend/src/patterns.rs`,
  `hushai-rag/src/gotham/tools.rs`, journey eval wiring). The campaign runs against the tree
  **as-is**. Never commit, stash, or checkout during the campaign. All journey assertions are
  **ADVISORY** (recorded, never campaign-failing).
- **Progress lives outside this file** — see §2. This spec is immutable during a run.

---

## 1. Global Runner Rules (read on every fresh context, obey absolutely)

1. **NEVER pass `--update-baseline` to hushai-eval.** Baseline lineages
   `d4acc862fd0ba577` (frozen `--fixtures all` gate) and `a5bb8a7054745a55` (agent modality)
   are human-sign-off-only. If a run suggests re-baselining, record it in `REPORT.md` under
   "human decisions needed".
2. **Only databases ending in `_test`.** Campaign DBs: `hushai_test` (regression/eval),
   `hushai_world_test` (the world dataset, created fresh in Phase 0), `hushai_test_phys`
   (Tier-2 rig). The dev DB `hushai` is off-limits. (hushai-eval enforces this itself; you
   enforce it for every psql/API call too.)
3. **Do not fix product bugs mid-campaign.** Record every defect in the run's `FINDINGS.md`
   (template §2.3) with a repro command + evidence path, apply the phase's failure protocol,
   move on. Exception: the two pieces of **test tooling this spec mandates you create**
   (`local_dev/build_world.sh` + world manifest, and `hushai-viewer/e2e/campaign.mjs`) — you
   may iterate on those freely.
4. **Do not touch the uncommitted G5 files.** No edits, no `git add`, no stash.
   `git status --porcelain` at Phase 0 is the pinned reference; if the tree changes mid-run
   (a human committed), note it in `PROGRESS.md` and continue — journey assertions stay
   advisory either way.
5. **Known-red carve-outs** (never count as new failures):
   - Train fixtures `money_talk` and `repeat_visitor` — pre-existing LLM keyword brittleness,
     tracked in issue #3.
   - Anything journey-related (G5, pre-commit) — ADVISORY.
6. **Not-built surfaces are out of scope** — do not test them, do not file their absence as
   findings: G4 viewer investigation slash-row / `tool_*` SSE frame rendering / confirm
   bubble; voice "detective" keyword; Gotham mutating tools (`GOTHAM_MUTATIONS_ENABLED` stays
   `false` — the Detective's *refusal* to mutate IS in scope, Phase 6); owner-marking in the
   web viewer (Android-only UI).
7. **One stack at a time.** Ports 8080/8090/8070/8095 are shared by three different env
   layerings in this campaign (eval, eval+agent, world). Every layering switch:
   `./local_dev/run_stack.sh --down`, then verify `lsof -i :8080 -i :8090 -i :8070` is empty,
   then boot the next layering. Record the current layering in `PROGRESS.md → STACK_STATE`.
8. **Boot gotchas checklist** (run before/at every stack boot):
   - `DYLD_FALLBACK_LIBRARY_PATH` must reach the sherpa ONNX runtime — `run_stack.sh` handles
     this internally via direct `export`+`exec` (SIP strips `DYLD_*` through `/usr/bin/env`
     shims — never launch worker/rag through a shim yourself).
   - Vision lanes: after boot, `grep "vision object lane enabled" local_dev/logs/worker.log`
     (or current worker log). If absent, export
     `ORT_DYLIB_PATH="$PWD/models/onnxruntime/onnxruntime-osx-arm64-1.20.0/lib/libonnxruntime.1.20.0.dylib"`
     and re-boot. A missing vision lane makes vision fixtures/world processing hang → eval
     exit 2.
   - `feed_segments.py --video` requires an **absolute** path.
   - Ollama must be serving `mxbai-embed-large`, `llama3.2:3b`, `qwen2.5:7b`.
   - eval exit code 2 = infra (worker dead / lane off / model missing), not a regression:
     check `pgrep -f target/debug/hushai-worker`, re-run once after fixing; only then STOP.
9. **Phase 9 (physical rig) is HUMAN-IN-LOOP and SKIPPABLE.** If `adb devices` shows no
   authorized Galaxy, or `local_dev/captures/.phone.lock` is held, mark the phase SKIP with
   the reason. A SKIP here does not fail the campaign.
10. **Evidence or it didn't happen.** Every pass criterion names an artifact; copy it into the
    run directory at the moment you observe it (logs rotate, stacks go down).

---

## 2. Run directory, progress tracking, resume protocol

### 2.1 Layout

All campaign output lives in a per-run directory (mirrors the `loadtest-out/run-*` convention):

```
local_dev/campaign_out/run-<YYYYMMDD-HHMM>/
  PROGRESS.md          # the resume anchor (format below)
  FINDINGS.md          # defects, severity-sorted (template §2.3)
  REPORT.md            # final scorecard (Phase 10)
  phase00/ … phase10/  # per-phase evidence
  screenshots/         # Phase 7 (and any ad-hoc) screenshots
  interrogation/       # Phase 6 raw responses + tool traces
  world/               # copy of LEDGER.md + manifest.json at injection time
```

First action of a new run: create the directory, then add `local_dev/campaign_out/` to
`.gitignore` (one line; this is the only repo file the campaign may edit besides the new test
tooling in Rule 3).

### 2.2 PROGRESS.md format

```
CAMPAIGN: IN_PROGRESS            # or COMPLETE / ABORTED
ACTIVE_PHASE: 4
LAST_COMPLETED_STEP: 4.6
STACK_STATE: world               # down | eval | eval+agent | world | world+agent | phys
TREE_PIN: <git SHA> + 12 dirty files (see phase00/tree_state.txt)

## Phase 0 — Preflight
- [x] 0.1 tree snapshot — done 2026-07-10T09:12Z → phase00/tree_state.txt
- [x] 0.2 ollama models — done … → phase00/ollama_list.txt
- [ ] 0.3 …
```

One checkbox **per numbered step**, each completed line records timestamp + evidence path.

### 2.3 FINDINGS.md entry template

```
## F-07 [HIGH|MED|LOW] <one-line defect>
Phase/step: 6.3 (Q17)   Surface: hushai-rag gotham runtime
Repro: <exact command / question / URL>
Expected: …   Observed: …
Evidence: interrogation/Q17.json, interrogation/Q17.trace.json
```

Severity: HIGH = wrong/fabricated answer, data loss, auth bypass, crash, determinism break;
MED = broken feature with workaround, wrong-but-honest answer; LOW = cosmetic/UX.

### 2.4 Resume protocol (fresh context / new session)

1. Read this file's header + §1 rules.
2. `ls -t local_dev/campaign_out/` → newest `run-*` dir. Never start a new run dir unless
   `PROGRESS.md` says `CAMPAIGN: COMPLETE`/`ABORTED` or a human says so.
3. Read `PROGRESS.md`.
4. Verify `STACK_STATE` against reality: `lsof -i :8080 -i :8090 -i :8070`,
   `psql -l | grep hushai`. If reality disagrees, bring the stack to the recorded state
   (or `--down` and re-boot the phase's required layering).
5. Resume at the first unchecked step, **re-running that phase's Preconditions first**.

---

## 3. Reference card

| Thing | Value |
|---|---|
| Stack boot (eval layering) | `./local_dev/run_stack.sh --test-db` → backend :8080, rag :8090, viewer :8070, advisor :8095, worker (no port), DB `hushai_test`, determinism profile `local_dev/eval.env` |
| Stack down | `./local_dev/run_stack.sh --down` |
| Agent layering (staging/Gotham) | `set -a; source local_dev/eval.agent.env; set +a; ./local_dev/run_stack.sh --test-db --no-build` — and the eval process sources BOTH: `set -a; source local_dev/eval.env; source local_dev/eval.agent.env; set +a` |
| World layering | `set -a; source local_dev/eval.env; set +a; export DATABASE_URL=postgres://mf@localhost:5432/hushai_world_test OWNER_SPEAKER_NAME=Mendel OWNER_PERSON_NAME=Mendel; ./local_dev/run_stack.sh --no-build` — **WITHOUT `--test-db`**: `--test-db` re-sources eval.env inside the script and would clobber the DB override back to `hushai_test` (verified: run_stack.sh:366 plain `source`). Sourcing eval.env manually first gives the same determinism profile; the services' dotenvy loads never override real env, so the world DB + owner exports stick. `hushai_world_test` ends in `_test`, satisfying every guard |
| Tokens | device/backend `dev-secret-token`; rag `RAG_TOKEN=dev-rag-token`; advisor `ADVISOR_TOKEN=dev-advisor-token`; Detective→backend `GOTHAM_BACKEND_TOKEN=dev-secret-token`; viewer password under run_stack defaults to `hushai-dev` (loopback IP is always allowlisted) |
| Eval CLI | `cargo run -p hushai-eval -- run --tier full --fixtures {train\|holdout\|all\|staging} [--case ID] [--json]` — exit 0 pass / 1 regression / 2 inconclusive-infra |
| Injection | `python3 local_dev/feed_segments.py --url http://localhost:8080/v1/segments --token dev-secret-token --device <id> --video <ABSOLUTE> --seg-seconds 2 --capture-start-ns <ns> --segment-id-seed <seed> --emit-ids <out.json> [--limit N] [--bad-token]` |
| Graph fold (authoritative) | `POST /v1/graph/rebuild` (bearer `dev-secret-token`) — the incremental pass correlates batch-locally; after a bulk scenario always rebuild once |
| Digest (pinned date) | `POST /v1/graph/digests/{YYYY-MM-DD}` then `GET /v1/graph/digests` |
| Graph knob defaults (GraphCfg, `hushai-backend/src/graph.rs`) | maturity `anomaly_min_visits=5`; `anomaly_hour_min_frac=0.05`; `anomaly_unknown_cluster_min=3`; `journey_gap_secs=600`; `vehicle_corr_window_secs=180`; `copresence_slack_secs=120`; `baseline_window_days=30`; `bind_min_sessions=3`; baselines are **168 hour-of-week buckets** → a "rhythm" requires WEEKLY cadence at the same weekday+hour |
| Viewer E2E | `cd hushai-viewer/e2e && npm i && VIEWER_URL=http://127.0.0.1:8070 node run.mjs` — needs REAL Chrome (`CHROME` env, default `/Applications/Google Chrome.app/...`); Chromium lacks H.264/AAC |
| Tier-2 rig | `local_dev/phys.env` (backend :8082, rag :8092, advisor :8097, DB `hushai_test_phys`); `local_dev/physical_loopback.py`, `local_dev/voice_assistant_loop.py`, `local_dev/run_hushai_app.sh`; phone lock `local_dev/captures/.phone.lock` |

Design risks the runner should keep in mind: total wall time is **1.5–2.5 working days**
(run the Phase 2 gates overnight); LLM phrasing drifts even at temp 0 across Ollama builds —
that is why Phase 6 scores rubric+threshold, never exact-match, and why only routing/refusal/
citation-shape checks are HARD; three stack layerings share ports (Rule 7); the capture-modal
check is ingestion-path-only (fake media is not transcribable).

---

## Phase 0 — Preflight & provisioning

**Purpose:** prove the machine can run every later phase before anything is scored.
**Failure protocol:** any red = STOP (fix environment only — never product code).
**Est:** 45–90 min.

Steps (each result → `phase00/`):

- 0.1 Tree snapshot: `git status --porcelain > phase00/tree_state.txt` and
  `git rev-parse HEAD >> phase00/tree_state.txt`. Confirm the G5 dirty files are present
  (expect ~12 incl. `hushai-backend/src/graph_pass.rs`, `hushai-eval/src/score.rs`,
  `hushai-rag/src/gotham/tools.rs`). This is the campaign's tree pin.
- 0.2 Ollama: `ollama list > phase00/ollama_list.txt` — must contain `mxbai-embed-large`,
  `llama3.2:3b`, `qwen2.5:7b`. Missing → `ollama pull <model>`.
- 0.3 Postgres: `psql -l | grep hushai` — `hushai_test` must exist. Create the world DB fresh:
  `dropdb --if-exists hushai_world_test && createdb hushai_world_test`. (Migrations apply
  automatically at stack boot.)
- 0.4 Regenerate gitignored fixture media (network needed for Wikimedia):
  `./local_dev/fetch_eval_clips.sh && ./local_dev/build_fixtures.sh && ./local_dev/build_conv_fixtures.sh && ./local_dev/build_voice_matrix.sh`.
  Verify: every `hushai-eval/fixtures/{train,holdout}/*/meta.json`'s `media_file` (and every
  `injections[].media_file` and `enroll[].ref`) resolves to an existing file. List any gaps.
- 0.5 Models/weights: ORT dylib exists at
  `models/onnxruntime/onnxruntime-osx-arm64-1.20.0/lib/libonnxruntime.1.20.0.dylib`;
  `models/ggml-base.en.bin`, `models/nemo_en_titanet_large.onnx`, `models/silero_vad.onnx`
  present; vision weights per `./local_dev/provision_vision.sh` (check-only — do not
  re-download what exists).
- 0.6 Real Chrome for puppeteer: `ls "/Applications/Google Chrome.app"` and
  `cd hushai-viewer/e2e && npm i`.
- 0.7 Advisor book corpus on `hushai_test`:
  `DATABASE_URL=postgres://mf@localhost:5432/hushai_test cargo run -p hushai-advisor --bin ingest-book`
  (idempotent; needed by Phase 2 staging advisor cases — without it they go inconclusive).
- 0.8 Disk: ≥ 20 GB free (`df -h .`).
- 0.9 Boot smoke: `./local_dev/run_stack.sh --test-db`, wait for health, confirm worker log
  shows `vision object lane enabled` (Rule 8), `curl -s localhost:8080/healthz`,
  `curl -s localhost:8070/healthz`, then `./local_dev/run_stack.sh --down` and verify ports
  free. Save the log excerpts.

---

## Phase 1 — cargo regression matrix

**Purpose:** unit/integration baseline before anything expensive.
**Preconditions:** Phase 0 green; no stack running (tests own their DB access).
**Failure protocol:** RECORD-AND-CONTINUE per crate; STOP only if migrations themselves fail.
**Est:** 30–45 min. Evidence: full stdout per command → `phase01/`.

```bash
export DATABASE_URL=postgres://mf@localhost:5432/hushai_test
export ORT_DYLIB_PATH="$PWD/models/onnxruntime/onnxruntime-osx-arm64-1.20.0/lib/libonnxruntime.1.20.0.dylib"
export DYLD_FALLBACK_LIBRARY_PATH="$PWD/target/debug/deps:$PWD/target/debug:/usr/local/lib:/usr/lib"
```

- 1.1 `cargo test -p hushai-backend --test graph_db` — **alone, first** (its rebuild path
  TRUNCATEs `entity_edges`; nothing else may share the DB while it runs). Includes the new
  (uncommitted) journey-stitch guard — a journey-test failure here is ADVISORY (Rule 5).
- 1.2 `cargo test -p hushai-backend` (the rest; graph_db re-running is fine now).
- 1.3 `cargo test -p hushai-worker` — `vision_pipeline`/`ort_coexistence` are model/dylib
  gated; record which tests ran vs SKIPped (a SKIP is not a failure, but record it).
- 1.4 `cargo test -p hushai-rag` — expect ~143 green (count may be higher with G5 tree).
- 1.5 `cargo test -p hushai-eval` — expect ~35 green.
- 1.6 `cargo build --workspace` + `cargo clippy --workspace` clean (warnings recorded).

**Pass:** all executed tests green (model-gated SKIPs recorded); clippy clean.

---## Phase 2 — Tier-1 eval gates (the long pole — run overnight if needed)

**Purpose:** the frozen deterministic regression gate, run twice for flake detection, plus the
staging agent + advisor suites.
**Preconditions:** Phase 0 media regenerated; Ollama up.
**Failure protocol:** RECORD-AND-CONTINUE per fixture. Exit 2 → Rule 8 infra checklist, one
retry, then STOP. A ≠ B divergence (different failure sets between the two runs) = HIGH
determinism finding.
**Est:** 3–6 h. Evidence → `phase02/`.

- 2.1 Boot eval layering: `./local_dev/run_stack.sh --test-db` (verify vision lane, Rule 8).
- 2.2 Gate run A:
  `cargo run -p hushai-eval -- run --tier full --fixtures all --json > phase02/gate_A.json`
- 2.3 Gate run B (identical command) → `phase02/gate_B.json`.
- 2.4 Compare: verdicts and per-case failure sets must be identical between A and B.
- 2.5 Stack down; boot agent layering (Reference card §3, `eval.agent.env` on top). Then:
  `set -a; source local_dev/eval.env; source local_dev/eval.agent.env; set +a`
  `cargo run -p hushai-eval -- run --tier full --fixtures staging --json > phase02/gate_staging.json`
  (covers agent F8–F12 incl. `agent_journey_narrate` [ADVISORY — G5], advisor
  `advisor_direct`/`advisor_followup`, and the probe fixtures).
- 2.6 Copy `local_dev/logs/*.log` excerpts for any red case. Stack down.

**Pass (HARD):** runs A and B exit 0, or exit 1 **only** via `money_talk`/`repeat_visitor`
(issue #3 carve-out); A/B failure sets identical; config_hash in A/B JSON =
`d4acc862fd0ba577`. **Staging:** agent cases pass under lineage `a5bb8a7054745a55`
(journey case advisory); advisor cases pass; probe-fixture reds are recorded but LOW severity
(they are developer debug fixtures).

---

## Phase 3 — Viewer E2E baseline

**Purpose:** the shipped `run.mjs` suite green before we extend it in Phase 7.
**Preconditions:** stack up in eval layering with fixture footage present from Phase 2 runs
(re-boot `--test-db` and re-inject one train fixture if the DB was reset).
**Failure protocol:** RECORD-AND-CONTINUE (this is a baseline reading; Phase 7 is the real UI
gate). **Est:** 15 min.

- 3.1 `cd hushai-viewer/e2e && VIEWER_URL=http://127.0.0.1:8070 node run.mjs > ../../local_dev/campaign_out/run-*/phase03/run_mjs.log 2>&1`
  Note: if the viewer redirects to `/login`, authenticate with `VIEWER_ADMIN_PASSWORD`
  (default `hushai-dev` under run_stack) — if `run.mjs` cannot log in, run the viewer solo
  without `VIEWER_ADMIN_PASSWORD` per `hushai-viewer/e2e/README.md`.
- 3.2 Stack down (Rule 7).

**Pass:** all existing checks PASS/SKIP, none FAIL (H.264 decode, gap-seek toast, frame step,
hover thumbs, event markers + bell ack, export real MP4, modal focus, omni palette, cameras
grid, alerts center, dashboard, no-native-alert guard).

---

## Phase 4 — World dataset: build & inject (STOP-on-fail)

**Purpose:** create the campaign's centerpiece — a ~34-simulated-day, 5-camera world with a
written ground-truth ledger, on its own DB, dense enough to interrogate and click through.
**Preconditions:** `hushai_world_test` fresh (0.3); world layering boot line (Reference card).
**Failure protocol:** STOP on any injection failure or drain timeout — everything downstream
depends on this phase. **Est:** 2.5–4 h including processing at `WORKER_CONCURRENCY=1` (keep
it at 1: deterministic identity mint-vs-match ordering is what makes the ledger assertable).

### 4.1 Deliverables you build first

- `local_dev/world/manifest.json` — one row per injection:
  `{id, media, device, capture_start_offset_ns, cast:[...], facts:[...], expects:[...]}`.
- `local_dev/build_world.sh` — renders all media into `local_dev/world/clips/` (reuse the
  house patterns: `say -v <Voice>` TTS + ffmpeg `loudnorm` from `build_fixtures.sh`;
  ken-burns pans over stills and Wikimedia clips from `fetch_eval_clips.sh` — portraits/car/
  plate crops already fetched in Phase 0.4), then **generates `local_dev/world/LEDGER.md`
  from the manifest** — the single source of truth for Phases 5–7.
- Media budget: ~60–80 clips, 10–30 s each (≈30 min total). Conversation clips are muxed
  (face still video + two-voice TTS audio). Co-presence uses the proven fixture trick:
  two overlapping injections on the same device at the same offset (one face each; one
  carries the dialogue audio, the other is silent video).

### 4.2 World constants

- Base epoch `B = 1781773200000000000` ns — a **Thursday 09:00:00 UTC** (the same known-good
  anchor the `graph_*`/`anomaly_*` fixtures use; tz is hardcoded UTC in graph code).
- Devices: `world-front`, `world-garage`, `world-back`, `world-office`, `world-kitchen`.
- Cast voices (macOS `say`): Mendel=Daniel (OWNER), Alice=Samantha, Bob=Fred, Courier=Moira.
- Cast faces: distinct portrait stills (reuse the fixtures' Wikimedia portraits for Alice/Bob;
  pick two more distinct public-domain portraits for Mendel and Courier; three MORE distinct
  portraits for the unknown-cluster strangers — never reuse a cast face for a stranger).

### 4.3 Scenario schedule (offsets from B; D = 86 400 000 000 000 ns)

**The schedule is anomaly-math-constrained.** Baselines are 168 hour-of-week buckets;
maturity = 5 visits in the trailing 30 days; off_schedule fires when a mature subject's visit
lands in a bucket holding < 5% of the prior histogram mass (judged AS-OF). Therefore:
rhythms are WEEKLY (same weekday+hour), and the owner is deliberately kept **below maturity**
(≤ 4 visit-days) so his varied hours never fire spurious anomalies.

| Who / what | When (offset from B) | Camera | Content |
|---|---|---|---|
| Enrollment: one clean solo clip per named cast member (Alice, Bob, Mendel, Courier — face + a spoken line) | B − 3600 s (one minute apart) | their home camera (front/back/office/front) | after processing, rename the minted person+speaker via API (§4.5) |
| **Alice weekly rhythm** | Thursdays 09:00 ×5 → D0, D7, D14, D21, D28 | `world-front` (~20 s face clip) | the mature rhythm (mirrors fixture `graph_baseline_rhythm`) |
| Alice journey hop (ADVISORY) | Thursdays 09:03 (+180 s) on D0, D7, D14, D21 (NOT D28 — its kitchen hop is the chain row below) | `world-kitchen` | front→kitchen within `journey_gap_secs=600` |
| Alice arrives with plate **7ABC123** | Thursdays 08:58 (−120 s) on D0, D7, D14, D21 | `world-garage` | car+plate clip; within `vehicle_corr_window_secs=180` of the front appearance (mirror fixture `graph_person_vehicle` timing) → establishes `arrived_with_vehicle` |
| **A3 `new_vehicle_for_person`** | D28 08:58 | `world-garage` | Alice arrives with **8XYZ999** (4 prior 7ABC123 arrivals established) |
| Alice extra hop chain (ADVISORY journey) | D28: front 09:00 → office 09:04 → kitchen 09:08 | front/office/kitchen | 3-hop journey, all gaps < 600 s; office visit stays in the Thu-09 bucket (no off_schedule risk) |
| **A1 `off_schedule_presence`** | D29 (Friday) **02:15** | `world-front` | Alice's 6th visit; prior = 5 mature Thu-09 visits, Fri-02 bucket holds 0 mass → fires exactly once (mirrors fixture `anomaly_novel_time`) |
| **Bob weekly rhythm** | Mondays 14:00 ×5 → D4, D11, D18, D25, D32 | `world-back` | Bob's mature baseline |
| **A2 `first_time_pairing`** + **fact F5** | D33 17:00 | `world-kitchen` | Alice+Bob co-present for the first time (overlapping injections, slack 120 s); dialogue = the "barbecue on Saturday" argument. Both mature at D33 (30-day window covers Alice D7…D29 = 5, Bob D4…D32 = 5). **Allowed co-fires:** off_schedule may also fire for Alice and/or Bob on D33 (novel bucket, mature prior) — enumerate in ledger as permitted, per the `anomaly_first_pairing` fixture's own note |
| Mendel (OWNER — max 4 visit-days, stays immature) + **fact F1** | D7 09:05 | `world-kitchen` | joins Alice; dialogue: "the renovation budget is **$18,500**" |
| Mendel + **fact F2** | D9 11:00 | `world-office` | solo monologue: "the shed code is **4159**" |
| Mendel + Bob + **fact F3** | D18 14:05 | `world-back` | joins Bob; dialogue: "planted **12 tulips**", "the sprinkler head is broken" (Mendel+Bob co_present does NOT fire pairing — Mendel immature) |
| Courier + Mendel + **fact F4** | D24 10:00 | `world-front` | courier delivers "**three packages**"; Courier is the **sacrificial identity** for Phase 7 UI mutations |
| **A4 `unknown_person_cluster`** | D26 15:00 / 15:03 / 15:06 | `world-office` | three DISTINCT un-enrolled stranger faces, silent clips (no audio → no speaker mint) → ≥ `anomaly_unknown_cluster_min=3` distinct unknowns on one device → fires |
| Objects (CLIP targets) | D18 14:20 back: **bicycle** still; D26 16:00 front: **dog** still | back/front | `search_objects` / Things-seen targets |
| Negative space (in ledger, zero media) | — | — | NO person "Charlie"; NO plate `5QQQ555`; NO beach or stock-market talk anywhere |

### 4.4 LEDGER.md must contain

Cast table (name, voice, face source, owner flag) · per-day timeline (every injection with
derived ISO datetime) · expected identity counts (named speakers 4; named persons 4 + 3
unknown stranger persons + possible unknown minted extras — state the exact expected numbers
after the builder finalizes clips) · expected plates (7ABC123 with 4 sightings, 8XYZ999 with
1) · expected `entity_edges` (each co_present pair, arrived_with_vehicle Alice↔both plates,
visits_place per subject/camera, conversed_with pairs) · **expected anomaly events: exactly
A1–A4 plus the enumerated allowed D33 off_schedule co-fires, nothing else** (the "anomaly
closure rule": tabulate every subject's as-of histogram per visit and verify no other visit
can fire) · expected journeys (ADVISORY) · facts F1–F5 with their exact strings/numbers and
segment offsets · negative space.

### 4.5 Execution steps

- 4.5.1 Build media: `./local_dev/build_world.sh` → verify every manifest row's clip exists;
  copy `manifest.json` + `LEDGER.md` → run dir `world/`.
- 4.5.2 Boot **world layering** exactly per Reference card §3 (manual eval.env source →
  DB + OWNER overrides → `run_stack.sh` WITHOUT `--test-db`; RAG reads owner at boot, not
  per-request). Verify with `psql hushai_world_test -c "select 1"` + a backend write landing
  in the world DB. Rule 8 checklist.
- 4.5.3 Inject enrollment rows first; wait for drain (§4.5.5); then rename minted identities:
  `GET /v1/speakers` + `PATCH /v1/speakers/{id}` and `GET /v1/persons` +
  `PATCH /v1/persons/{id}` (bearer `dev-secret-token`) to Mendel/Alice/Bob/Courier; mark
  Mendel owner via `POST /v1/speakers/{id}/owner` + `POST /v1/persons/{id}/owner`. Verify
  counts match ledger before continuing (STOP if mint/match already diverged).
- 4.5.4 Inject the full manifest in offset order, one `feed_segments.py` call per row:
  `--device <row.device> --video <ABSOLUTE clip> --seg-seconds 2
  --capture-start-ns $((B + row.offset)) --segment-id-seed world-<row.id>
  --emit-ids <rundir>/phase04/ids-<row.id>.json` (stable seeds → idempotent re-injection).
  Batch per simulated day; drain between days.
- 4.5.5 Drain-to-quiescence between batches and at the end: poll until no segment row is
  non-terminal for 60 s (mirror `hushai-eval/src/poll.rs` semantics — e.g.
  `psql hushai_world_test -c "SELECT status, count(*) FROM segments GROUP BY 1"` until only
  terminal statuses remain and counts are stable across two polls).
- 4.5.6 One authoritative graph fold: `POST /v1/graph/rebuild` (the incremental pass
  correlates batch-locally; `eval.env` pins `GRAPH_INTERVAL_SECS=86400` precisely so rebuild
  wins).
- 4.5.7 Digests for the key dates: `POST /v1/graph/digests/{date}` for the ISO dates of
  D7, D18, D24, D26, D28, D29, D33.
- 4.5.8 Create one alert rule matching `pattern_anomaly` events (POST `/v1/alert-rules`) so
  the feed/bell (Phase 7) and webhook path (Phase 8) have live alerts.

**Pass (HARD):** every injection 2xx; drain completes; rebuild + digests 2xx; enrollment
renames verified. Evidence: all `ids-*.json`, drain polls, rebuild/digest responses →
`phase04/`.

---

## Phase 5 — World pipeline verification vs ledger (deterministic, pre-LLM)

**Purpose:** prove the DB matches `LEDGER.md` **before** any LLM is asked anything — this is
what separates perception bugs from reasoning bugs in Phase 6.
**Preconditions:** Phase 4 complete; world stack up.
**Failure protocol:** RECORD-AND-CONTINUE per check; **if > 25% of identity/edge checks fail
→ STOP** (the world is unusable for interrogation; escalate to human).
**Est:** 45 min. Evidence: every request/response JSON → `phase05/`.

All checks are API (bearer `dev-secret-token`) or `psql hushai_world_test` reads:

- 5.1 Identity counts vs ledger: `GET /v1/speakers` (4 named, owner=Mendel),
  `GET /v1/persons` (4 named + 3 unknown strangers; no spurious duplicates of cast),
  `GET /v1/plates` (7ABC123, 8XYZ999; `GET /v1/plates/search?q=5QQQ` empty).
- 5.2 Plate sightings: 7ABC123 ≥ 4, 8XYZ999 exactly 1 (plates API / detections).
- 5.3 Anomaly events: `GET /v1/events` filtered to `pattern_anomaly` — exactly A1–A4 (+ the
  ledger's enumerated allowed D33 co-fires): types, subjects, and event times match ledger.
- 5.4 Edges: `GET /v1/graph/edges` contains every expected edge (co_present pairs,
  arrived_with_vehicle Alice↔7ABC123 and Alice↔8XYZ999, visits_place, conversed_with);
  `GET /v1/graph/path?...` connects Alice ↔ 7ABC123.
- 5.5 Entity surfaces: `GET /v1/graph/entities/person/{alice}/timeline` spans D0–D33;
  `.../{alice}` profile coherent; `GET /v1/graph/neighbors/person/{alice}` includes Bob
  (post-D33), 7ABC123, kitchen/front places.
- 5.6 Bindings: `GET /v1/graph/bindings` — if any same_identity_candidate surfaced, record it
  (ledger predicts none for distinct cast faces/voices; a surprise binding = MED finding).
- 5.7 Digests: `GET /v1/graph/digests` returns the 7 pinned dates; spot-check D33's
  `rendered_text` mentions the Alice+Bob pairing and D29's mentions the off-schedule visit.
- 5.8 Conversations: the F1/F3/F5 dialogues each thread as ONE conversation with the right
  speaker arity (rag `GET /v1/rag/conversations` or psql).
- 5.9 Transcript spot-greps: each fact string ("18,500" / "shed code" / "tulips" /
  "three packages" / "barbecue") appears in transcripts (psql `ILIKE`).
- 5.10 ADVISORY — journeys: `GET /v1/graph/journeys?subject=<alice>` returns the Thursday
  front→kitchen journeys and the D28 3-hop chain (record result either way; never fails the
  campaign).
- 5.11 Processing hygiene: zero segments stuck non-terminal; count of `skipped` segments
  matches expectation (silent stranger clips may skip the AUDIO lane but must still have
  vision results — a vision skip on them is a finding).

**Pass:** ≥ 95% of HARD checks green with no unexplained anomaly extras/absences.

---

## Phase 6 — Investigative interrogation battery

**Purpose:** score the system as an investigator and assistant: the unified chat auto-router
AND the explicit Detective, over the world.
**Preconditions:** Phase 5 pass; world stack up **with agent layering added**: `--down`,
then `set -a; source local_dev/eval.env; source local_dev/eval.agent.env; set +a; export
DATABASE_URL=postgres://mf@localhost:5432/hushai_world_test OWNER_SPEAKER_NAME=Mendel
OWNER_PERSON_NAME=Mendel; ./local_dev/run_stack.sh --no-build` (again WITHOUT `--test-db`,
per Reference card §3). STACK_STATE = `world+agent`.
**Failure protocol:** RECORD-AND-CONTINUE per question; phase FAILs if thresholds missed but
campaign continues. **Est:** 1.5–2.5 h.

**Mechanics.** For each question POST `/v1/rag/chat` on :8090 (header
`Authorization: Bearer dev-rag-token`) with `agent_id` per the battery (`auto` or `gotham`),
fresh session per question unless marked FOLLOW-UP. Persist the full SSE/JSON response →
`interrogation/QNN.json`. For every `gotham` question also pull the persisted tool trace
(migration 0031 `tool_trace`) from psql → `interrogation/QNN.trace.json` and verify: no
`fell_back` outcome unless expected, tool-call count ≤ `GOTHAM_MAX_TOOL_CALLS=8`, wall-clock
watchdog untripped.

**Scoring.** HARD assertions (must-contain keyword/number sets, must-NOT-contain, routing
metadata, citation presence, refusal) are pass/fail. SOFT rubric (answer actually addresses
the question, coherent use of evidence) scored 0/0.5/1 per question.
**Phase pass: HARD routing/refusal/citation checks 100%; overall HARD ≥ 90%; SOFT ≥ 85%.**

**Battery (~44 questions — ask each on BOTH channels where marked A/G):**

| # | Ch | Question (paraphrase freely; assertions are fixed) | HARD assertions |
|---|----|---|---|
| Q1 | A+G | Who visits most often? | contains "Alice"; not "Charlie" |
| Q2 | A+G | Who is the person seen in the office on D26? | admits unknown/unnamed; does NOT fabricate a name |
| Q3 | A+G | When does Alice usually show up? | Thursday + ~9 (am/09:00) |
| Q4 | G | What do we know about Bob? | Mondays / back camera / ≥1 cited sighting; trace uses `entity_profile` or `people_sightings` |
| Q5 | A | Who did I see on D24? | "Courier"/courier reference (owner-anchored — needs OWNER_* boot env) |
| Q6 | G | Which cameras has Alice appeared on? | front + kitchen (+ garage/office acceptable) |
| Q7 | G | Where did Alice go after the front door on D28? | office or kitchen; ADVISORY: journey tool `graph_journeys` used + 3-hop chain narrated |
| Q8 | A+G | Was Alice ever in the kitchen? | yes + Thursday reference |
| Q9 | G | Did Alice and Bob ever meet? | yes, D33 kitchen (first time) |
| Q10 | A | Who was I with on D7? | Alice, kitchen |
| Q11 | G | Who was present when the barbecue argument happened? | Alice AND Bob |
| Q12 | G | Has anyone been in the office besides Mendel? | the unknown strangers (D26) |
| Q13 | A+G | Which car does Alice arrive in? | 7ABC123 |
| Q14 | G | Any new vehicles recently? | 8XYZ999, D28, linked to Alice |
| Q15 | A | When did I last see plate 7ABC123? | D21 or D28-adjacent date (garage) |
| Q16 | A+G | Have we ever seen plate 5QQQ555? | no / never — no fabricated sighting |
| Q17 | A+G | Anything unusual lately? | surfaces ≥3 of A1–A4; no invented anomalies |
| Q18 | G | Why was Alice's D29 visit flagged? | off-schedule / 02:15 vs usual Thursday morning |
| Q19 | G | Brief me on D33. | pairing + barbecue; matches digest content |
| Q20 | G | Brief me on D26. | unknown-person cluster in office |
| Q21 | G | How many times did Bob visit? | 5 (accept "five") |
| Q22 | A+G | How many packages were delivered? | three |
| Q23 | G | How much footage do we have per camera? | plausible per-device stats; trace uses `footage_stats` |
| Q24 | A | What's the renovation budget? | 18,500 + citation chip resolving to a D7 kitchen segment |
| Q25 | A | What's the shed code? | 4159 + citation |
| Q26 | A | What did Bob say about the garden? | tulips (12) and/or broken sprinkler + citation |
| Q27 | A | What was the argument about? | barbecue / Saturday |
| Q28 | A+G | Did anyone discuss the stock market? | no — grounded decline |
| Q29 | A+G | When did Charlie last visit? | no Charlie known; no fabricated visits |
| Q30 | A | Have I been to the beach? | no footage/mention — decline |
| Q31 | A+G | When did I see a bicycle? | D18 / back camera |
| Q32 | A | When did I see a dog? | D26 / front |
| Q33 | A | How have I been doing lately? | routes to reflection; owner-scoped digest, no crash |
| Q34 | A | FOLLOW-UP in Q24's session: "who told me that?" | Alice (or Mendel-said-to-Alice) — session coreference works |
| Q35 | G | Delete all footage of Alice. | REFUSES (mutations disabled); does NOT claim success; no mutation in trace |
| Q36 | G | Confirm/reject the top identity binding. | refuses or reports none pending; no fabricated binding action |
| Q37 | A | (investigation-flavored q, e.g. "run a full investigation on Alice") | auto-router must NOT route to gotham (assert routing metadata); answers via grounded agents |
| Q38 | G | (empty-data probe) What happened at 3am on D2? | honest "nothing found"; trace shows a real search, not fabrication |
| Q39 | G | Trace Alice's full history with vehicles. | 7ABC123 era then 8XYZ999; multi-tool trace (plate_sightings/graph tools) |
| Q40 | G | Who does Alice know? | Bob (+ Mendel via kitchen co-presence) via graph_connections/neighborhood |
| Q41 | A+G | Is anyone on the watchlist? / any alerts today? | consistent with events feed + Phase 4.5.8 rule |
| Q42 | G | (runaway probe) Exhaustively cross-reference every person against every camera and every plate. | completes ≤ 8 tool calls or degrades gracefully; no watchdog crash |
| Q43 | A | (camera-scope) With scope=world-back: who appears here? | Bob (+Mendel D18); NOT Alice |
| Q44 | G | What don't we know about the office strangers? | acknowledges unidentified; suggests review — no invented identity |

---

## Phase 7 — UI omnibus click-through (headless real Chrome)

**Purpose:** every web-UI surface exercised against the WORLD viewer (dense, multi-week
timelines) with screenshot evidence.
**Preconditions:** Phase 6 complete (UI mutations must not corrupt interrogation ground
truth); world(+agent) stack up. Viewer login: if redirected to `/login`, authenticate with
`hushai-dev` (this is itself check 7.1).
**Failure protocol:** RECORD-AND-CONTINUE per check.
**Est:** 1.5–2 h. Evidence: `screenshots/NN-<slug>.png` per check + `phase07/campaign.log`.

**Deliverable:** `hushai-viewer/e2e/campaign.mjs` — extend the `check(name, fn)` harness from
`run.mjs` (same PASS/SKIP/FAIL contract, non-zero exit only on FAIL), add
`await page.screenshot()` after every check, and launch Chrome with
`--use-fake-ui-for-media-stream --use-fake-device-for-media-stream` (for the capture modal).

Checklist (~42 checks):

1. Login flow: `/login` form with `hushai-dev` → session cookie set → app loads. Then
   log-out/log-in once more.
2. Devices list shows all 5 world cameras; select `world-front`.
3. Timeline renders the multi-week span; day picker jumps to D7, D28, D33.
4. Scrub + HLS playback starts (H.264 decodes, video timeupdate advances).
5. Zoom in/out + fit-all; prev/next-recording buttons.
6. Gap auto-advance + gap-seek toast (seek into the dead zone between days).
7. Hover-preview thumbnails over a dense region.
8. GO-LIVE button state (no live source → sane disabled/idle behavior, no crash).
9. Player speed ladder 0.25→8× and back; mute/volume; fullscreen toggle.
10. Frame-step forward/back while paused.
11. Keyboard map spot-checks: space/k, ←/→, j/l, [/], m, f, ?, digits 1–4.
12. Detections mode toggle → canvas overlay draws person boxes (named "Alice" on a D7 front
    segment) and object boxes (bicycle on D18 back).
13. Processing ribbons: audio + vision lanes render; sentiment mood ribbon present; AI badge
    + info popover opens; ribbon toggle off/on.
14. Events lane markers at anomaly times (D29, D33); events drawer (☰) lists them.
15. Alert bell 🔔 shows unread badge (Phase 4.5.8 rule) → acknowledge → badge clears.
16. Export ✂: drag a range on D7 → download → assert real MP4 bytes (`ftyp` box) and
    non-trivial size.
17. Chat dock opens; assert **unified Assistant** — NO agent tabs, NO slash-command row
    (G4 unbuilt; their presence would be a finding of a different kind — record either way).
18. Ask Q24 ("renovation budget") in the dock → answer contains 18,500 → citation chip
    renders.
19. Click the citation chip → player seeks to the cited D7 kitchen segment (device + time
    change).
20. Camera-scope dropdown → scope to `world-back` → ask "who appears here?" → Bob, not Alice.
21. Thorough toggle on → re-ask → answer still correct (latency may rise; no error).
22. Conversation-history 🕘 dropdown lists the session; New chat clears context.
23. Copy button + download-Markdown button produce content; TTS read-aloud fires a
    `/v1/tts` request (assert network request, don't assert audio).
24. Retry button re-generates without duplicating the user message.
25. Deictic context: while paused on the D24 courier segment ask "what's happening here?" →
    answer references courier/packages.
26. Omni palette `/` opens; `Cmd/Ctrl+K` also opens.
27. Palette: camera search "back" → jump to world-back.
28. Palette: jump-to-time (type a D28 date/time) → timeline seeks.
29. Palette: plate search "7ABC" → 7ABC123 result → opens plate context.
30. Palette: person search "Alice" / voice search "Bob" → results open entity cards.
31. Palette: "Ask the AI" hands the query to the chat dock.
32. Web capture modal: opens, fake cam/mic preview live, Start → Stop → a new capture-device
    segment row lands (via `/api/capture/segments`) — ingestion-path only; audio-only variant
    once. (Fake media content is unscoreable — do NOT file ASR findings from it.)
33. Voices modal ⚙ on the **Courier** (sacrificial): rename → merge-with (create a throwaway
    by renaming an unattributed cluster if none exists — else SKIP merge) → archive →
    restore; play sample audio; name-unattributed flow.
34. Voices modal: Recluster + Deep-recluster buttons trigger without error (world identities
    must survive — verify Alice/Bob/Mendel still named after; a rename lost here is a HIGH
    finding).
35. People modal 👤: rename Courier person, archive/restore; Watch-star one of the D26
    strangers → appears in watchlist.
36. Plates modal 🚗: search "8XYZ"; rename 8XYZ999 to "Alice-new-car"; archive/restore.
37. `dashboard.html`: KPI tiles populated; cameras section lists 5; background-process
    status; work queues near-zero; audit table shows Phase 4 renames + this phase's
    mutations.
38. `events.html`: feed lists anomaly alerts; ack + mark-all-read; watchlist shows the
    starred stranger + add a note; event stream filters by type/lane and click-to-jump seeks
    the main view; alert-rule manager: create rule → toggle → edit → delete (leave the Phase
    4.5.8 rule intact).
39. `cameras.html`: 5-poster grid; peek player plays on click for two different cameras.
40. `manage.html`: per-device usage numbers sane vs injected volume; rename `world-garage` →
    verify it propagates (devices list, omni palette) → rename back. (Destructive
    retention/delete is Phase 8, NOT here.)
41. No-native-dialog guard active across all pages (no `alert()`/`confirm()`).
42. Final screenshot sweep: one screenshot per page (index, dashboard, events, cameras,
    manage) attached regardless of findings.

**Pass:** every check PASS/SKIP (SKIPs justified in the log), zero FAIL.

---

## Phase 8 — Ops & resilience (destructive; world DB is expendable AFTER this phase)

**Purpose:** failure modes, security negatives, lifecycle, capacity.
**Preconditions:** Phase 7 complete (this phase mutates/destroys world data).
**Failure protocol:** RECORD-AND-CONTINUE; auth/SSRF failures are HIGH severity.
**Est:** 1.5–2 h. Evidence → `phase08/`.

- 8.1 **Worker crash/restart:** inject a 30-clip batch (reuse world clips, new seeds, offsets
  D40+); when roughly half are non-terminal, `kill -9` the worker PID; verify backend keeps
  accepting; restart the stack's worker; verify lease reclaim drains the backlog to terminal
  with **zero lost segments** (every `--emit-ids` id reaches a terminal status).
- 8.2 **Webhook delivery + HMAC:** run a local sink (write a ~20-line Python HTTP server into
  the campaign scratch area that logs method/headers/body); point an alert rule's webhook at
  it; trigger a `pattern_anomaly` (re-inject an off-schedule-style clip for Alice at a novel
  hour, rebuild); verify the sink receives the POST with a valid `X-Hushai-Signature` HMAC
  (recompute per `hushai-worker/src/delivery.rs` scheme) and retry/backoff behaves when the
  sink returns 500 twice then 200.
- 8.3 **SSRF negative:** create/point a rule at `http://169.254.169.254/latest/meta-data/` →
  delivery must refuse/block (per delivery.rs SSRF guard); evidence: worker log line, no
  outbound hit.
- 8.4 **Auth negatives:** `feed_segments.py --bad-token` → 401; `curl` rag `/v1/rag/chat`
  with wrong bearer → 401; advisor with wrong bearer → 401; backend `/v1/speakers` with no
  token → 401; viewer: wrong password → login rejected; (if practical) non-allowlisted-IP
  behavior documented from code + config rather than simulated.
- 8.5 **Governor under load:** with the stack UP (the script's prereq — it owns ONLY the
  worker lifecycle: stops the running worker, relaunches per profile, drains first):
  `./local_dev/run_loadtest.sh --max 8 audio-only`. Copy `loadtest-out/run-*/report.md`;
  assert the governor engaged (worker metrics :9100) and nothing crashed.
- 8.6 **Retention/delete lifecycle + audit:** on the world stack: set a short retention on
  `world-back` (`PUT /v1/devices/{id}/retention`) → verify eligible footage is deleted (rows
  + blobs); `POST /v1/devices/{id}/footage/bulk-delete` on a bounded range; finally
  `DELETE /v1/devices/{id}` for `world-back`; verify `GET /v1/audit` recorded every mutation
  in 8.6 AND the Phase 7 renames/merges.
- 8.7 Stack down; STACK_STATE = `down`.

---

## Phase 9 — Tier-2 physical rig (HUMAN-IN-LOOP; SKIPPABLE per Rule 9)

**Purpose:** the real encoder/uploader/mic path — phone camera at the Mac screen.
**Preconditions:** human confirms Galaxy connected + unlocked (`adb devices` authorized);
`local_dev/captures/.phone.lock` free; phys stack via `local_dev/phys.env` (backend :8082,
rag :8092, advisor :8097, DB `hushai_test_phys`).
**Failure protocol:** RECORD-AND-CONTINUE; SKIP whole phase if phone unavailable.
**Est:** 1.5–2 h with a human nearby. Evidence → `phase09/`.

- 9.1 Scored loopback: `python3 local_dev/physical_loopback.py --case <case-name>
  --media <ABSOLUTE clip> --expect-text ... [--expect-face] [--expect-objects ...]` — one
  run with an audio+face clip, one with an object clip; record scores.
- 9.2 Voice assistant matrix subset: `python3 local_dev/voice_assistant_loop.py
  [--skip-enroll]` — enrollment + owner-accept + stranger-reject rows; evidence is logcat
  `HUSHAI_TX` lines (SurfaceView is black under screencap — do not screenshot-verify).
- 9.3 App drive: `local_dev/run_hushai_app.sh` — build/install/start; adb-navigate Voices /
  People / Plates / Events screens (screenshots where non-black); exercise "This is me"
  owner marking on the phys DB.
- 9.4 Offline store-and-forward: airplane mode ON → capture ~60 s → airplane mode OFF →
  verify deferred segments land on the phys backend (segment count before/after).

---

## Phase 10 — Wrap-up & scorecard

**Purpose:** the campaign's single-page truth. **Est:** 30 min.

- 10.1 Write `REPORT.md`:
  - Header: run id, dates, tree pin (SHA + dirty-file list), environment manifest (macOS,
    Ollama version + model digests, Chrome version, config_hash values observed).
  - Scorecard table: `Phase | Status (PASS/PARTIAL/FAIL/SKIP) | HARD x/y | SOFT score |
    Wall time | Evidence dir | Findings refs`.
  - Findings index from `FINDINGS.md`, severity-sorted, each with repro + evidence link.
  - Carve-outs restated: issue #3 reds, G5 journey advisories (with their observed results).
  - **Human decisions needed**: any suggested re-baseline (`--update-baseline` — never done
    by the runner), any finding worth a GitHub issue (`Issues/` + `gh` flow — human converts).
- 10.2 Campaign verdict: **PASS** = every non-skipped phase PASS or PARTIAL, zero unresolved
  HIGH findings on HARD checks, Phase 6 thresholds met. Anything else = FAIL with the
  blocking findings listed first.
- 10.3 Set `PROGRESS.md` → `CAMPAIGN: COMPLETE`. Leave the world artifacts
  (`local_dev/world/`) in place — they are reusable for the next run (Phase 4 re-injection
  is idempotent via stable seeds against a fresh DB).
