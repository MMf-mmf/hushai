# Changelog

Dated development history for the Hushai workspace. Durable reference (architecture, invariants,
how to run) lives in [`AGENTS.md`](AGENTS.md); this file is the narrative of what changed when.
Newest first. Dates are when the work landed on the current development branch
(`feat/hushai-voice-assistant`).

## Unreleased (in-flight)

- **Gotham Wave 2 / Pillar G2 — daily briefing / digest (PR5, 2026-07-08, spec `Gotham.md` §1.6,
  Phase E, uses migration 0029 `daily_digests`)** — new `patterns::build_and_upsert_digest`: for a
  pinned civil day it materializes a deterministic `daily_digests` row — `sections` jsonb
  (`new_entities`, `top_visitors`, `anomalies`, `conversations`, `first_time_pairings`, `journeys`,
  plus a `counts` sub-object) and a template `rendered_text`, with **NO LLM at write time** (the
  `hushai-rag::analytics::render_digest` discipline — the RAG/G3 layer narrates at read time). Reads
  the same sessionized sources as the graph, with the same no-self-fold exclusion
  (`event_type NOT IN ('pattern_anomaly','gotham_briefing')`) over a capture-anchored day window
  `[D*86400-tz, (D+1)*86400-tz)`; deterministic throughout (BTreeMap order, total tie-breaks). Two
  triggers: `POST /v1/graph/digests/{date}` (`graph_pass::generate_digest_for_date`, admin/eval
  force-a-pinned-date, audit-logged, strict `YYYY-MM-DD` calendar validation → 400) and a worker-0
  wall-clock driver (`graph_pass::maybe_generate_daily_digest`, once local time passes
  `GRAPH_DIGEST_HOUR_LOCAL` default 21, idempotent by PK, re-checked under the graph advisory lock).
  `GRAPH_DIGEST_HOUR_LOCAL` is **NON-hashed** (a report schedule, not a stored derivation → NOT in
  `GraphCfg`/`config_hash`, and deliberately kept OUT of `eval.env` so the eval config-hash — which
  prefix-folds `GRAPH_` — does not shift and re-baseline F1–F6). `rebuild` does NOT truncate
  `daily_digests` (date-partitioned reports, not fold state); the eval `reset` does (derived data).
  Eval `graph` modality gains `expect_briefing` (`BriefingGt`: exact `counts` + label `mentions`
  against the structured `sections`, never the prose); the runner force-generates the pinned-date
  digest after the authoritative rebuild and fails CLOSED to INCONCLUSIVE if the endpoint is absent.
  **F7 `briefing_daily` (train) calibrated live on the rig — 8/8 assertions, gate ×2, frozen under
  config_hash `d4acc862`** (config-hash unchanged, so F1–F6 stayed valid). `graph_db` integration
  guard extended with an outlier-day digest assertion (the anomaly surfaces in `sections`; the
  subject is not "new" that day). Adversarial multi-agent review: 1 confirmed finding (a
  calendar-invalid but shape-valid date returned 500 not 400) fixed + verified live.
