# Migrations index

Postgres migrations for the whole workspace (the backend owns the schema; the worker shares it via
a path dep). They **auto-apply** on backend/worker startup via `sqlx::migrate!`, in order. Each is
forward-only — add a new numbered file, never edit a shipped one. See the header comment in each
`.sql` for the full rationale.

| # | File | What it does |
|---|------|--------------|
| 0001 | `init` | Phase-1 schema: `devices`, `sessions`, `streams`, `segments` (content-addressed media blobs + metadata). |
| 0002 | `transcription_pipeline` | `transcript_sentences` + the `segment_transcription_status` work queue (transcribe/embed). |
| 0003 | `scalability` | Recreate `transcript_sentences` as monthly RANGE-partitioned with per-partition HNSW; denormalize `device_id` (filtered search uses the ANN index); partition + retention helpers. |
| 0004 | `segment_child_cascade` | `ON DELETE CASCADE` so deleting a segment removes its derived rows (retention / GC / test cleanup). |
| 0005 | `viewer_timeline_index` | Index backing the viewer's windowed-by-device timeline coverage queries. |
| 0006 | `speaker_identity` | `transcript_sentences.speaker_id` (text, denormalized) + `speakers` (`centroid vector(192)`) + `speaker_segments` (monthly-partitioned raw voiceprints). |
| 0007 | `speaker_segment_knn` | Per-partition HNSW (`vector_cosine_ops`) on `speaker_segments.embedding` (multi-vector k-NN match + raw-level heal) + a `quality` column. |
| 0008 | `chat_sessions` | `chat_sessions` + `chat_messages` — persistent multi-turn RAG chat history (PLAIN tables, not partitioned). |
| 0009 | `person_vision` | `persons` + `person_segments` (partitioned + HNSW, mirror of speakers) and `scene_objects` (CLIP-space objects). |
| 0010 | `worker_heartbeat` | `worker_heartbeat` liveness row (the worker has no HTTP port; the dashboard reads it). |
| 0011 | `device_management` | `devices.display_name` (human name) + `devices.retention_days` (per-device retention policy). |
| 0012 | `face_crops` | `person_segments.{crop_uri,is_best_shot,restored,yaw,pitch,quality_score}` — persist the cleaned/restored best-shot crop + recognition provenance. |
| 0013 | `license_plates` | ALPR: `license_plates` catalog (pg_trgm + fuzzystrmatch, unique `plate_text_norm`) + monthly-partitioned `plate_detections`. |
| 0014 | `events_and_alerts` | The proactive layer: `events` (PLAIN, `UNIQUE(dedup_key)`), `alert_rules`, `alert_deliveries` (outbox + in-app feed). |
| 0015 | `alert_delivery_dedup` | `UNIQUE(rule_id,event_id,channel)` (idempotent fan-out) + `alert_deliveries.event_id` → `ON DELETE SET NULL`. |
| 0016 | `alert_delivery_attempt` | `next_attempt_at` + partial due-index — the retry clock for the crash-safe webhook delivery loop. |
| 0017 | `audit_log` | Append-only `audit_log` of admin/operator actions (roadmap B6). |
| 0018 | `audit_log_immutable` | `BEFORE UPDATE` trigger making `audit_log` entries tamper-evident (INSERT-only). |
| 0019 | `watchlist` | `watchlist` — "People/Plates of Interest" (each entry owns a managed `alert_rules` row). |
| 0020 | `queue_claim_indexes` | `segments(capture_start_unix_nanos)` + partial claimable indexes so the oldest-first claim stays cheap (perf only, no behavior change). |
| 0021 | `archive_catalog_entities` | `archived_at` on `speakers`/`persons`/`license_plates` — the "Disregard" (archive) labeling action. |
| 0022 | `skipped_status_and_hints` | `skipped` becomes a first-class TERMINAL status (+ `skip_reason`) on both AI work queues (backs the ingest hint gate + content gates). |
| 0023 | `owner_identity` | `is_owner` on `speakers`/`persons` + a partial-unique single-owner index — the "This is me" tap. |
| 0024 | `entity_profiles` | Running-memory profiles per person/speaker: append-only observation log folded incrementally from `events` (worker drain pass + RAG chat-time freshen); merge hooks fold duplicates. DERIVED/rebuildable. |
| 0025 | `conversations` | Persisted conversation threading: `conversations` catalog + `conversation_id`/`turn_index` on `transcript_sentences` + `threader_state` watermark. Assigned by the worker-0 batch threader (gap blocks + same-mic disentanglement); open=provisional / closed=frozen / NULL=gap-heuristic fallback. |
| 0026 | `advisor_books` | Ahithophel advisor corpus: `books` + `book_chapters` (raw AND clean text + routing synopsis) + `book_chunks` (`vector(1024)` + HNSW). Populated by `hushai-advisor`'s idempotent `ingest-book` binary; PLAIN tables (curated content, never retention-dropped). |
| 0027 | `advisor_sessions` | Ahithophel advisor consultations: `advisor_sessions` (phase machine gathering/answering/done + followup_rounds + refined_question), gap-free-seq `advisor_messages` (`kind` = message/followup_questions/final_answer, `chapters` jsonb citations), and `advisor_memories` (embedded Q&A summaries, `vector(1024)` + HNSW). |
| 0028 | `entity_graph` | Gotham intelligence layer: `entity_edges` (co_present/conversed_with/arrived_with_vehicle/same_identity_candidate/visits_place; text endpoints, NO FK; canonical undirected ordering; evidence jsonb; binding review-queue `status`) + `graph_state` watermark singleton. Folded deterministically from `events` + closed `conversations` by `graph_pass`. DERIVED/rebuildable. |
| 0029 | `entity_baselines_digests` | Gotham Wave 2 schema (populated later): `entity_baselines` (168 hour-of-week histogram, dwell p50/p90, device/companion stats, recomputed over a trailing window) + `daily_digests` (structured `sections` facts + deterministic `rendered_text`; LLM narrates at read time). DERIVED/rebuildable. |
| 0030 | `entity_journeys` | Gotham Wave 4 schema (stitched later): `entity_journeys` (cross-camera hop chains, ≥ 2 distinct devices, open/closed like conversations) + `camera_adjacency` VIEW over observed hop transitions. DERIVED/rebuildable. |

**Adding one:** create `NNNN_short_name.sql` with a header comment; if it changes a backend `db.rs`
`query!` macro, re-run `cd hushai-backend && DATABASE_URL=… cargo sqlx prepare -- --lib` and commit
the `.sqlx/` change (see AGENTS.md "Build/run gotchas"). Retention/partition maintenance is scheduled
separately via `local_dev/partition_maintenance.{sh,pg_cron.sql}`.
