# Gotham — the hushai intelligence layer

**Entity graph · link analysis · pattern baselines · proactive briefings · an agentic tool-calling orchestrator over everything the suite already knows.**

This spec is written BEFORE implementation. Parts 1–2 are the build contract for Wave 1 (pillars G1 + G3-minimal); Part 3 defines the E2E verification capability; the phases in Part 4 are executed after implementation and become the acceptance gate. Later waves (G2, G4, G5) are specified here at interface/outline depth and get sibling build-contract specs when their turn comes (the `v1_spec.md` → `v2_integration_spec.md` precedent in `hushai-advisor/`). Every `file:line` reference in this document was validated against the working tree on **2026-07-07** (branch `docs/advisor-v1-verification-spec`, migration head `0027_advisor_sessions.sql`). This file's pillar checkboxes and result matrix are updated as work lands (the `docs/feature-parity-roadmap.md` convention).

---

## Scope

What "Palantir-Gotham-style" means **here** — a single-owner, fully local, zero-egress home system:

1. **Connect the dots** the suite already perceives but never joins: who talks to whom, who arrives with which vehicle, which voice belongs to which face, who frequents which camera, who moves front-door → garage. Today every one of these is either computed per-question at query time (`hushai-rag/src/retrieve.rs:1195,1270,1322`) or not computable at all (voice↔face is unlinked for everyone except the owner).
2. **Learn what normal looks like** per entity — arrival rhythms, dwell, companions — and surface deviations through the existing alert stack.
3. **Serve it on a silver platter**: a daily briefing the user reads or hears, and an agent that can be *asked anything* and autonomously composes the suite's capabilities — search, counting, graph traversal, media deep-links — into a grounded, cited answer.
4. **Act, with consent**: watchlist and alert-rule changes proposed by the agent, executed only after explicit confirmation.