- **Gotham Wave 2 / Pillar G2 — baselines + pattern anomalies (2026-07-08, spec `Gotham.md`
  §1.6, uses migration 0029)** — new `hushai-backend::patterns` producer: per-touched-subject
  `entity_baselines` recompute (168 hour-of-week histogram, dwell p50/p90, device/companion
  top-K — pure math in `graph.rs`, deterministic) + `off_schedule_presence` anomaly emission,
  wired into the `graph_pass` transaction. Anomalies are ordinary `events` rows
  (`event_type='pattern_anomaly'`, `severity='warning'`, `metadata.kind`, idempotent
  `dedup_key='anom:<kind>:<subject>:<civil-day>'`, `ON CONFLICT DO NOTHING`), so they ride the
  shipped A-pillar (rules/cooldown/feed/webhook/push) with zero new alert plumbing; the worker-0
  driver alert-evaluates the freshly-emitted anomaly ids post-commit (the evaluator is
  worker-crate). **off_schedule is judged AS-OF** — each visit against the histogram of the
  subject's STRICTLY-EARLIER visits (the spec's incremental "new visit vs prior baseline" model),
  so a subject's first appearances (incl. the enrollment clip an hour before a case) never fire;
  only a later violation of an established rhythm does. The drain queries exclude
  `pattern_anomaly`/`gotham_briefing` so the graph never folds its OWN output (a feedback loop the
  `graph_db` guard caught). Eval `graph` modality gains `expect_anomaly`/`expect_no_anomaly`/
  `expect_baseline` (assignment-invariant: subject by enrolled name→id, `metadata.kind`,
  peak-hour-of-day); F4 `graph_baseline_rhythm` + F5 `anomaly_novel_time` (train) + F6
  `anomaly_negatives` (sealed holdout) **calibrated live on the rig, gate ×2, frozen under
  config_hash `d4acc862`**. `graph_db` integration guard extended (baseline row + off_schedule
  anomaly + immature-subject negative). Digest render + endpoints + F7 + the other three anomaly
  predicates are PR5/follow-ups; alert-DELIVERY E2E (Phase D feed/webhook) rides the shipped
  A-pillar and is the tracked remaining Phase-D item.
