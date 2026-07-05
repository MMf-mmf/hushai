# Changelog

Dated development history for the Hushai workspace. Durable reference (architecture, invariants,
how to run) lives in [`AGENTS.md`](AGENTS.md); this file is the narrative of what changed when.
Newest first. Dates are when the work landed on the current development branch
(`feat/hushai-voice-assistant`).

## Unreleased (in-flight)

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