What it is **not**: no new perception lanes, no new models, no cloud, no external intelligence feeds, no multi-tenant investigation platform. See [Non-goals](#non-goals--referenced-not-duplicated).

## Locked decisions

- **Naming**: "Gotham" is the spec codename. Env prefixes are functional, per house convention (`THREADER_*`, `PROFILES_*`): **`GRAPH_*`** for the data layer, **`GOTHAM_*`** for the agent runtime. The agent's user-facing display name is **"Detective"** (registry entry `gotham`).
- **No new service, no new port.** The agent runtime is a module in `hushai-rag` (rationale in Part 2 §2.2). The graph API lives in `hushai-backend` (schema owner). Existing ports (8080/8090/8070/8095) are untouched.
- **Postgres only.** No Neo4j / graph database. Edges are rows; traversal is bounded recursive CTEs. The local-first single-DB story is a feature.
- **No LLM anywhere in graph materialization.** Edges, baselines, anomalies, and digest *facts* are deterministic integer/ratio math (the `profiles.rs`/`threading.rs` doctrine: compute deterministically, narrate at answer time). This is what makes the layer evaluable with byte-stable baselines.
- **Identity binding is assistive, never authoritative.** A voice↔face candidate is *proposed* to the user; nothing auto-merges. Catalogs are never merged across modalities — the edge IS the identity link.
- **The runtime bet**: rig-core's native tool-calling over Ollama, with a hand-rolled ReAct fallback and a whole-turn fallback to today's auto pipeline. Chat must **never regress** when Gotham misbehaves or is disabled.
- **Time anchoring**: baselines, anomalies, and digests are computable "as of" an explicit capture-anchored time (the threader precedent — windows derive from the batch's capture times, never wall clock). This is a hard constraint, not a preference: eval invariant 3 (fixture-pinned timestamps, `hushai-eval/RECURSIVE_TESTING.md`) collapses without it.
- **AGENTS.md:386 is stale and must be corrected** in the first Gotham PR: "Rig has no tool-calling, so routing is a classification prompt" is false for the shipped dependency graph — see Part 2 §2.1.

---

## §1 Product framing — the raw material already exists

Gotham adds **links, patterns, and agency** on top of catalogs the perception pipeline already maintains. It never adds perception.

| Raw material | Where it lives | Produced by | What's missing (Gotham's job) |
|---|---|---|---|
| Voice identities | `speakers` (192-d TitaNet centroids, `is_owner`) + `speaker_segments` | worker `speaker_match.rs` | No link to faces; relationships implicit |
| Face identities | `persons` (512-d ArcFace centroids, `is_owner`) + `person_segments` | worker `vision/face_match.rs` | No link to voices; co-presence computed per-question |
| Vehicles | `license_plates` (normalized-string identity) + `plate_detections` | worker `vision/plates/plate_match.rs` | No person↔vehicle association |
| Conversations | `conversations` (0025: `speaker_ids uuid[]`, `link_group_id` cross-device) | backend threader, worker-0 driven | Participant pairs never materialized as relationships |
| Typed events | `events` (0014: person/plate/object/speech, severity, `dedup_key` sessionization) | worker `events_producer.rs` | Consumed by profiles only; no cross-entity folding |
| Entity memory | `entity_profiles` (0024: append-only per-entity `profile_text`, visit counts) | backend `profiles.rs`, worker-0 driven | Per-entity only — no *inter*-entity structure |
| Alert stack | `alert_rules` + `alert_deliveries` + `watchlist` (0014–0019) + feed/webhook/push | worker `alerts.rs` + `delivery.rs` | Ready to carry anomaly events for free |
| Answer arms | `retrieve.rs`, `presence.rs`, `stats.rs`, `analytics.rs`, `context.rs` in hushai-rag | — | Reachable only through the classification router, one arm per question — no composition |

The universal join key across every modality is `segment_id`; the sessionized, low-volume derivatives (`events`, closed `conversations`) are the graph's only inputs — never the partitioned per-detection firehose tables.

---

## §2 Pillars

Dependency order: **G1 → {G2, G5}**; **G3** starts in parallel with G1 (existing arms as tools) and integrates graph tools as G1/G2 land; **G4** after G3 + the advisor-v2 integration PRs (it consumes that slash/voice infrastructure).

**Waves**: Wave 1 = G1 + G3-minimal (build contracts in this file, Parts 1–2). Wave 2 = G2 + G3 graph tools. Wave 3 = G4. Wave 4 = G5 (schema ships in Wave 1's migrations; stitching + narration in Wave 4).

### G1 — Entity/link graph + voice↔face binding
- [x] Migration 0028 (`entity_edges`, `graph_state`) — plus 0029/0030 schema shipped early
- [x] `graph.rs` pure core (12 unit tests) + `graph_pass.rs` watermark drain (`co_present`, `conversed_with`, `visits_place`)
- [x] Merge hooks inside speakers/persons/plates merge transactions (+ `delete_entity_in_tx` cascade)
- [x] Worker-0 driver + `GRAPH_*` knobs
- [x] `/v1/graph/*` read API (entity page, edges, neighbors, timeline, path, bindings, rebuild)
- [x] Binding trials + review queue + confirm/reject + owner seed + `arrived_with_vehicle`
- [x] Eval `graph` modality + fixtures F1–F3 + baselines  ← PR 3: harness + F1–F3 CALIBRATED on the rig (Phases 0/A/B/C all pass; each fixture green + gate ×2; media reproducible via `fetch_eval_clips.sh`). **Promoted `staging`→`train`; full-suite re-baselined under config_hash `d4acc862`** (graph metrics byte-deterministic). Two pre-existing LLM chat fixtures (`money_talk`/`repeat_visitor`) fail on brittle keyword assertions under 7B answer drift — unrelated to Wave 1, tracked separately.

> **Status (2026-07-08):** G1 code landed and compiles workspace-wide (backend/worker/viewer);
> `graph::` unit tests green (12/12). PR 3 (eval `graph` modality) built: `GraphGt` +
> `expect_entity`/`expect_edge`/`expect_no_edge` (`fixtures.rs`), `score_graph` gated in
> `score_all` (+6 unit tests), unwindowed edge + name→id resolution query (`query.rs`),
> `poll::wait_graph_folded` fold-quiescence gate, `lib.rs` wiring, `"GRAPH_"` folded into the
> eval config-hash (`manifest.rs`), `eval.env` graph knobs (`GRAPH_INTERVAL_SECS=2`,
> `GRAPH_GRACE_SECS=0`), and F1–F3 fixtures in **staging** (a parse+kind-invariant unit test
> guards their JSON). Every graph read query + the fold-quiescence gate LIVE-VALIDATED against
> the real 0028 schema (scratch schema, dropped): edge shape, assignment-invariant name→id
> membership (F3 positive obs≥2 present, negative single co-sighting below-bar absent), and the
> watermark gate flipping behind→folded. **Verification pass (adversarial multi-agent review)
> found + fixed 3 defects:** (1) `reset_db` didn't clear the graph tables (cross-case edge bleed)
> → guarded `RESET_GRAPH_SQL`; (2) `graph_pass` correlates cross-subject edges batch-locally, so
> the eval's serial inject-quiesce cadence never co-located a person + their vehicle in one pass →
> the graph modality now waits for inputs to settle then triggers ONE authoritative
> `POST /v1/graph/rebuild` (single whole-scenario batch); (3) the rebuild handler used
> `GraphOpts::default()` (grace 90, excludes fresh events) → added `GraphOpts::from_env()`. New
> backend integration test `tests/graph_db.rs` proves rebuild's cross-subject correlation end-to-end
> (green; `hushai_test` now at migration head 0030). NOT yet done: live-stack calibration on a
> running rig (F1–F3 media curation + freeze values twice → promote staging→train; Phases B/C), and
> the documented Wave-1 producer simplifications (batch-local co-presence, binding `person_only`).
> Baselines/anomalies (0029) and journeys (0030) are schema-only until Waves 2/4.
>
> **LIVE E2E on the rig (2026-07-08) — Phases 0/A/B/C ALL PASS.** F1 (`graph_face_voice_bind`,
> binding→`candidate`), F2 (`graph_cross_camera_fusion`, `visits_place`×2), F3
> (`graph_person_vehicle`, `arrived_with_vehicle` obs 2 + below-bar Bob negative) each PASS + gate
> ×2 (baselines frozen under config_hash `d4acc862`). Phase B: `/v1/graph/*` 401 without bearer,
> edges/entity/neighbors shapes, rebuild round-trips to an identical edge set. Media reproducible via
> `fetch_eval_clips.sh` (Judith/Sally PD portraits + Auckland plate + JFK speech; F2 reproduces its
> baseline from script-regenerated media). **The live run caught + fixed 2 more bugs beyond the
> batch-local finding:** (a) `enroll_plate` never upserted the case device → FK violation (eval);
> (b) `graph_pass::upsert_edge` ON CONFLICT dropped `status` → the binding review queue NEVER
> auto-surfaced in shipped G1 (added `status = $13`; guarded by the extended `graph_db.rs`). Plate
> ALPR reads EMD774 as `EM0774` (D→0) — F3 enrolls the OCR norm with display 'EMD774'.

**Shipped means**: relationship questions ("who hangs out with whom", "who frequents the front door", "whose voice is that face") answered from materialized, evidence-carrying edges via a bearer-authed API; merge/delete reconciliation proven; Tier-1 fixtures green twice back-to-back.

### G2 — Baselines, anomalies, daily briefing
- [x] Migration 0029 (`entity_baselines`, `daily_digests`) — shipped in Wave 1
- [x] `patterns.rs` — baseline recompute + anomaly predicates ✅ (PR4); **daily-digest producer ✅ (PR5)**
- [x] Anomalies emitted as ordinary `events` rows (`event_type='pattern_anomaly'`) → ride the A1–A7 stack with zero new alert plumbing. **ALL FOUR predicates now wired.** **AS-OF `off_schedule_presence`** (each visit judged vs the subject's strictly-earlier visits — the incremental model), plus the three EDGE predicates — **`first_time_pairing`** (a `co_present` edge 0→1 between two mature regulars, emitted per-endpoint so assertions stay assignment-invariant), **`new_vehicle_for_person`** (an `arrived_with_vehicle` 0→1 for a person with a prior different-plate edge), **`unknown_person_cluster`** (≥ `GRAPH_ANOMALY_UNKNOWN_CLUSTER_MIN` distinct unknown persons co-present in one device window — DEVICE-keyed, no catalog subject). The edge predicates key off the 0→1 transitions collected at drain time (`graph_pass::EdgeTransitions`) and are judged in `patterns::flag_edge_anomalies` AFTER the baseline recompute so endpoint maturity is available (for the rebuild AND the incremental worker). Baselines recomputed per touched subject in the graph pass; the worker alert-evaluates fresh anomalies post-commit (the evaluator is worker-crate). **No new `GraphCfg`/hashed knob** (reuses `anomaly_min_visits` / `anomaly_unknown_cluster_min`; "established other vehicle" = any prior different-plate edge that predates), so `config_hash` stays `d4acc862` and F1–F7 baselines are untouched. Deterministic wiring proof: `hushai-backend/tests/graph_db.rs`; E2E-through-perception fixture `anomaly_first_pairing` (staging).
- [x] Digest producer + endpoints + fixtures F4–F7 — **ALL CALIBRATED live (gate ×2, frozen `d4acc862`)**. PR4: F4 `graph_baseline_rhythm` (baseline visits≥5 peak-hour 09, no anomaly), F5 `anomaly_novel_time` (off_schedule fires), F6 `anomaly_negatives` (sealed holdout, no over-fire). **PR5: `patterns::build_and_upsert_digest` (deterministic `sections` + template `rendered_text`, NO LLM), `POST /v1/graph/digests/{date}` force-generate + worker-0 wall-clock driver (`GRAPH_DIGEST_HOUR_LOCAL`, non-hashed), F7 `briefing_daily` (pinned-date structured `sections` — 8/8 assertions gate ×2).**

**Shipped means**: an off-schedule visit fires an alert rule end-to-end; the briefing endpoint returns byte-stable structured facts for a *pinned* date; sealed anomaly-negative fixture green.

### G3 — Agentic tool-calling runtime ("Detective")
- [ ] `hushai-rag/src/gotham/` module: rig ToolSet runtime + ReAct fallback + startup tools-probe
- [ ] Phase-1 tool registry (read-only, ~15 tools) + SSE superset (`phase`/`tool_call`/`tool_result`/`confirm`)
- [ ] Migration 0031 (`chat_messages.tool_trace`, `gotham_pending_actions`)
- [ ] Whole-turn fallback to the auto pipeline; audit writes; `AGENTS.md:386` correction
- [ ] Eval `agent` modality + fixtures F8–F11 (staging)
- [ ] Phase 2: graph tools (probe-gated), media links, voice keyword
- [ ] Phase 3: mutating tools + two-phase confirmation, auto-router promotion, proactive briefing turn

**Shipped means**: a multi-hop question ("did the person who drives EMD774 ever talk to Bob?") answered over live SSE with an observable tool trace and citations; runaway loop stopped at the cap; staging fixtures pass twice back-to-back; existing chat byte-identical when `GOTHAM_ENABLED=false`.

### G4 — Investigation UI + voice
- [ ] Viewer: slash-picker row `gotham` (advisor-v2 registry seam), tool-step rendering, confirm bubbles
- [ ] Entity/investigation panes: entity page, link explorer, evidence chips deep-linking into the HLS timeline
- [ ] Binding review queue UI (confirm/reject, sample-audio + sample-face side by side)
- [ ] Android: voice keyword route (`AssistantRouting.kt` seam), `AWAIT_FOLLOWUP` confirmations, SSE unknown-event tolerance verified
- [ ] Headless-Chrome e2e + phone-rig markers

**Hard dependency**: advisor-v2 PRs (slash picker, `AWAIT_FOLLOWUP`) land first — Gotham appends registry rows and reuses the phase machinery, never rebuilds it (`hushai-advisor/v2_integration_spec.md`).

### G5 — Cross-camera journeys
- [ ] Migration 0030 (`entity_journeys`, `camera_adjacency` view)
- [ ] Journey stitching in the events drain (open/closed contract from `conversations`)
- [ ] `/v1/graph/journeys` + `/v1/graph/path`; journey tool for G3; timeline strip for G4
- [ ] Fixture F2 journey assertions promoted to gating

**Shipped means**: "walk me through what Casey did yesterday" yields an ordered camera-hop narrative ("front door 09:02 → garage 09:07") with deep links; `expect_journey` green.

---

## §3 Privacy & security posture

This system records the user's own home. Gotham makes the recorded data *more* legible — that raises sensitivity, and the posture must be explicit:

- **Everything local.** Every tool target, model call, and materialization pass is loopback (Postgres, Ollama, backend, viewer). No egress, ever. The runtime refuses to register a tool whose resolved endpoint is non-loopback without an explicit override (the `RAG_ALLOW_INSECURE` philosophy, `hushai-rag/src/lib.rs`).
- **Person linking is opt-in per link.** `same_identity_candidate` edges surface in a review queue; only a user-confirmed edge may be used to union voice/face history. Rejection is sticky. Nothing auto-merges (§1.4 of Part 1).
- **Deletion inheritance.** Graph rows are DERIVED DATA (0024 precedent): deleting a person/speaker/plate must delete or orphan-tombstone its edges, baselines, and journeys in the same transaction as the catalog delete (REVIEW.md deletion/GC surface). A rebuild from surviving sources must never resurrect a deleted entity's links.
- **Audit.** Binding confirm/reject and every mutating agent action write `audit_log` (0017, append-only). Agent *read* tool calls are audited too (`GOTHAM_AUDIT_READS=true` default) — investigations of one's own data are still actions worth a trail. Graph read API calls go through the viewer gateway audit like every other `/v1/*` admin read.
- **Token model.** Unchanged planes: browser → viewer (session cookie + IP allowlist) → backend/rag with server-side bearer injection (`hushai-viewer/src/proxy.rs:141,180`). Gotham's outbound backend calls use a dedicated `GOTHAM_BACKEND_TOKEN` (loopback default) that the browser never sees. No new fail-closed surface because there is no new service.
- **Prompt injection is a first-class threat.** Recorded strangers' speech becomes agent input via transcripts. Mitigations are structural, not vibes: Phase 1 is read-only; mutations always require a human confirmation turn whose summary is composed by deterministic code from parsed args (never by the model); the preamble marks all tool output as data-not-instructions (Part 2 §2.5).

---

## §4 Architecture overview

```
                        ┌──────────────────────────────────────────────────────┐
 perception (existing)  │  DATA LAYER (G1/G2/G5)                               │
 worker audio lane ──►  │  hushai-backend: graph.rs (pure core)                │
 worker vision lane ──► │    graph_pass.rs (watermark drain over settled       │
   └─ events_producer   │      events + closed conversations, worker-0 driven) │
 backend threader ────► │    patterns.rs (baselines · anomalies · digests)     │
                        │  tables: entity_edges · graph_state ·                │
                        │    entity_baselines · daily_digests ·                │
                        │    entity_journeys      (migrations 0028/0029/0030)  │
                        │  anomalies → events rows → A1–A7 alert stack (free)  │
                        └───────────────┬──────────────────────────────────────┘
                                        │  /v1/graph/* (backend, bearer-authed)
                        ┌───────────────▼──────────────────────────────────────┐
                        │  RUNTIME LAYER (G3)                                  │
                        │  hushai-rag/src/gotham/: rig ToolSet loop            │
                        │    (multi_turn + PromptHook) │ ReAct fallback        │
                        │  tools = existing arms + graph API + admin API       │
                        │  trace → chat_messages.tool_trace + audit_log        │
                        │  fallback → today's auto pipeline (same SSE stream)  │
                        └───────────────┬──────────────────────────────────────┘
                                        │  /v1/rag/chat SSE superset
                        ┌───────────────▼──────────────────────────────────────┐
                        │  SURFACE LAYER (G4)                                  │
                        │  viewer chat (slash row · tool steps · confirm) ·    │
                        │  investigation panes · binding queue │ Android voice │
                        │  ("detective" keyword · AWAIT_FOLLOWUP confirms) │   │
                        │  proactive: daily briefing → events feed + chat      │
                        └──────────────────────────────────────────────────────┘
```

Boundary contracts:
- **entity_profiles (0024)** stays exactly as-is: profiles = per-entity narrative log; graph = inter-entity edges. The boundary is locked; the graph never writes `profile_text`, profiles never write edges.
- **events/alerts (Pillar A, shipped)**: Gotham *emits into* it (`pattern_anomaly`, `gotham_briefing` event rows) and *reads from* it. It never re-implements rules, cooldowns, or delivery.
- **advisor v2 spec**: the slash-picker registry, `AWAIT_FOLLOWUP` phase, and pane seams are consumed as-built. Gotham's G4 items are additive registry rows.
- **camera contract** (`contracts/cameraToBackendContract.md`): untouched. The graph is downstream-only; if cameras ever emit location hints they land in the reserved proto fields 18–40 — nothing here needs them.

---

# Part 1 — Data-layer build contract (G1, plus G2/G5 schema)

## 1.1 Design stance

1. **Explicit edge table, not views.** Query-time relationship functions (`retrieve.rs:1195,1270,1322`) are O(scan) per question, single-hop, and can't accumulate evidence, support "how are X and Y connected", or survive merges with provenance. A materialized `entity_edges` table is the only shape supporting multi-hop CTE traversal, confidence accumulation, merge folding, and eval baselines. The existing query-time functions remain as the always-fresh fallback for the not-yet-drained tail (the 0025 "consumers degrade, never error" contract).
2. **Sessionized inputs only.** The graph drains `events` (0014) and **closed** `conversations` (0025) — never `person_segments`/`speaker_segments`/`scene_objects`/`plate_detections`. Events/conversations are plain, low-row-count, carry `updated_at` watermarks, and are already the folding source for `profiles.rs`. Every graph pass is one indexed watermark scan.
3. **Deterministic, no LLM.** All scoring is integer/ratio math, confidences rounded to 4 decimals before storage, total tie-breaks on every sort, `BTreeMap` iteration (the `profiles.rs` idioms), "now" taken once from the DB clock.
4. **Nodes are the existing catalogs.** No node table — `persons`, `speakers`, `license_plates`, `devices`, `conversations` ARE the nodes. Edge endpoints are `(node_type, node_id)` pairs with **no FK** (the `events.subject_id`/0024 contract: merges may orphan; merge hooks fold; a stale pointer never blocks a write). Endpoint id type is `text` (canonical lowercase hyphenated uuid string, or `device_id` verbatim) because devices are text-keyed — and devices belong in the graph: path queries route through places ("X and Y both frequent the front door").

## 1.2 Migration `0028_entity_graph.sql`

```sql
CREATE TABLE entity_edges (
    edge_id           uuid PRIMARY KEY,          -- Uuid::now_v7 (house standard)
    edge_type         text NOT NULL CHECK (edge_type IN
        ('co_present',              -- person|speaker <-> person|speaker: overlapping visits, same device
         'conversed_with',          -- speaker <-> speaker: pairs from closed conversations.speaker_ids
         'arrived_with_vehicle',    -- person -> plate: temporal correlation on one device
         'same_identity_candidate', -- speaker <-> person: the voice<->face binding (§1.4)
         'visits_place')),          -- person|speaker|plate -> device
    src_type          text NOT NULL CHECK (src_type IN ('person','speaker','plate','device')),
    src_id            text NOT NULL,
    dst_type          text NOT NULL CHECK (dst_type IN ('person','speaker','plate','device')),
    dst_id            text NOT NULL,
    observation_count bigint NOT NULL DEFAULT 0,
    first_seen_unix_nanos bigint,
    last_seen_unix_nanos  bigint,
    confidence        real,                      -- edge-type-specific; rounded to 4 decimals
    -- Newest-N provenance samples, capped at GRAPH_EDGE_SAMPLE_CAP:
    --   [{"event_id":..,"segment_id":..,"t":<unix_nanos>}, ...]
    -- plus per-type counters (binding: {"together":N,"speaker_only":N,"person_only":N})
    evidence          jsonb NOT NULL DEFAULT '[]'::jsonb,
    metadata          jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- Binding review-queue state machine; NULL for every other edge type:
    status            text CHECK (status IN ('candidate','confirmed','rejected')),
    config_hash       text,                      -- GRAPH_* fingerprint last touching this row
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    CHECK (status IS NULL OR edge_type = 'same_identity_candidate')
);

-- Idempotent upsert key (the events.dedup_key idiom, structural):
CREATE UNIQUE INDEX entity_edges_identity_idx
    ON entity_edges (edge_type, src_type, src_id, dst_type, dst_id);
CREATE INDEX entity_edges_src_idx  ON entity_edges (src_type, src_id, edge_type);
CREATE INDEX entity_edges_dst_idx  ON entity_edges (dst_type, dst_id, edge_type);
CREATE INDEX entity_edges_type_seen_idx ON entity_edges (edge_type, last_seen_unix_nanos DESC);
CREATE INDEX entity_edges_binding_queue_idx ON entity_edges (updated_at DESC)
    WHERE edge_type = 'same_identity_candidate' AND status = 'candidate';

CREATE TABLE graph_state (
    id                      smallint PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    events_watermark        timestamptz NOT NULL DEFAULT to_timestamp(0),
    conversations_watermark timestamptz NOT NULL DEFAULT to_timestamp(0),
    config_hash             text,
    updated_at              timestamptz NOT NULL DEFAULT now()
);
INSERT INTO graph_state (id) VALUES (1);
```

Both tables carry the DERIVED-DATA header comment (0024 precedent): rebuildable from `events` + `conversations` + catalogs; deleting forgets structure, never identity. Watermarks are wall-clock (`updated_at`), not capture time — survives late reprocessing of old-capture backlogs (0024/0025 reasoning).

**Direction convention**: undirected types (`co_present`, `conversed_with`, `same_identity_candidate`) store the lexicographically smaller `(node_type, node_id)` endpoint as `src` — producer-enforced by a unit-tested pure function, making the unique index the dedup key with no mirror-row problem. Directed types keep natural direction (`arrived_with_vehicle` person→plate; `visits_place` entity→device).

## 1.3 Migrations `0029_entity_baselines_digests.sql` and `0030_entity_journeys.sql` (G2/G5 schema, shipped early so numbering is settled)

```sql
-- 0029
CREATE TABLE entity_baselines (
    subject_type      text NOT NULL CHECK (subject_type IN ('person','speaker','plate')),
    subject_id        uuid NOT NULL,
    window_days       integer NOT NULL,
    -- 168 hour-of-week buckets, local civil time via the existing fixed-offset convention
    hour_histogram    integer[] NOT NULL DEFAULT '{}',
    visits_in_window  integer NOT NULL DEFAULT 0,
    dwell_p50_secs    integer,
    dwell_p90_secs    integer,
    device_stats      jsonb NOT NULL DEFAULT '{}'::jsonb,  -- {"<device_id>":{"visits":N,"last_seen_ns":..}}
    companion_stats   jsonb NOT NULL DEFAULT '[]'::jsonb,  -- top-K, deterministic order
    config_hash       text,
    computed_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (subject_type, subject_id)
);

CREATE TABLE daily_digests (
    digest_date    date PRIMARY KEY,            -- local civil date under tz_offset
    tz_offset_secs integer NOT NULL,
    sections       jsonb NOT NULL,              -- structured, deterministic facts (see §1.6)
    rendered_text  text NOT NULL,               -- deterministic template render; NO LLM
    config_hash    text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);

-- 0030
CREATE TABLE entity_journeys (
    journey_id            uuid PRIMARY KEY,
    subject_type          text NOT NULL CHECK (subject_type IN ('person','plate')), -- v1: vision lanes only
    subject_id            uuid NOT NULL,
    started_at_unix_nanos bigint NOT NULL,
    ended_at_unix_nanos   bigint NOT NULL,
    hop_count             integer NOT NULL,
    hops                  jsonb NOT NULL,   -- [{"device_id":..,"arrive_ns":..,"depart_ns":..,"event_id":..}]
    status                text NOT NULL DEFAULT 'open' CHECK (status IN ('open','closed')),
    dedup_key             text,             -- "journey:<subject_id>:<first_hop_bucket>"
    config_hash           text,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX entity_journeys_dedup_idx ON entity_journeys (dedup_key) WHERE dedup_key IS NOT NULL;
CREATE INDEX entity_journeys_subject_time_idx ON entity_journeys (subject_type, subject_id, started_at_unix_nanos DESC);
CREATE INDEX entity_journeys_open_idx ON entity_journeys (status) WHERE status = 'open';
```

Journey rules (Wave 4 implements; contract locked now): a subject's visit on device B starting within `GRAPH_JOURNEY_GAP_SECS` of its visit end on device A extends the open journey (the 0025 `link_group_id` philosophy: LINK across devices, never merge underlying data). **Only journeys spanning ≥ 2 distinct devices persist** — single-device visits are already events/profiles territory. Open/closed mutability contract copied from `conversations`. `camera_adjacency` is a plain VIEW over hop pairs (the C4 floor-plan seed — no table until C4 needs one). Speakers excluded in v1 (cross-device co-hearing is already `link_group_id`; voice journeys are noise until binding ships).

Baselines get their own table — NOT an `entity_profiles` extension: profiles are append-only narrative; baselines are structured and *recomputed* over a trailing window.

## 1.4 The voice↔face binding (`same_identity_candidate`)

The gap: `speakers` and `persons` are separate catalogs linked only implicitly for the owner (`is_owner` on both, 0023). For everyone else, "what did the guy in the red jacket say" is unanswerable.

**Candidate generation — deterministic trials.** Every **closed** conversation is a trial. For conversation C on device D spanning [t0, t1] with participants `speaker_ids`:
- persons present = subjects of `known_person`/`unknown_person` events on D overlapping [t0, t1] (± `GRAPH_COPRESENCE_SLACK_SECS`);
- each (speaker, person) pair present together increments the pair's `together` counter;
- a speaker in C with **no** person present increments `speaker_only` on that speaker's existing candidate edges; a person visit on D overlapping speech with the speaker absent increments `person_only`.

**Score**: session-level Jaccard, `confidence = together / (together + speaker_only + person_only)`, rounded to 4 decimals. Counters persist in `evidence` jsonb — the score is recomputable and auditable.

**Surfacing rule — never auto-merge.** An edge enters the review queue (`status='candidate'`) only when ALL hold: `together ≥ GRAPH_BIND_MIN_SESSIONS` AND `confidence ≥ GRAPH_BIND_MIN_CONFIDENCE` AND margin over the speaker's runner-up person candidate ≥ `GRAPH_BIND_MARGIN` (defeats the always-together-couple confound). The user confirms/rejects via API (audit-logged, mirrors the merge UX). `confirmed` = consumers may union voice/face history across the edge; `rejected` = sticky negative (evidence keeps accumulating; status never auto-flips back). Catalogs are never merged — 192-d and 512-d spaces don't mix; **the edge is the identity**.

**Owner seed**: an idempotent pass step creates a `confirmed` edge between the `is_owner` speaker and `is_owner` person when both exist, and retires it if ownership moves.

**Consumer helper**: `graph_pass::bound_person_for_speaker()` exported for hushai-rag to union histories across `confirmed` edges only.

## 1.5 Materialization — `graph.rs` + `graph_pass.rs` + worker-0 driver

File placement (mirrors the `threading.rs` / `conversations.rs` split):

| File | Role | Pattern source |
|---|---|---|
| `hushai-backend/src/graph.rs` | Pure deterministic cores, no DB/clock/RNG: pair generation from visit lists, canonical edge ordering, binding scoring, anomaly predicates, journey stitching, `GraphCfg` + `config_hash()` (SHA-256 of canonical knob string, 16 hex chars) | `threading.rs:90` `config_hash`; reuses `profiles::coalesce_events_to_visits` (`profiles.rs:503`, already `pub`) |
| `hushai-backend/src/graph_pass.rs` | DB orchestrator: `graph_pass(pool, &opts) -> GraphStats`, advisory-locked watermark drains, edge upserts, `merge_in_tx`, `seed_owner_binding`, `rebuild` | `profiles.rs:104` `update_all`, `profiles.rs:164` `merge_in_tx` |
| `hushai-backend/src/graph_api.rs` | axum handlers for §1.7, mounted in `routes.rs` | existing admin modules |
| `hushai-backend/src/patterns.rs` | Baselines recompute, anomaly emission, digest render (Wave 2) | same pass |

Advisory lock: `GRAPH_LOCK_KEY: i64 = 0x6867_7270_68` ("hgrph") — distinct from `PROFILE_LOCK_KEY` (`profiles.rs:31`, 0x6870_726f_66). Graph folding never mutates catalogs → no deadlock-ordering interaction with the identity locks.

**Driver**: worker 0, drain-time, interval-gated — the exact block pattern of the profiles driver at `hushai-worker/src/lib.rs:927-956` (`worker_id == 0 && cfg.graph_enabled && last_graph.elapsed() >= cfg.graph_interval_secs` → `hushai_backend::graph_pass::graph_pass(&pool, &opts)`). New knobs in `hushai-worker/src/config.rs`. Metrics via the shared `observe.rs` counters (`hushai_graph_edges_upserted_total`, `hushai_graph_pass_seconds`); **no entity names as labels** (cardinality guard).

**Pass algorithm** (single transaction, bounded):
1. `pg_advisory_xact_lock(GRAPH_LOCK_KEY)`; load `graph_state`; compare `config_hash` — on mismatch: warn + metric, keep accumulating (rebuild is explicit only: `GRAPH_REBUILD_ON_START` / admin endpoint — the `THREADER_BACKFILL_ON_START` idiom).
2. **Events drain**: settled events (`updated_at > events_watermark`, older than `GRAPH_GRACE_SECS`, in-progress guard — the `profiles.rs` semantics verbatim), `ORDER BY updated_at LIMIT GRAPH_MAX_EVENTS_PER_PASS`. Coalesce per subject into visits. Then:
   - `visits_place`: upsert per (subject, device) — count/first/last/evidence sample.
   - `co_present`: per visit, find overlapping neighbors **by re-querying `events` on (device_id, time)** — NOT batch-local (a batch-split edge miss would be permanent; events are sessionized so the re-query is cheap). Pairwise upsert, capped at `GRAPH_COPRESENCE_MAX_SUBJECTS` per window (N² guard; the cap is determinism-relevant → hashed).
   - `arrived_with_vehicle`: plate events on the same device within `GRAPH_VEHICLE_CORR_WINDOW_SECS` of a person visit start → person→plate upsert.
   - Wave 2: anomaly predicates vs `entity_baselines` → `INSERT INTO events` (§1.6). Wave 4: journey stitching.
3. **Conversations drain**: closed conversations with `updated_at > conversations_watermark` → `conversed_with` per speaker pair in `speaker_ids`; binding trials per §1.4.
4. Recompute `entity_baselines` for subjects touched this pass (bounded set; full deterministic recompute per subject over the trailing window, anchored to the batch's capture times — the locked time-anchoring constraint).
5. Advance both watermarks + stamp `config_hash`; commit.

**Merge & delete hooks**: `graph_pass::merge_in_tx(tx, node_type, loser, survivor)` called inside the existing merge transactions in `speakers.rs`/`persons.rs`/`plates.rs`, beside the existing `profiles::merge_in_tx` call: repoint loser edges to survivor; fold duplicates (`observation_count` sum, `first_seen` LEAST, `last_seen` GREATEST, evidence merged newest-N, binding counters summed, confidence recomputed); drop self-edges. Delete paths remove the entity's edges/baselines/journeys in the same tx (§3 posture). Documented accepted loss (same as profiles): loser events not yet drained at merge time stay orphaned. No rename hook — edges carry no denormalized labels; the API joins catalogs at read time.

## 1.6 Anomalies + daily digest (G2 contract)

**Anomalies are ordinary `events` rows** — `event_type='pattern_anomaly'`, `severity='warning'`, `metadata.kind`, idempotent `dedup_key = "anom:<kind>:<subject>:<bucket>"`. The 0014 event-type vocabulary is code-validated free text — no migration needed — and the entire A-pillar (rules matching, cooldowns, feed, webhook HMAC, Android push) carries them for free. Predicates (deterministic, gated on baseline maturity `visits_in_window ≥ GRAPH_ANOMALY_MIN_VISITS`):

| kind | Fires when | Wave-2 status |
|---|---|---|
| `first_time_pairing` | `co_present` edge transitions 0→1 observations and BOTH entities are established regulars (mature baselines) | ✅ wired (`flag_edge_anomalies`, emitted per-endpoint; keyed by each subject) |
| `off_schedule_presence` | visit lands in an hour-of-week bucket holding < `GRAPH_ANOMALY_HOUR_MIN_FRAC` of the subject's histogram mass | ✅ wired (`recompute_and_flag`, AS-OF) |
| `unknown_person_cluster` | ≥ `GRAPH_ANOMALY_UNKNOWN_CLUSTER_MIN` distinct unknown-person subjects co-present in one window on one device | ✅ wired (`flag_edge_anomalies`, DEVICE-keyed — no catalog subject) |
| `new_vehicle_for_person` | new `arrived_with_vehicle` edge for a person with an established different vehicle edge (Wave-2: any prior different-plate edge that predates — no count-threshold knob) | ✅ wired (`flag_edge_anomalies`, keyed by the person) |

**Daily digest**: produced by `patterns.rs` in the worker-0 driver once wall clock passes `GRAPH_DIGEST_HOUR_LOCAL` and no row exists for yesterday's local date (idempotent by PK). `sections` jsonb: `{"new_entities":[...], "anomalies":[...], "top_visitors":[...], "conversations":{"count":N,..}, "first_time_pairings":[...], "journeys":[...]}`. `rendered_text` is a deterministic template render. The LLM narrates at read time (RAG/G3) — never at write time. The digest endpoint takes an explicit `date` parameter (bare form defaults to today for humans; **fixtures always pass the date** — eval invariant 3).

## 1.7 Query surface — backend `/v1/graph/*`

Backend serves structure (schema owner, same bearer plane as the rest of `/v1/*`; reached through the viewer gateway with server-side token injection + audit). hushai-rag reads the tables directly in-process for chat enrichment (its established `retrieve.rs` pattern — no HTTP hop).

```
GET  /v1/graph/entities/{type}/{id}            entity page: catalog row + edges grouped by type
                                               (evidence samples, counts, confidence) + baseline +
                                               latest journeys + profile_text (0024)
GET  /v1/graph/entities/{type}/{id}/timeline   merged events + conversations timeline, paginated
GET  /v1/graph/edges?type=&node=&min_confidence=&since=&limit=
GET  /v1/graph/neighbors/{type}/{id}?hops=2&min_confidence=&edge_types=
                                               bounded recursive CTE (hops ≤ 3 hard cap)
GET  /v1/graph/path?from={type}:{id}&to={type}:{id}&max_hops=4
                                               shortest evidence path; may route through devices
GET  /v1/graph/bindings?status=candidate       voice<->face review queue
POST /v1/graph/bindings/{edge_id}/confirm      audit-logged
POST /v1/graph/bindings/{edge_id}/reject       sticky negative
GET  /v1/graph/journeys?subject={type}:{id}&since=&limit=          (Wave 4)
GET  /v1/graph/digests?limit=  ·  GET /v1/graph/digests/{date}     (Wave 2)
POST /v1/graph/rebuild                         admin, audit-logged: truncate derived rows,
                                               reset graph_state, refold
```

Anomalies need no endpoint — `/v1/events?type=pattern_anomaly` already works. Every GET maps 1:1 to a G3 tool. Viewer proxy: add the `/v1/graph/` prefix to `is_backend_path` (`hushai-viewer/src/proxy.rs:141`) — a REVIEW.md must-follow.

## 1.8 Part-1 touch points

| # | File | Change |
|---|---|---|
| 1 | `hushai-backend/migrations/0028_entity_graph.sql` (+0029, 0030) | new — §1.2/§1.3; update `migrations/README.md` in the same change |
| 2 | `hushai-backend/src/graph.rs` | new — pure cores + `GraphCfg`/`config_hash` |
| 3 | `hushai-backend/src/graph_pass.rs` | new — pass, merge/delete hooks, owner seed, rebuild |
| 4 | `hushai-backend/src/graph_api.rs` + `routes.rs` | new module + route mounts |
| 5 | `hushai-backend/src/patterns.rs` | new (Wave 2) — baselines/anomalies/digests |
| 6 | `hushai-backend/src/speakers.rs`, `persons.rs`, `plates.rs` | call `graph_pass::merge_in_tx` beside `profiles::merge_in_tx`; delete paths cascade graph rows |
| 7 | `hushai-worker/src/lib.rs:927-956` region | sibling worker-0 interval block for the graph pass |
| 8 | `hushai-worker/src/config.rs` | `GRAPH_*` knobs |
| 9 | `hushai-viewer/src/proxy.rs:141` | `/v1/graph/` in `is_backend_path` |
| 10 | `hushai-backend/src/observe.rs` call sites | graph pass counters/histograms |

# Part 2 — Agentic runtime build contract (G3, "Detective")

## 2.1 Corrected premise: the tool-calling machinery already ships in our dependency tree

`AGENTS.md:386` ("Rig has no tool-calling, so routing is a classification prompt") is **false for the versions this workspace resolves**. Verified against `Cargo.lock` and the cargo registry sources on 2026-07-07:

- `rig = "0.37"` (hushai-rag/Cargo.toml, hushai-advisor/Cargo.toml) is a **facade crate**: `Cargo.lock:6601` pins `rig 0.37.1`, which depends on **`rig-core 0.38.2`** (`Cargo.lock:6637`).
- rig-core 0.38.2 has the full tool stack: `Tool` trait (`src/tool/mod.rs` — `const NAME`, typed `Args: Deserialize`/`Output: Serialize`, `definition() -> ToolDefinition` with JSON-schema params, `call()`), `ToolSet`/`ToolDyn` registries, agent builder `.tool(...)` (`agent/builder.rs:337,574`) and `.default_max_turns(n)` (`agent/builder.rs:180`), streaming prompt `.multi_turn(turns)` (`agent/prompt_request/streaming.rs:690`) — a bounded plan→act→observe loop that threads tool results automatically and yields `MaxTurnsError` at the cap (`streaming.rs:1683`) — plus `PromptHook` with `on_tool_call` returning `ToolCallHookAction::Skip{reason}` where the reason is fed back to the LLM as the tool result (`agent/prompt_request/hooks.rs:117`) — a built-in veto/gating point — and an in-loop invalid-call policy `InvalidToolCallHookAction` (`hooks.rs:37`) covering the "unparseable → feedback → retry" behavior the advisor philosophy demands.
- **The Ollama provider wires it**: `req.tools` → Ollama's native `tools` JSON (`providers/ollama.rs:531`), `tool_calls` parsed from streaming and non-streaming responses, results serialized as `role:"tool"` messages. Only `tool_choice` is unsupported (warn+ignore) — not needed here.
- Model side: Ollama's `qwen2.5` template is function-calling enabled. A model whose template lacks tools makes Ollama return HTTP 400 ("does not support tools") — a clean, detectable failure.

**Deliverables from this finding** (first G3 PR): correct `AGENTS.md:386`; **pin `rig` to an exact version** (the facade floats rig-core minors — a lockfile refresh would silently change the agent loop's semantics).

### Runtime decision

- **Primary — `GOTHAM_RUNTIME=rig`**: rig ToolSet + `multi_turn` + `PromptHook`. The loop, message threading, streaming, invalid-call retry, and the mutation-gate hook exist and are wired to Ollama; hand-rolling buys nothing.
- **Fallback — `GOTHAM_RUNTIME=react`**: hand-rolled strict-JSON loop behind the same tool registry. Contract: exactly one JSON object per turn — `{"thought":"..","action":{"tool":"..","args":{..}}}` or `{"thought":"..","final":".."}`. Forgiving parser (extract first `{...}`; unparseable → one retry with feedback; still unparseable → non-empty raw text becomes the final answer, else whole-turn fallback). Same bounds/knobs. This is the escape hatch for a future model whose Ollama template lacks tools.
- **Startup probe**: on boot and on model change, fire one tiny tools-enabled `/api/chat` request; if Ollama rejects tools for `GOTHAM_LLM_MODEL`, log loudly and auto-select `react`. Raw-HTTP as a *runtime* is rejected — rig's provider already speaks that wire format.

**Model tiering**: `GOTHAM_LLM_MODEL=qwen2.5:7b` default (fleet parity; the Ollama tag is the instruct variant), `qwen2.5:14b` documented as the recommended upgrade where RAM allows (`docs/hardware-sizing-30-cameras.md` framing). Optional `GOTHAM_JUDGE_MODEL` for a Phase-2 grounding-critique pass (the `ADVISOR_JUDGE_MODEL` precedent). **`GOTHAM_NUM_CTX=16384`** — the advisor proved Ollama's silent truncation at the 4096 default (`hushai-advisor/src/config.rs:32-38`); tool schemas + observations are hungrier than book chapters. Ride it through `additional_params` exactly like `hushai-advisor/src/llm.rs:39-64` and the seed idiom in `hushai-rag/src/llm.rs:60-66`.

## 2.2 Placement: `hushai-rag/src/gotham/` (module, not a crate)

1. **The never-regress fallback is only clean in-process.** Requirement: a failed Gotham turn finishes through the existing auto-router pipeline — same request, same session, same SSE stream. That's a function call inside `chat.rs` (`rag_chat` at `chat.rs:95`); across a service boundary it becomes a second HTTP hop with duplicated session state.
2. **Every arm already lives here as a callable function** — `retrieve.rs` (nearest/conversations/co-presence/events), `presence.rs`, `stats.rs`, `analytics.rs:203` `compute_digest`, `context.rs:199` `enrich_sources_with_vision`, `timeparse.rs`, `humanize.rs` — plus the loaded engines a separate service would re-load (mxbai embedder, CLIP text tower, pool).
3. **Chat-surface reuse**: sessions (`chat_sessions.agent_id="gotham"`), `insert_message` (`chat.rs:1612`), SSE plumbing, `CallerContext`/voice suffix, F4 condenser, and eval's `expect_routed_agent` — `routed_agent_id` is already streamed + persisted (`chat.rs:1255,1267`).
4. Risk isolation is achieved by other means: `GOTHAM_ENABLED` kill switch; the agent reachable only by explicit selection in Wave 1; hard bounds on every loop; separate model/temp/num_ctx knobs so existing paths are **byte-identical when Gotham is off**; the whole-turn fallback.
5. The advisor is a separate crate because its *domain* is disjoint (book corpus, own schema). Gotham's domain is exactly rag's domain; a `hushai-gotham` crate would immediately path-dep hushai-rag for 90% of its body.

```
hushai-rag/src/gotham/
  mod.rs        public entry: run_turn(st, ctx) -> impl Stream<GothamEvent>; enabled() checks
  runtime.rs    rig agent build (tools + hook), multi_turn driving, wall-clock watchdog;
                react runtime behind the same trait
  tools/mod.rs  ToolRegistry: ToolSpec {name, ui_label, side_effect, schema, exec}
  tools/*.rs    transcripts.rs · presence.rs · people.rs · vision.rs · events.rs ·
                graph.rs · admin.rs (backend HTTP) · media.rs · meta.rs
  confirm.rs    pending-action lifecycle (two-phase confirmation)
  trace.rs      tool-trace accumulation + persistence + audit writes
  preamble.rs   persona + tool-use rules
```

Backend-owned mutations (watchlist, alert-rules) go through **backend HTTP** (`/v1/watchlist`, `/v1/alert-rules`), never direct SQL — the watchlist handler owns the managed-rule coupling (migration 0019). Knobs: `GOTHAM_BACKEND_BASE_URL` (default `http://127.0.0.1:8080`) + `GOTHAM_BACKEND_TOKEN`. Audit written via the shared `hushai_backend::audit` helper (backend is already a path dep) — the "backend-side hooks" path 0017's header defers.

## 2.3 Tool inventory

Cross-cutting conventions:
- **Time args**: never raw nanos. `{"window":"today"|"yesterday"|"last_7d"|"last_30d"}` or `{"after":"<ISO local>","before":"<ISO local>"}`, resolved via `timeparse.rs` + caller `tz_offset_secs` (preserves the "model never sees raw machine values" invariant).
- **Results**: structured JSON rendered to a compact observation with pre-humanized labels; truncated to `GOTHAM_TOOL_RESULT_MAX_CHARS` with an explicit `"(+N more — narrow the window)"` tail so the model knows truncation happened.
- **Citations**: evidence-yielding tools register `retrieve::Source` rows in the turn's source accumulator (deduped by segment_id, globally `[n]`-numbered); the final answer cites exactly like today's chat.
- **`side_effect: read | mutate`**; mutate is confirmation-gated (§2.5).

| # | Tool | Args | Side-effect | Backing |
|---|---|---|---|---|
| 1 | `search_transcripts` | query, window?, device_id?, speaker_name?, top_k?≤20 | read | `retrieve::nearest` + conversation expansion + `context::enrich_sources_with_vision` |
| 2 | `latest_conversation` | – | read | `retrieve.rs` |
| 3 | `list_conversations` | window?, participant_name? | read | conversations catalog (0025) |
| 4 | `conversation_transcript` | conversation_id | read | `retrieve.rs` |
| 5 | `search_objects` | description, window?, device_id?, top_k? | read | CLIP text tower + scene_objects NN |
| 6 | `people_sightings` | person_name? (absent=recent roster), window?, device_id? | read | `retrieve::list_by_person` / `list_recent_persons:1270` |
| 7 | `who_was_i_with` | window? | read | `retrieve::list_co_occurring_persons:1195` + owner resolution |
| 8 | `co_presence` | person_a, person_b, window? | read | `retrieve::list_co_presence_pair:1322` |
| 9 | `plate_sightings` | plate_text?/label?, window? | read | plates resolve + `list_by_plate` |
| 10 | `presence_count` | subject_type, name_or_text, window? | read | `presence.rs` deterministic visit coalescing |
| 11 | `footage_stats` | window?, lane? | read | `stats.rs` |
| 12 | `reflection_digest` | window_days?≤365 | read | `analytics::compute_digest:203` + `render_digest:780` |
| 13 | `events_feed` | window?, lane?, alerts_only? | read | `retrieve::list_events` |
| 14 | `entity_profile` | name | read | 0024 read path incl. chat-time freshen |
| 15 | `list_devices` | – | read | backend `GET /v1/devices` |
| 16 | `watchlist_list` | – | read | backend `GET /v1/watchlist` |
| 17 | `audit_read` | window?, action? | read | backend `GET /v1/audit` |
| 18 | `graph_entity` | entity_name | read | `GET /v1/graph/entities/..` |
| 19 | `graph_connections` | a, b | read | `GET /v1/graph/path` + `edges` |
| 20 | `graph_neighborhood` | entity, depth?≤2 | read | `GET /v1/graph/neighbors/..` |
| 21 | `graph_journeys` | entity, window? | read | `GET /v1/graph/journeys` (Wave 4) |
| 22 | `graph_anomalies` | window? | read | `GET /v1/events?type=pattern_anomaly` |
| 23 | `graph_briefing` | date? | read | `GET /v1/graph/digests/{date}` (Wave 2) |
| 24 | `media_links` | source_index `[n]` \| person_name \| plate_text | read | sample-audio/face/crop + viewer timeline deep-link descriptors; structured `media` items, never inlined in prose; omitted from voice callers' registry |
| 25 | `watchlist_add` | subject_type, name_or_text, reason? | **mutate** | backend `POST /v1/watchlist` |
| 26 | `watchlist_remove` | name_or_text | **mutate** | backend `DELETE /v1/watchlist/{id}` |
| 27 | `alert_rule_create` | subject_type, subjects[], min_severity?, channel? | **mutate** | backend `POST /v1/alert-rules` |
| 28 | `alert_rule_delete` | rule_id | **mutate** | backend `DELETE /v1/alert-rules/{id}` |
| 29 | `export_clip` (Wave 3+) | device_id, window | **mutate** | viewer export path |
| 30 | `ask_user` | question | meta | terminates turn; question becomes the assistant message; voice → `AWAIT_FOLLOWUP` |
| 31 | `final_answer` | *(react runtime only)* text, cited `[n]` | meta | rig runtime: a plain assistant message IS the final answer |

**Registry sizing** (locked): Phase 1 registers tools 1–14 + 30 (~15 schemas). A 25+-tool catalog measurably degrades a 7B's selection accuracy and eats num_ctx. Graph tools (18–23) register only when `/v1/graph/*` probes healthy (the CLIP/TTS self-disable precedent in `hushai-rag/src/lib.rs`). Mutating tools register only when `GOTHAM_MUTATIONS_ENABLED=true` AND the caller is an admin surface; voice callers additionally require `owner_verified`.

## 2.4 Agent loop

```
INIT      resolve session (agent_id="gotham"), history window, CallerContext, tz;
          persist user turn; intercept: pending confirmation? -> CONFIRM_EXEC (no LLM)
PLAN      LLM via rig multi_turn (tools registered; preamble + history + facts briefing).
          SSE phase{"planning"} before each completion call.
 ├─ tool_calls -> GATE (PromptHook::on_tool_call): unknown tool -> Skip(feedback);
 │      budget exhausted -> Skip("budget exhausted — answer from what you have");
 │      mutate without confirmation -> PENDING (persist action, emit confirm, end turn)
 │    -> EXEC per-tool tokio timeout GOTHAM_TOOL_TIMEOUT_MS; SSE tool_call{...}
 │    -> OBSERVE truncate + register sources; SSE tool_result{...}; loop -> PLAN
 ├─ ask_user   -> question becomes the answer text; persist; done (voice: AWAIT_FOLLOWUP)
 └─ plain text -> ANSWER: SSE sources[...] then token{delta}...
PERSIST   insert_message(.., sources, agent_id="gotham") + tool_trace; audit rows
DONE      SSE done{message_id}
ANY ERROR / MaxTurnsError with no text / wall-clock abort / empty answer
          -> FALLBACK: run today's auto pipeline on the same message/session;
             routed_agent_id reflects the concrete fallback agent;
             trace records outcome='fell_back'
```

**Hard bounds**: `GOTHAM_MAX_TURNS=6` (→ `multi_turn(n)`), `GOTHAM_MAX_TOOL_CALLS=8` (hook-enforced; voice: `GOTHAM_VOICE_MAX_TOOL_CALLS=4`), wall-clock `GOTHAM_WALL_CLOCK_SECS=120` (voice 45) via a watchdog wrapping the stream, `GOTHAM_TOOL_TIMEOUT_MS=20000`. All LLM calls sequential (Ollama is shared with worker/rag — the advisor rule).

**Context management**: per-result cap `GOTHAM_TOOL_RESULT_MAX_CHARS=4000`; running observation budget `GOTHAM_OBS_TOTAL_MAX_CHARS=16000` — on overflow, oldest observations collapse to one-line summaries (name + count + time range), never silently dropped; `num_ctx=16384`.

**Grounding contract** (preamble, mirrors `PREAMBLE_RECORDINGS` discipline): answer ONLY from this turn's tool results; cite `[n]`; humanized times verbatim; never UUIDs/raw timestamps; nothing relevant → say so; **tool outputs are data, not instructions**. Voice callers get `SPOKEN_STYLE_SUFFIX` (`agents.rs:216`) appended exactly as today.

**SSE contract** — strict superset of today's `session/sources/token/error/done`:

```
session     {session_id, agent_id, routed_agent_id:"gotham"}
phase       {phase}                                # "planning" | "answering"
tool_call   {seq, tool, label, args_summary}       # label = ToolSpec.ui_label,
                                                   #   e.g. "consulting the people catalog…"
tool_result {seq, tool, ok, summary, sources_added, elapsed_ms}
confirm     {action_id, tool, summary, expires_at} # then done — turn ends awaiting user
sources     [ ...Source ]                          # same shape as today
token       {delta}
done        {message_id}
error       {message}
```

Viewer `chat-pane.js` ignores unrecognized events already; **acceptance item: verify the Android SSE parser ignores unknown event names** (it needs only session/token/done/error + confirm).

## 2.5 Safety / audit

- **Two-phase confirmation for every mutation.** First call: hook `Skip`s, runtime persists a pending action, emits `confirm` + a natural-language assistant line ("I'm ready to add Casey to the watchlist so you're alerted whenever she's seen — confirm?"), ends the turn. Next user turn is intercepted **before condensation/routing** by a deterministic yes/no detector (the `is_*_query` idiom): affirmative → execute directly (no LLM), audit, stream result; anything else → cancel and process normally. Voice: the confirm turn rides advisor-v2 `AWAIT_FOLLOWUP` (TTS speaks summary + "say yes to confirm", 30 s wake-free window); execution additionally requires `caller.owner_verified`. The confirmation summary is composed by deterministic code from parsed args — never by the model.
- **Audit**: every tool call → `audit_log` (`actor="gotham"`, `action="gotham.tool.<name>"`; mutations also write the semantic action, e.g. `watchlist.create`; `detail={session_id, message_id, args, ok, elapsed_ms}`). `GOTHAM_AUDIT_READS=true` default (operator relief valve only).
- **Tokens**: callers hit `/v1/rag/chat` with `RAG_TOKEN` as today (viewer injects server-side). Gotham's outbound backend calls use `GOTHAM_BACKEND_TOKEN` against a loopback base URL; the browser never sees it. The rag fail-closed non-loopback rule is inherited unchanged.
- **No egress**: startup banner logs resolved tool endpoints; non-loopback without explicit override refuses to register.
- **Prompt injection**: read-only Phase 1; human-confirmed mutations; data-not-instructions preamble; deterministic confirm summaries (§3 posture).

## 2.6 Migration `0031_gotham.sql`

```sql
ALTER TABLE chat_messages ADD COLUMN tool_trace jsonb;   -- assistant turns only; NULL elsewhere
-- [{seq, tool, args, ok, elapsed_ms, result_chars, outcome}] — shape, not payloads
-- (full results are NOT persisted; audit_log carries args)

CREATE TABLE gotham_pending_actions (
    action_id  uuid PRIMARY KEY,
    session_id uuid NOT NULL REFERENCES chat_sessions (session_id) ON DELETE CASCADE,
    tool       text NOT NULL,
    args       jsonb NOT NULL,
    summary    text NOT NULL,
    status     text NOT NULL DEFAULT 'pending',  -- pending|confirmed|cancelled|expired
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL
);
CREATE UNIQUE INDEX gotham_pending_one_per_session
    ON gotham_pending_actions (session_id) WHERE status = 'pending';
```

## 2.7 Surfaces

- **Viewer chat**: new registry entry `gotham` / display "Detective" (`AgentKind::Gotham` in `agents.rs:30` enum + registry). `parse_agent_label` unchanged in Wave 1 — the auto router does NOT know about it yet. Reached via the advisor-v2 slash picker (rows: `auto | advisor | gotham`) but — unlike advisor — it is the **same** `/v1/rag/chat` endpoint with `agent_id:"gotham"`, so ChatPane needs only additive rendering: phase pill (advisor styles), tool-step list from `tool_call`/`tool_result` (citation-chip pattern), confirm bubble whose Confirm/Cancel buttons send "yes"/"no".
- **Voice**: keyword invocation per the advisor-v2 pattern — "⟨wake⟩ **detective** ⟨question⟩" routes with `agent_id:"gotham"` + `caller:{kind:"voice", owner_verified, device_id}`. Risk note: "detective" must be validated against the Vosk small model exactly as "advisor" was (the v2 spec's one hard external dependency); fallback candidate words: "inspector", "sherlock". Voice clients ignore `phase`/`tool_*`; `confirm` → `AWAIT_FOLLOWUP`.
- **Auto-promotion (Wave 3 of G3)**: deterministic `is_multi_hop_query` pre-route (≥2 resolved entities + a relational/causal join: "who is X to Y", "walk me through", "investigate", "connect") + one new `ROUTER_PREAMBLE` category (`agents.rs:84`), keeping `expect_routed_agent` fixtures stable. Until then `auto` behavior is byte-identical.
- **Proactive channel (Wave 3 of G3)**: a tokio interval task (`GOTHAM_BRIEFING_ENABLED`, `GOTHAM_BRIEFING_HOUR=7`) runs one bounded loop turn over `graph_briefing` + `graph_anomalies` + `events_feed`, writes an `events` row (`event_type='gotham_briefing'`) so it rides the existing feed/alert delivery, and stores the narrative as a fresh gotham chat session the user can open and interrogate. The digest table is the fact source; Gotham narrates and connects, never re-derives.
- **Advisor cross-link**: a `consult_advisor` tool is **explicitly deferred** — multi-minute latency and the Yenta follow-up protocol don't fit a bounded tool call.

## 2.8 Part-2 touch points

| # | File | Change |
|---|---|---|
| 1 | `hushai-rag/src/gotham/*` | new module tree (§2.2) |
| 2 | `hushai-rag/src/chat.rs:95-360` region | dispatch branch for `agent_id="gotham"` + whole-turn fallback seam + pending-confirmation intercept before condensation |
| 3 | `hushai-rag/src/agents.rs` | `AgentKind::Gotham` + registry entry; (Wave 3) router category |
| 4 | `hushai-rag/src/llm.rs:60` region | extend the `tune` idiom with tools + num_ctx params |
| 5 | `hushai-rag/src/config.rs` | `GOTHAM_*` knobs |
| 6 | `hushai-rag/Cargo.toml` + `Cargo.lock` | pin `rig` exact; document rig-core resolution |
| 7 | `hushai-backend/migrations/0031_gotham.sql` | §2.6 + `migrations/README.md` row |
| 8 | `hushai-viewer/ui/js/chat/chat-pane.js` (+ advisor-v2 `slash.js`) | tool-step rendering, confirm bubble, slash row |
| 9 | `hushai-android` `AssistantRouting.kt` seam (advisor-v2) | "detective" keyword route (Wave 3 of G3 / G4) |
| 10 | `AGENTS.md:384-387` | correct the "Rig has no tool-calling" paragraph |

# Part 3 — E2E verification capability

## 3.1 Two eval modalities, split by determinism class

The harness's deepest convention (`hushai-eval/RECURSIVE_TESTING.md`): LLM-free output can gate everywhere; LLM output starts in staging.

- **`graph`** — Tier-1 deterministic. After injection quiesces AND the graph fold reaches quiescence (fold watermark ≥ max injected event time — eval pins `GRAPH_INTERVAL_SECS` low and `GRAPH_GRACE_SECS=0` in `local_dev/eval.env`, the `PROFILE_GRACE_SECS` precedent; the poller in `src/poll.rs` gains this check), score the graph read APIs + DB directly. Same clips + pinned timestamps + locked knobs ⇒ byte-identical edges. Eligible for `train`/`holdout` and the full gate. All assertions **assignment-invariant**: edges asserted by denormalized names/kinds via enrollment refs (the `clip_speaker_roster` `enroll:` precedent), never minted UUIDs.
- **`agent`** — scores the G3 runtime over live SSE, structurally: the chat-modality pattern (`must_contain`/`expect_number`/`expect_routed_agent`, `hushai-eval/src/fixtures.rs:300`) plus the tool trace. **Staging-only at first** (`--fixtures staging`), INCONCLUSIVE when the runtime is absent (the advisor-modality posture). Promotion to train only after two machines agree.

Tool-trace observability: the runtime's `tool_call`/`tool_result` SSE events are parsed by `query_rag.rs` (the advisor `memory`-event precedent); absent events (old binary) degrade tool metrics to Info, never FAIL. Briefing assertions hit the **structured `sections` JSON**, never the prose; narration gets Info-only reference-cosine at most.

## 3.2 New assertion fields

Each becomes a `Metric` with `Direction` + `floor_ok` in `src/score.rs`, gated on modality in `score_all`; ground-truth structs in `src/fixtures.rs`; queries in `src/query.rs`; tolerance bands in `baseline.rs` (edge/anomaly counts strict, like counts today); new keys classify as `New` against existing baselines — non-gating retrofit.

| Field | Modality | Semantics |
|---|---|---|
| `expect_entity {kind, name}` | graph | resolved entity exists (via enrolled names) |
| `expect_edge {from, to, kind, min_evidence}` | graph | edge present with ≥ N evidence observations |
| `expect_no_edge {from, to, kind}` | graph | counter-assertion: below-threshold pairs must NOT bind |
| `expect_journey {subject, cameras[]}` | graph | ordered subsequence of camera hops (Wave 4) |
| `expect_anomaly {kind, subject}` / `expect_no_anomaly` | graph | `pattern_anomaly` events row present/absent in the pinned window |
| `expect_briefing_counts {…}` / `expect_briefing_mentions []` | graph | against `daily_digests.sections` for a pinned date |
| `expect_tool_calls_any/all []`, `max_tool_calls` | agent | trace contains tools; loop cap honored |
| `expect_confirmation: bool` | agent | a `confirm` event was (not) emitted |
| `must_contain_any` / `must_not_contain` / `expect_number` / `expect_routed_agent` / `min_citations` | agent | reused verbatim from the chat modality |

Knob registration: add `"GRAPH_"` to `KNOB_PREFIXES` (`hushai-eval/src/manifest.rs:70`) — safe to prefix-fold (no secrets/URLs in the family). `GOTHAM_*` determinism knobs are **hand-listed** in `KNOBS` (`manifest.rs:19`) — see §5.

## 3.3 Fixture bank F1–F11

Multi-clip scenarios via `Meta.injections[]` (`fixtures.rs:54`) staging one subject across days/cameras (the `repeat_visitor` recipe) are exactly the graph's input shape. All graph fixtures pin `base_capture_unix_nanos` (invariant 3); multi-day offsets are multiples of 86 400e9.

| # | Case | Split | Scenario | Key assertions |
|---|---|---|---|---|
| F1 | `graph_face_voice_bind` | train | enrolled person speaking on camera; second silent face | `expect_edge{Alice-voice↔Alice-face, same_identity_candidate, min_evidence}`; `expect_no_edge` for the silent face |
| F2 | `graph_cross_camera_fusion` | train | same subject: front cam day 1, garage cam day 2 | `visits_place` edges to both devices; (Wave 4) `expect_journey [front, garage]` |
| F3 | `graph_person_vehicle` | train | person + plate co-staged twice; a second pair once | `expect_edge{person→plate, arrived_with_vehicle}`; `expect_no_edge` for the single co-sighting |
| F4 | `graph_baseline_rhythm` | train | subject at ~09:00 across 5 synthetic days | baseline hour bucket ≈ 9, visits ≥ 5; `expect_no_anomaly` |
| F5 | `anomaly_novel_time` | train | F4's ladder + one 03:00 appearance | `expect_anomaly{off_schedule_presence}` — scored via the existing events modality |
| F6 | `anomaly_negatives` | **holdout, sealed** | known subject at usual time + one never-seen face once | known subject mints **0** anomalies; unknown-cluster fires at most per its gate (counter-fixture posture) |
| F7 | `briefing_daily` | train | multi-day scenario (shares F4/F5 media) | `expect_briefing_counts{visits,…}` + `expect_briefing_mentions` for the pinned date |
| F8 | `agent_single_tool` | staging | reuse a graph scenario | "how many times did Alice visit this week" → `expect_tool_calls_any[presence_count]`, `expect_number` |
| F9 | `agent_multi_hop` | staging | person + plate + conversation staged | "did the person who drives EMD774 ever talk to Bob?" → tools ⊇ {plate_sightings, graph_connections, list_conversations}; `must_contain_any` planted token |
| F10 | `agent_journey_narrate` | staging | F2's scenario | journey/graph tool called; cameras mentioned in order |
| F11 | `agent_refusal_no_data` | staging | minimal | nonexistent entity → `must_not_contain` decoys, `max_tool_calls` honored, clean stream (no `error` event) |

**Calibration protocol** (carried verbatim from the advisor-v2 spec): first live run freezes observed values (widen, never narrow); two back-to-back runs with identical verdicts before freezing any content assertion; **never loosen a gate to go green** (RECURSIVE_TESTING §4 cardinal rule).

## 3.4 Harness touch points

| # | File | Change |
|---|---|---|
| 1 | `hushai-eval/src/fixtures.rs` | `GraphGt` + agent assertion fields on `ChatQ` (`fixtures.rs:254`) |
| 2 | `hushai-eval/src/score.rs` | `graph`/`agent` scorers, gated in `score_all` |
| 3 | `hushai-eval/src/query.rs` | edge/baseline/digest queries |
| 4 | `hushai-eval/src/query_rag.rs` | parse `tool_call`/`tool_result`/`confirm` SSE events |
| 5 | `hushai-eval/src/poll.rs` | graph-fold quiescence check |
| 6 | `hushai-eval/src/manifest.rs:19,70` | `GOTHAM_*` hand-list; `"GRAPH_"` prefix |
| 7 | `local_dev/eval.env` | `GRAPH_GRACE_SECS=0`, low fold interval, `GOTHAM_TEMPERATURE=0`, `GOTHAM_SEED`, `GOTHAM_ENABLED=true` |
| 8 | `fixtures/{train,holdout,staging}/<case>/` | F1–F11 `meta.json` + `expected.json` |

---

# Part 4 — Acceptance phases

Executed after each wave's implementation; fenced commands + bold PASS criteria in the executed revision (the `v1_spec.md` style). Run from repo root; a failed criterion stops the run. Wave map: **Wave 1** executes Phases 0–C (F1–F3 subset) + F-smoke; **Wave 2** completes C–F; **Wave 3** executes G–H.

**Phase 0 — Preconditions.** Existing full gate exit 0 (`cargo run -p hushai-eval -- run --tier full --fixtures all`); migrations 0028+ applied on dev + `hushai_test` (+ `hushai_test_phys` for Wave 3); Ollama models present (`qwen2.5:7b`, embed model); no dirty graph state. **PASS:** all green before any Gotham commit is judged.

**Phase A — Build + unit gates.** Workspace build + clippy clean on touched crates; graph pure-core unit tests (canonical edge ordering, binding score, idempotent re-fold, merge/delete reconciliation, N²-cap determinism); eval unit total stated explicitly (current count + new scorer/parse tests). **PASS:** counts met, 0 failures.

**Phase B — Graph API curl-level.** Seed via one injection case; curl entity page / edges / neighbors through the viewer proxy; bearer required (401 without); `pg_advisory` lock respected under concurrent passes; metrics counters present; rebuild endpoint round-trips to identical edge set (byte-compare sorted dump). **PASS:** shapes + auth posture + rebuild determinism verified.

**Phase C — Graph fixtures Tier-1.** Per-case runs → `--update-baseline` → gating run ×2 with identical verdicts, F1–F4 (+ F6 sealed holdout in the full gate). **PASS:** exit 0 twice.

**Phase D — Anomaly → alert integration (Wave 2).** F5 anomaly visible via `/v1/events?type=pattern_anomaly`; an alert rule on it fires into the feed (and webhook, HMAC intact); negative: a conforming day fires nothing. **PASS:** end-to-end alert + clean negative.

**Phase E — Briefing (Wave 2).** Pinned-date `sections` byte-stable across two folds; `rendered_text` renders; F7 green. **PASS:** deterministic core proven.

**Phase F — Agent runtime live (staging).** Startup tools-probe logs runtime selection; F8–F11 via `--fixtures staging`; live SSE shows `tool_call` trace; adversarial runaway prompt ("keep searching until you find…") stops at `GOTHAM_MAX_TOOL_CALLS`; kill-switch test: `GOTHAM_ENABLED=false` → existing chat fixtures byte-identical; fallback test: unreachable backend tool target → turn completes via auto pipeline, `outcome='fell_back'` in trace. **PASS:** exit 0 twice, cap honored, kill-switch + fallback proven.

**Phase G — Viewer investigation UX (Wave 3).** Manual numbered script (slash row → Detective pane → tool steps render → confirm bubble; binding queue confirm/reject with sample audio+face) + headless-Chrome checks appended to the committed e2e harness (`data-cmd` hooks; token-never-in-browser assertion). SKIP ≠ PASS. **PASS:** all observations + e2e 0 failed.

**Phase H — Voice (Wave 3).** Logcat marker contract under `HUSHAI_TX` (grep-stable strings: keyword routed, owner verified, confirm spoken, `AWAIT_FOLLOWUP` entered/expired) + phone rig loop on the phys stack (`hushai_test_phys`, +2 ports). **PASS:** marker order + DB shape.

---

## §5 Config reference

**`GRAPH_*` family** — prefix-folded into `KNOB_PREFIXES` (`manifest.rs:70`; no secrets/URLs in the family; over-inclusion is the documented policy). ★ = determinism-relevant, folded into `graph.rs::config_hash`:

| Knob | Default | Meaning |
|---|---|---|
| `GRAPH_ENABLED` | `true` | master switch (worker-0 driver) |
| `GRAPH_INTERVAL_SECS` | `300` | pass cadence |
| `GRAPH_MAX_EVENTS_PER_PASS` | `2000` | backfill converges over passes |
| `GRAPH_REBUILD_ON_START` | `false` | truncate derived rows + reset watermarks once |
| ★ `GRAPH_GRACE_SECS` | `90` | settle window (≥ 2× event session bucket) |
| ★ `GRAPH_COPRESENCE_SLACK_SECS` | `120` | overlap slack for co_present / binding trials |
| ★ `GRAPH_COPRESENCE_MAX_SUBJECTS` | `12` | pairwise cap per window (N² guard) |
| ★ `GRAPH_VEHICLE_CORR_WINDOW_SECS` | `180` | person↔plate correlation window |
| ★ `GRAPH_EDGE_SAMPLE_CAP` | `16` | evidence samples kept per edge |
| ★ `GRAPH_BIND_MIN_SESSIONS` | `3` | binding: min `together` before candidate |
| ★ `GRAPH_BIND_MIN_CONFIDENCE` | `0.6` | binding: min Jaccard |
| ★ `GRAPH_BIND_MARGIN` | `0.2` | binding: top-1 vs top-2 margin |
| ★ `GRAPH_BASELINE_WINDOW_DAYS` | `30` | baseline trailing window |
| ★ `GRAPH_ANOMALY_MIN_VISITS` | `5` | baseline maturity gate |
| ★ `GRAPH_ANOMALY_HOUR_MIN_FRAC` | `0.05` | off-schedule threshold |
| ★ `GRAPH_ANOMALY_UNKNOWN_CLUSTER_MIN` | `3` | unknown-person cluster size |
| ★ `GRAPH_JOURNEY_GAP_SECS` | `600` | cross-camera hop stitch gap |
| `GRAPH_DIGEST_HOUR_LOCAL` | `21` | digest generation hour (existing fixed-offset tz convention) |

**`GOTHAM_*` family** — determinism-relevant subset **hand-enumerated** in `KNOBS` (`manifest.rs:19`). Do **NOT** prefix-fold `GOTHAM_` — it would sweep `GOTHAM_BACKEND_TOKEN` (secret) and machine-specific URLs (the `ADVISOR_`/`RAG_` trap the manifest comments already warn about):

| Knob | Default | Hash? | Meaning |
|---|---|---|---|
| `GOTHAM_ENABLED` | `true` | — | kill switch; off ⇒ existing chat byte-identical |
| `GOTHAM_RUNTIME` | `rig` | ✔ | `rig` \| `react` |
| `GOTHAM_LLM_MODEL` | `qwen2.5:7b` | ✔ | loop model (14b recommended where RAM allows) |
| `GOTHAM_JUDGE_MODEL` | unset | ✔ | optional critique pass model |
| `GOTHAM_TEMPERATURE` | `0.0` | ✔ | decode profile |
| `GOTHAM_SEED` | unset | ✔ | decode profile |
| `GOTHAM_NUM_CTX` | `16384` | ✔ | explicit context window (silent-truncation lesson) |
| `GOTHAM_MAX_TURNS` | `6` | ✔ | LLM iterations (`multi_turn`) |
| `GOTHAM_MAX_TOOL_CALLS` | `8` | ✔ | hook-enforced tool budget |
| `GOTHAM_VOICE_MAX_TOOL_CALLS` | `4` | ✔ | voice budget |
| `GOTHAM_TOOL_TIMEOUT_MS` | `20000` | ✔ | per-tool timeout |
| `GOTHAM_WALL_CLOCK_SECS` | `120` | ✔ | turn watchdog (voice: `GOTHAM_VOICE_WALL_CLOCK_SECS=45`) |
| `GOTHAM_TOOL_RESULT_MAX_CHARS` | `4000` | ✔ | observation truncation |
| `GOTHAM_OBS_TOTAL_MAX_CHARS` | `16000` | ✔ | running observation budget |
| `GOTHAM_MUTATIONS_ENABLED` | `false` | — | registers mutating tools (Wave 3 of G3) |
| `GOTHAM_CONFIRM_TTL_SECS` | `300` | — | pending-action expiry |
| `GOTHAM_AUDIT_READS` | `true` | — | audit read tool calls |
| `GOTHAM_BACKEND_BASE_URL` | `http://127.0.0.1:8080` | — | outbound admin/graph target |
| `GOTHAM_BACKEND_TOKEN` | — | — | **secret** — never hashed/logged |
| `GOTHAM_BRIEFING_ENABLED` | `false` | — | proactive briefing task |
| `GOTHAM_BRIEFING_HOUR` | `7` | — | local hour |
| `GOTHAM_CRITIQUE_ENABLED` | `false` | ✔ | judge pass toggle |

---

## Non-goals / referenced, not duplicated

1. **Events/alerts/notifications (Pillar A, shipped)** — anomalies and briefings *emit into* it; Gotham never rebuilds rules/cooldown/delivery/push (`docs/feature-parity-roadmap.md` A1–A7).
2. **Entity profiles (0024)** — profiles = per-entity narrative log; graph = inter-entity edges. Boundary locked; neither writes the other's rows.
3. **Advisor v2 integration spec** — slash-picker registry, `AWAIT_FOLLOWUP`, pane seams: consumed as built; G4 sequenced after those PRs.
4. **VSaaS roadmap C3 (multi-tenancy/RBAC), C4 (floor-plan/map), B4 (S3 blobs)** — journeys render as lists/timelines, not maps; single-owner posture throughout.
5. **Open perception issues** (rolling-window ASR, face emotion/activity captions, phone-as-gateway — `Issues/unfinished/`) — Gotham consumes whatever perception exists; improving perception is out of scope. When emotion/activity captions land, they enrich edges/digests automatically via `events`.
6. **New perception lanes or models** — the graph derives exclusively from existing catalogs + events.
7. **Cloud egress or external intelligence feeds** — never; local-first is the differentiator.
8. **Camera contract changes** — downstream-only; reserved proto fields 18–40 stay reserved.

## Risks / open questions

1. **Local-7B tool-calling reliability** — the load-bearing bet of G3. Mitigations locked into the contract: lean per-phase catalog (~15 schemas); rig's in-loop invalid-call retry; hook budget feedback; `react` fallback runtime; whole-turn fallback to the auto pipeline (chat never regresses); 14b tiering. Open: final model choice is decided by Phase F calibration data, never blind-tuned.
2. **Binding false positives poison fusion** — a wrong voice↔face or person↔plate edge propagates into journeys and briefings. Mitigations: evidence hysteresis (min-sessions), margin gate, never-auto-confirm, evidence-carrying UI, sticky rejection, merge/delete reconciliation in-tx. Ambiguous pairs (always-together couples) correctly *never* surface — an empty review queue can be right.
3. **Anonymous-identity churn** — unknown-person ids mint/merge frequently; edge correctness depends on the merge hooks firing inside *every* merge path (manual, auto_merge_recent, retro-attach). `recluster-deep` rewrites assignments wholesale — the safe response is an explicit graph rebuild; the runbook must say so.
4. **Merge orphan window** (inherited, documented) — merges don't repoint `events.subject_id`; loser events not yet drained never become edges. Same accepted loss as profiles.
5. **Co-presence N² blowup** on busy scenes — capped per window; the cap is hashed (determinism-relevant).
6. **Fixed-tz baselines shift at DST** (existing convention's limitation) — accepted; anomaly thresholds coarse enough for ±1 h.
7. **rig facade drift** — `rig = "0.37"` already resolves rig-core 0.38.2; pin exactly, and note loop semantics live in rig-core.
8. **Ollama contention** — agent loops multiply LLM calls on hardware shared with worker sentiment + rag; sequential calls, acceptance phases serialized, voice budgets halved.
9. **Prompt injection via recordings** — §3 posture: read-only Phase 1, human-confirmed mutations, data-not-instructions preamble, deterministic confirm summaries.
10. **Anomaly fatigue** — thresholds start conservative + report-only; frozen into hard bars from the first measurement run (voice-matrix frozen-bars precedent); a red is a finding, never a threshold edit.
11. **Voice keyword recognizability** — "detective" must pass the same Vosk small-model validation "advisor" required; fallbacks listed (§2.7).
12. **Gateway-audit posture for investigation reads** — the advisor chose audit-exclusion for privacy; graph reads currently audit like all `/v1/*`. Decide before G4 ships whether investigation queries deserve the advisor treatment (owner decision).

## Result matrix

| Phase | Check | Result |
|---|---|---|
| 0 | Preconditions (migrations 0028–0030 @ head, ollama models, clean graph) | ✅ |
| A | Build + clippy + unit totals (eval 30, graph 12, graph_db integration 1) | ✅ |
| B | Graph API + auth (401) + rebuild determinism (identical edge set) | ✅ |
| C | Graph fixtures F1–F3 ×2 (+ G2 F4/F5/F6 ×2, frozen `d4acc862`) | ✅ |
| D | Anomaly detection + emission — ALL FOUR predicates (`off_schedule_presence` F4/F5/F6 live ×2; `first_time_pairing`/`new_vehicle_for_person`/`unknown_person_cluster` via `graph_db` guard + `anomaly_first_pairing` staging fixture); alert-DELIVERY E2E (rule→feed+webhook, HMAC intact + negatives) via `hushai-worker/tests/alert_anomaly_delivery.rs` (real PG + real POST) | ✅ |
| E | Briefing byte-stable + F7 (`briefing_daily` 8/8 gate ×2, `d4acc862`; `graph_db` outlier-day digest guard) | ✅ |
| F | Agent staging F8–F11 ×2 + cap + kill-switch + fallback | ⬜ |
| G | Viewer investigation UX + e2e | ⬜ |
| H | Voice markers + phone rig | ⬜ |

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| Edges never appear | worker-0 driver gated off (`GRAPH_ENABLED`), or events not settling — check `GRAPH_GRACE_SECS` vs event sessionization; inspect `graph_state` watermarks |
| Edge counts differ between eval runs | a `GRAPH_*` knob changed without `--update-baseline` (config-hash mismatch is logged), or non-determinism bug — check tie-break sorts first |
| Binding queue always empty | gates too strict for the data (raise nothing — verify `together` counters in `evidence` first), or conversations not closing (threader) |
| Ollama HTTP 400 "does not support tools" | `GOTHAM_LLM_MODEL` template lacks tools — probe should have auto-selected `react`; check startup banner |
| Agent answers without citing / hallucinates | grounding preamble regression or observation truncation swallowed sources — check `tool_trace.result_chars` vs caps |
| Turn hangs then falls back | per-tool timeout vs backend down — check `audit_log` `ok=false` rows and `GOTHAM_BACKEND_BASE_URL` |
| Existing chat changed with Gotham off | forbidden — bisect: registry/router edits must be inert when `GOTHAM_ENABLED=false` |
| Anomaly storm after enabling G2 | baselines immature — `GRAPH_ANOMALY_MIN_VISITS` gate not applied, or backfill replayed old events as fresh (watermarks reset?) |

## Suggested PR slicing

1. **Migrations 0028–0030 + `graph.rs` pure core** — schema, deterministic derivation, unit tests, `migrations/README.md` rows. No consumers; smallest reviewable unit.
2. **`graph_pass.rs` + worker-0 driver + `/v1/graph/*` read API** — merge/delete hooks, metrics, proxy route, AGENTS.md component row + this spec cross-linked.
3. **Eval `graph` modality + F1–F3 + baselines** — makes Phases B/C executable.
4. **Baselines/anomalies → events integration + F4–F6** — Phase D. ✅ LANDED, incl. alert-DELIVERY E2E: `hushai-worker/tests/alert_anomaly_delivery.rs` proves `pattern_anomaly` event → `alerts::evaluate` (feed + webhook rows) → `delivery::run_once` (webhook sent + valid `X-Hushai-Signature` HMAC; feed left in-app) + negatives (wrong type / below-floor severity fire nothing). Gated on `DATABASE_URL`.
5. **Digest + endpoints + F7** — Phase E. ✅ LANDED: `patterns::build_and_upsert_digest` (deterministic `sections`/`rendered_text`, no LLM), `POST /v1/graph/digests/{date}` force-generate + worker-0 wall-clock driver (`GRAPH_DIGEST_HOUR_LOCAL`), eval `expect_briefing` (`BriefingGt` counts + mentions), F7 `briefing_daily` gate ×2 under `d4acc862`.
6. **`gotham/` module + rig runtime + probe + migration 0031 + slash row + AGENTS.md:386 correction + rig pin** — the G3 skeleton, read-only tools 1–14. ◑ LANDED (build/clippy/unit + kill-switch code-verified; live tool-loop deferred to PR7/Phase F): migration 0031 + rig exact-pin + AGENTS.md correction (part 1); then `hushai-rag/src/gotham/` (`mod.rs` `run_chat` streaming a SUPERSET SSE, `runtime.rs` rig `multi_turn`+`PromptHook` + `react` fallback + startup/graph probes + wall-clock watchdog, `tools.rs` a single `ToolDyn` over read tools 1–14 + `ask_user` + probe-gated graph tools, `preamble.rs`/`trace.rs`/`confirm.rs`), `GOTHAM_*` in `config.rs`, `chat.rs` dispatch (degrades to `Grounded` when `GOTHAM_ENABLED=false`, existing chat byte-identical), `agents.rs` `gotham` registry entry. 28 new unit tests green (react JSON parser, registry sizing, confirm yes/no, trace, schemas). **NOT done: viewer slash row (Phase G), voice keyword (Phase H), live tool-loop calibration (PR7).**
7. **Eval `agent` modality + F8–F11** — Phase F.
8. **Viewer investigation UI + binding queue + e2e** — Phase G (after advisor-v2 PR 2).
9. **Voice route + confirmations + phone rig** — Phase H (after advisor-v2 PR 3).

Each PR updates this spec's result matrix for the phases it makes executable.

## Doc-deliverables checklist (same-change rule)

- [x] `AGENTS.md`: component-map row for the Gotham layer (PR 6 — added the "Gotham Detective runtime" row + bumped registry count 7→8); **:384–387 tool-calling correction** (PR 6 ✅). Testing section still to gain `graph`/`agent` modality names (PRs 3/7 — `graph` done, `agent` pending).
- [x] `hushai-backend/migrations/README.md`: rows for 0028–0031 (0028–0030 prior; **0031 added PR 6**).
- [ ] `CHANGELOG.md`: entry per landed wave.
- [ ] `docs/feature-parity-roadmap.md`: one-line pointer under Pillar C — "intelligence layer → `Gotham.md`" (PR 2).
- [ ] `local_dev/eval.env` + `run_stack.sh`: `GRAPH_*`/`GOTHAM_*` determinism pins (PRs 3/7).