- **Gotham intelligence layer — Wave 1 / Pillar G1 data layer (2026-07-08, migrations
  0028–0030, spec `Gotham.md`)** — the entity/link graph: `entity_edges` (co_present /
  conversed_with / arrived_with_vehicle / visits_place / the review-queued voice↔face
  `same_identity_candidate` binding) + `graph_state` watermark (0028); `entity_baselines` /
  `daily_digests` (0029) and `entity_journeys` + `camera_adjacency` view (0030) ship their
  schema now, populated in later waves. New `hushai-backend::graph` (pure deterministic cores —
  canonical undirected ordering, co-presence pairing, binding Jaccard, evidence merge, journey
  stitch, anomaly predicates, `GraphCfg`/`config_hash`; 12 unit tests) + `graph_pass` (the
  `profiles.rs` sibling: `GRAPH_LOCK_KEY` advisory-locked watermark drain over settled `events`
  + closed `conversations`, idempotent edge upserts, `merge_in_tx`/`delete_entity_in_tx`
  reconciliation, `seed_owner_binding`, `rebuild`) driven by worker 0 (`GRAPH_*` knobs). Read/
  admin API `/v1/graph/*` (`graph_api`, bearer-authed, proxied via viewer `is_backend_path`):
  entity page/timeline, edges, neighbors (recursive CTE ≤ 3 hops), path (≤ 4), binding queue +
  confirm/reject (audit-logged, sticky reject), rebuild. Merge hooks land beside
  `profiles::merge_in_tx` in persons/speakers/plates. Deterministic (no LLM in materialization),
  DERIVED/rebuildable, never auto-merges identities.
  - **PR 3 — eval `graph` modality (2026-07-08).** The deterministic Tier-1 harness for the
    graph: `GraphGt` + `expect_entity`/`expect_edge`/`expect_no_edge` on `Expected`
    (`fixtures.rs`); `score_graph` gated in `score_all` — assignment-invariant (edges asserted by
    enrolled `display_name`/`device_id` → resolved to catalog ids, never minted UUIDs), undirected
    kinds match either endpoint order, `expect_no_edge` is threshold-aware (a below-`min_evidence`
    co-sighting counts as "did not bind"); an unwindowed `entity_edges` read + name→id resolution
    (`query.rs`); `poll::wait_graph_folded` (no OPEN conversations + fold watermark caught up to
    eligible events/closed-conversations, mirroring `graph_pass::drain_*`), wired into `run_case`
    before `observe`; `"GRAPH_"` folded into the eval config-hash (`manifest.rs`) so a knob change
    re-baselines; `eval.env` graph knobs (`GRAPH_ENABLED`, `GRAPH_INTERVAL_SECS=2`,
    `GRAPH_GRACE_SECS=0`). Fixtures F1–F3 (`graph_face_voice_bind`/`graph_cross_camera_fusion`/
    `graph_person_vehicle`) in **train** — authored in staging, Phase-C calibrated on the rig
    (each green + gate ×2), then promoted (a parse + edge/node-kind-invariant unit test guards
    their JSON). +7
    eval unit tests (6 scorer + 1 fixture parse). Every graph read query + the fold-quiescence gate
    LIVE-VALIDATED against the real 0028 schema (scratch schema, dropped).
  - **PR 3 verification pass — 3 fixes from an adversarial multi-agent review (2026-07-08).**
    (1) `reset::reset_db` never cleared the graph tables → a prior case's `entity_edges` bled into
    the next (`score_graph` reads them unwindowed): added guarded `RESET_GRAPH_SQL` (TRUNCATE
    `entity_edges`/`entity_baselines`/`entity_journeys`, UPDATE-reset the `graph_state` singleton;
    `to_regclass`-guarded so non-graph DBs are unaffected). (2) `graph_pass` correlates cross-subject
    edges (`arrived_with_vehicle`/`co_present`) BATCH-LOCALLY, so the eval's serial inject-and-quiesce
    cadence drained a person and their vehicle in separate passes → the edge never formed. The graph
    modality now waits for inputs to settle (`poll::wait_graph_inputs_settled`) then triggers one
    authoritative `POST /v1/graph/rebuild` (`query::trigger_graph_rebuild`, reusing the injection
    bearer) that folds the whole scenario in a single batch; `GRAPH_INTERVAL_SECS` pinned high so the
    incremental pass doesn't race it. (3) the rebuild handler used `GraphOpts::default()` (grace 90) —
    which would exclude freshly-injected events (`updated_at` younger than 90s) → empty rebuild; added
    `GraphOpts::from_env()` (backend) so the endpoint honors configured knobs (`GRAPH_GRACE_SECS=0`
    for the eval). New backend integration test `tests/graph_db.rs` seeds events + rebuilds + asserts
    cross-subject edges (`arrived_with_vehicle` obs 2 vs below-bar 1, `co_present`, `visits_place`) —
    the permanent regression guard, green on `hushai_test` (now migrated to head 0030).
  - **PR 3 live E2E on the rig — Gotham Phases 0/A/B/C ALL PASS (2026-07-08).** F1
    (`graph_face_voice_bind`, voice↔face binding → `candidate`), F2 (`graph_cross_camera_fusion`,
    `visits_place` to two devices), F3 (`graph_person_vehicle`, `arrived_with_vehicle` obs 2 + a
    below-bar Bob negative) each run end-to-end against the live stack (backend+worker+rag+ollama on
    `hushai_test`), PASS, and gate ×2 with identical verdicts (baselines frozen under config_hash
    `d4acc862`). Phase B: `/v1/graph/*` returns 401 without bearer, correct edges/entity/neighbors
    shapes, and `rebuild` round-trips to a byte-identical edge set. Fixture media (Judith + Sally PD
    portraits, the Auckland plate crop, JFK Rice speech) is reproducible via `fetch_eval_clips.sh`
    (F2 reproduces its baseline from script-regenerated clips). **The live run caught + fixed 2 more
    real bugs:** (a) `enroll_plate` never upserted the case device before the FK insert → plate
    direct-seed failed (added the `upsert_device` the person/speaker path already does); (b)
    `graph_pass::upsert_edge`'s `ON CONFLICT DO UPDATE` **omitted `status`**, so a binding's status
    froze at the first trial's NULL and NEVER transitioned NULL→`candidate` through folding — the G1
    voice↔face review queue never auto-surfaced in shipped code (fix: `status = $13`; guarded by an
    extended `graph_db.rs` binding assertion). Also: the ALPR reads plate EMD774 as `EM0774` (D→0),
    so F3 enrolls the OCR norm with display name 'EMD774'. F1–F3 promoted `staging`→`train` and
    the full-suite re-baseline the `GRAPH_` config-hash change forced was run — all fixtures now
    baselined under config_hash `d4acc862` (graph metrics byte-deterministic). Two pre-existing
    LLM-answer chat fixtures (`money_talk`, `repeat_visitor`) fail deterministically on brittle
    keyword assertions under 7B answer drift — unrelated to Wave 1 (hushai-rag untouched, model
    unchanged), tracked separately, NOT re-baselined.
- **Ahithophel advisor v1 (2026-07-06, migrations 0026/0027, new crate `hushai-advisor`)** —
  the first Ahithophel-framework agent (AhithophelPlan "Agent Architecture and Roles"): an
  Axum service (`:8095`) running a bounded multi-agent consultation pipeline over an ingested
  book. Min-Info + Yenta collapse into ONE sufficiency-gate call (numbered follow-up
  questions, `ADVISOR_MAX_FOLLOWUP_ROUNDS` cap, unparseable verdict = proceed — a flaky 7B
  can never wedge a session); Message Refiner folds the gathered Q&A into a standalone
  problem statement; Traffic Controller routes over 50 chapter synopses widened by pgvector
  chunk candidates (anti-tunnel-vision); Answer→Controller→Refiner iterate at most
  `ADVISOR_MAX_REFINE_ITERS`, converging when routing proposes no new chapters; the polished
  answer streams over SSE (`session`/`phase`/`questions`/`chapters`/`token`/`done`, extending
  the rag chat protocol); a Q&A summary embeds into `advisor_memories` for retrieval into
  later consultations. Corpus: `books`/`book_chapters`/`book_chunks` (0026) populated by the
  idempotent `ingest-book` binary from `Agent Ahithophel/books/chapters_text/` — deterministic
  OCR heuristics (de-hyphenation, page-number strip, drop-cap rejoin, mid-sentence paragraph
  merge) + a length-ratio-guarded LLM copy-edit pass, paragraph-packed ~1500-char chunks in
  the shared mxbai 1024-d space. Sessions: `advisor_sessions` phase machine
  (gathering/answering/done) + gap-free-seq `advisor_messages` with a `kind` discriminator
  (0027). First service to set `num_ctx` explicitly (`ADVISOR_NUM_CTX=16384`) — multi-chapter
  prompts silently truncate at Ollama's 4096 default. Temperament and Research agents
  deliberately deferred (temperament hook parameter exists; web research conflicts with the
  local-only privacy stance). run_stack.sh launches it as service #5 and mints
  `ADVISOR_TOKEN`.

- **Conversation threading (2026-07-06, migration 0025)** — persisted conversations with
  concurrent-group separation (the AhithophelPlan "conversa" ask). `conversations` catalog +
  `conversation_id`/`turn_index` on `transcript_sentences`, assigned by a deterministic batch
  threader on worker 0 (silence-gap blocks → speaker-turn/topic disentanglement of interleaved
  same-mic group conversations; Elsner–Charniak-style pairwise evidence + graph partition).
  Closed conversations freeze + emit one `conversation` event; cross-device overlap LINKS
  (`link_group_id`), never merges. RAG became conversation-scoped: neighborhood expansion +
  per-conversation prompt sections with a never-combine instruction, id-change stop in the
  recency backscan (fixes back-to-back conversations gluing under the gap), threaded-first
  window grouping, a participants intent ("what did X and Y talk about"), voice recency
  anchored to the asking phone, and `GET /v1/rag/conversations[/{id}]`. Analytics groups by
  `COALESCE(conversation_id, device:gap_seq)`. Eval grew a `conversations` modality
  (pairwise-F1 / coverage / count / must-not-merge gates + conversation-scoped citation
  checks), a threading quiesce wait, THREADER_/CONVO_/RAG_EXPAND_ knob lineage, and 9 conv_*
  fixtures (5 train/holdout diarization-independent, 2 sealed holdout, 2 staging probes incl.
  the same-mic interleave that hard-depends on TTS voice separability). NOTE: sequential topic
  drift without temporal interleave deliberately stays ONE conversation
  (`THREADER_TOPIC_ONLY_SPLIT=false`); same-topic interleaved groups on one mono mic are
  documented as information-theoretically inseparable — no 100% claim. Retrieval gained a
  relative-margin prune (`RAG_PRUNE_REL_MARGIN`, default 0.25): semantic hits with
  `distance > best + margin` are dropped BEFORE neighborhood expansion, so one marginal hit
  from an unrelated conversation (just under the absolute `RAG_DISTANCE_THRESHOLD`) can no
  longer pull that entire conversation into the grounded prompt — this is what makes
  answer citations collapse to a single conversation on single-topic questions.

- **Executive-chat overhaul (2026-07-05)** — fixes for the owner's live-testing bug batch:
  visit coalescing (`presence.rs` counts continuous VISITS, never per-2s-segment sightings —
  the "seen 62 times" bug), a deterministic distinct-people count, a footage-stats capability
  (`hushai-rag/src/stats.rs`: "how many minutes of video today" answered from `segments`
  arithmetic, the LLM never narrates the numbers), a window conversation summary ("what have
  we spoken about today" → gap-grouped conversations, not 2s-snippet top-k), natural-language
  windows ("last 10 minutes") on the People/Objects/Plates/Events arms, and session hygiene
  (viewer per-tab `sessionStorage` pointer + stale-restore guard; server LLM-visible history
  age filter `RAG_CHAT_HISTORY_MAX_AGE_SECS`; spoken "new chat"/"start over" reset on the
  voice client).
- **Entity profiles ("running memory", migration 0024)** — every person/speaker identity
  accumulates an append-only observation log (one line per coalesced visit/conversation),
  folded deterministically from the sessionized `events` table (worker drain pass + RAG
  chat-time freshen); anonymous identities accumulate too, and naming/merging attaches the
  history ("tell me about <name>" narrates it). `hushai-backend/src/profiles.rs`.
- **Speaker retro-attach** — naming/owning a voice (and a worker drain pass) now pulls in its
  unattributed history on multi-vector evidence (>=2 raw neighbors within the existing 0.5
  match distance) and folds anonymous duplicates at the 0.15 auto-heal bar; nothing loosened
  online. `POST /v1/speakers/{id}/retro-attach`; `hushai-backend/tests/retro_attach.rs`.
- **Guided voice enrollment (Android)** — six prompted samples with per-sample gates + a
  MANDATORY held-out voice check before the multi-vector profile replaces the old one
  (top-2-mean verification at the unchanged 0.5 gate; guarded >=0.70 adaptation slots). The
  old single-5s-centroid unconditional commit was the "forgets my voice next session" bug.
  Headless harness extras (`--ez assistant/enroll`) + reject logging for the acoustic matrix.
- **Test campaign plumbing** — `ChatQ.session` threading (multi-turn + cross-session-isolation
  fixtures), new fixtures `chat_sessions` / `script_long` / `jfk_long` / `visit_coalesce`,
  physical tier moved to its own `hushai_test_phys` DB + ports (`local_dev/phys.env`) with a
  phone lockfile, and the voice-assistant acoustic matrix
  (`local_dev/build_voice_matrix.sh` + `local_dev/voice_assistant_loop.py`).

- **Cross-platform onboarding** — `local_dev/onboard.sh` (interactive fresh-machine front door:
  asks what you're connecting + which AI lanes, installs deps, provisions models, mints per-device
  tokens, launches, connects devices) on top of a new `local_dev/lib_platform.sh` adapter layer
  (OS detect, pkg install, Postgres start, LAN-IP, CA trust, dynamic-linker path) sourced by
  `run_stack.sh` / `serve.sh` / `gen_certs.sh`. `run_stack.sh` now self-tears-down a prior run on
  start; `serve.sh` grew a Linux LAN path.
- **Repo streamlining** — restructured `AGENTS.md` from a 1000-line dated changelog into a durable
  orientation doc (this changelog is the extracted history); added `REVIEW.md`, `hushai-worker` /
  `hushai-rag` READMEs, and a migrations index; removed unused dependencies (`thiserror` from
  worker/rag, `chrono` from backend/eval, `tower` from backend, `tracing` facade from eval);
  untracked regenerable artifacts (`local_dev/.feed_work/`, a stray root `data/` blob) and removed
  unrelated book-project scratch files.

## 2026-07-01

- **ALPR working end-to-end (verified).** Provisioned the plate detector
  (open-image-models `yolo-v9-t-640-license-plate`, MIT ONNX) + fast-plate-ocr CCT export; read a
  real plate "EMD774" (exact) → minted. Baked the load-bearing decode facts into `plates/detect.rs`
  / `plates/ocr.rs`: END2END detector output `[N,7]`, centered 114-gray letterbox, UINT8 OCR input
  dtype auto-detect, fixed-length softmax CCT head, whole-frame scan fallback.
- **RAG-chat ANSWER scoring in `hushai-eval`.** The harness now scores the chat answer (the
  PI-workflow layer), not just perception: `chat`/`rag`-modality fixtures carry
  `Expected.chat.questions[]` with deterministic assertions (`must_contain` / `expect_number` /
  `expect_routed_agent` / `min_citations` / `citation_must_attribute`) + Info-only cosine/judge.
  Drove RAG fixes now live: `presence.rs` (deterministic counts/first-last/rhythm), the SSE
  `session` event carries `routed_agent_id`, deterministic co-occurrence, and `RAG_QUERY_CONDENSE`
  multi-turn follow-up condensation. New fixtures `repeat_visitor`, `money_talk`.

## 2026-06-30

- **SCRFD is the default face detector** (`FACE_DETECTOR_KIND=scrfd`), YuNet the fallback — fixed
  the live phone-at-screen small-face miss (a screen face YuNet read as 0 detections SCRFD reads as
  3). Face detection is now a `detect::FaceDetect` trait.
- **Object class decode fixed (COCO-91).** The RF-DETR export is `labels[1,300,91]`; the class-logit
  column index *is* the COCO category id. The old dense-COCO-80 mapping mislabeled every detection
  (person → "bicycle"). `objects.rs` now maps via the canonical COCO-91 layout + loads
  `rf-detr-classes.json`. Added class-aware NMS (`geom::nms_by` per label; RF-DETR's 300-query head
  emits duplicate boxes).
- **Vision provisioning** — `local_dev/provision_vision.sh` (isolated venv) exports RF-DETR + CLIP;
  object + face lanes both enabled.
- **Physical camera-at-screen test tier (Tier 2)** — `local_dev/physical_loopback.py` plays a clip
  fullscreen while the USB phone captures it through the live pipeline, scored tolerantly.
- **Speaker-on-physical-audio investigated (do NOT blind-tune):** real room audio mints 0 speakers
  because segments are `AttachOnly` (voiced_frac and snr_db both below gates). An adversarial review
  showed lowering the gates would make a TV/laptop playing dialogue mint a spurious speaker. Safe
  calibration needs a labeled real-capture set.

## 2026-06-29

- **VSaaS pillar A1–A7 COMPLETE** — the proactive layer: `events` table + producer
  (`events_producer.rs`, sessionized), `alert_rules` + evaluator (`alerts.rs`, tz windows /
  cooldown / idempotent fan-out), webhook delivery loop (`delivery.rs`, crash-safe lease + backoff,
  at-least-once, HMAC signing, SSRF posture), viewer Events/Alerts page, **watchlists** (managed
  alert rules that survive merges), and **Android push** (`AlertNotifier.kt`, FCM-free feed poll).
  Migrations 0014–0019. Hardened from adversarial reviews.
- **Observability (B1/B5)** — dependency-free Prometheus exporter (`observe.rs`) shared by all four
  binaries; `/metrics` everywhere, `/readyz` on backend/rag/viewer; cardinality guard on labels.
- **Logging (B2-ish)** — one shared `logging.rs`: `LOG_FORMAT=json`, daily-rotated `LOG_DIR`,
  per-request `request_id` correlation, panic-capture hook.
- **Audit log (B6)** — append-only `audit_log` (migrations 0017/0018, UPDATE-blocking trigger)
  written at the viewer gateway with the real client IP.
- **`hushai-eval` Tier 1 built + verified** — the deterministic file-injection regression harness.
  First recursive-testing win: found AND fixed a speaker-lane bug (sherpa Silero VAD needs
  512-sample windowed `accept_waveform`, not one call; post-VAD speech went 0.31s → 4.41s on a 14s
  clip).

## 2026-06-28

- **LAN security model** — native rustls TLS on all three services (`tls.rs`, opt-in by env,
  aws-lc-rs provider), viewer admin plane (IP allowlist + argon2 password + HMAC session cookie),
  per-device tokens (`DEVICE_TOKENS` HashMap, constant-time compare), rag `RAG_TOKEN`, friendly
  `https://hushai.local/` via `setup_hostname.sh` + `run_stack --lan` + `serve.sh`.
- **Device & footage management** — rename/usage/retention/delete/export; `devices.rs` (migration
  0011) + a viewer Files page; deletion safety (NULL the NO-ACTION FKs under advisory locks; blobs
  reclaimed after commit via `storage::reclaim_blobs`, content-hash re-checked).
- **Vision image cleanup + ALPR built** — the detect → crop → super-res/restore/deskew → recognize
  cascade (`enhance.rs`) for faces and vehicles; ALPR lane (`vision/plates/`). Migrations 0012/0013.

## 2026-06-26

- **Speaker de-duplication overhaul** — fixed "background static fragments one person into many
  unknown-speaker rows." Three layers: (1) real Silero VAD strips static before embedding; (2)
  identity model replaced a single drifting centroid with a multi-vector k-NN vote + mint-guard
  hysteresis + quality gate (Mint/AttachOnly/Reject) + self-healing centroid, `assign_speaker`
  returns `Option<Uuid>`; (3) raw-embedding-level clustering + heal/auto-merge endpoints +
  "Clean up voices" UX. Migration 0007 (per-partition HNSW + `quality`).
- **Vanishing-voice fix** — a frequent speaker never appeared in the Voices list. Root cause was
  operational + a backfill gap: self-healing speaker backfill (`reconcile_missing_speaker_segments`,
  `SPEAKER_BACKFILL_ON_START`), reject tombstones so restart doesn't re-queue forever, a
  `GET /v1/speakers/unattributed`(+`/name`) human-in-the-loop path, chat de-collapsing of distinct
  unnamed speakers, a launchd-supervised worker, rolling-window speaker embedding (`select_window`),
  and gate calibration for real phone audio.
- **Vision pipeline Phase A — face identity** — the worker gained "eyes": a vision path
  (`media_type IN (2,3)`) → YuNet+ArcFace → `persons`/`person_segments` (migration 0009), a
  near-verbatim mirror of the speaker system with a different advisory-lock key. Proved ort/sherpa
  ONNX-Runtime coexistence (1.20 via `load-dynamic` alongside sherpa's bundled 1.17.1). Later phases
  (B objects, C/D persons API + Android, E person attribution) built on top.

## 2026-06-24

- **RAG / chunk-storage scalability** (migration 0003) — recreated `transcript_sentences` as a
  monthly RANGE-partitioned table with per-partition HNSW; denormalized `device_id` so filtered
  search uses the ANN index (fixed the recall cliff); indexed `segment_id`; batched inserts;
  removed the idle full-table backfill scan; added retention/partition helpers. Single-tenant by
  design.
- **Capacity follow-ups** — native binary vector encoding (sqlx 0.9 + pgvector 0.4.2, no more
  `::vector` text literals; behavior-neutral); `EMBED_OLLAMA_BASE_URL` / `LLM_OLLAMA_BASE_URL`
  endpoint split; scheduled partition maintenance (`local_dev/partition_maintenance.{sh,pg_cron.sql}`
  + launchd).
- **Worker container handling** (`media.rs`) — prepend `codec_init_data` only when
  `container == "fmp4"`; the Android client uploads self-contained MP4s and prepending corrupted
  them ("moov atom not found"). Android audio now transcribes + is RAG-retrievable end-to-end.
