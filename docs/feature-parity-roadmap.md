# Feature-parity roadmap — VSaaS giants + cloud-native

**Goal:** close the gap between Hushai and industry-giant VSaaS platforms (Verkada, Rhombus,
Eagle Eye Networks, Genetec, Avigilon Alta) and cloud-native operational expectations, **without
giving up the local-first / no-egress posture** that is Hushai's differentiator.

This doc is the burn-down backlog. It is maintained as work lands — each phase links to the
migration / modules that implement it. Where Hushai is already *ahead* of the giants, that's
called out so we don't re-build what we have.

---

## Where we already match or beat the giants

| Capability | Giants | Hushai today |
|---|---|---|
| Multi-camera ingest, exactly-once | ✅ | ✅ content-addressed segment ingest (`hushai-backend`) |
| Scrubbable cloud NVR + playback | ✅ | ✅ HLS timeline w/ PROGRAM-DATE-TIME (`hushai-viewer`) |
| AI: people / faces | ✅ | ✅ YuNet+ArcFace identity catalog (`persons`) |
| AI: vehicles / LPR | ✅ (add-on) | ✅ ALPR lane (`license_plates`) |
| AI: object detection | ✅ | ✅ CLIP open-vocab (`scene_objects`) |
| **Natural-language search over footage** | ⚠️ limited ("smart search" keywords) | ✅✅ **RAG chat over recordings — we are ahead** |
| Speaker ID / audio analytics | ✗ rare | ✅✅ voiceprints + sentiment + reflection — **ahead** |
| Mobile app | ✅ | ✅ Android capture + assistant |
| Retention policies | ✅ | ✅ per-device keep-last-N-days |
| Device health dashboard | ✅ | ✅ System dashboard |
| TLS / auth / per-device creds | ✅ | ✅ LAN TLS + IP allowlist + per-device tokens |

## Where we are behind — the parity gap

The giants are fundamentally **proactive** ("tell me the instant something matters") and
**operable at cloud scale**. Hushai is reactive (you must ask) and single-node. Two pillars:

---

## Pillar A — VSaaS: proactive events, alerts & notifications

This is the single biggest product gap. We already *detect* everything (faces, plates, objects,
speech); we just never turn detections into **events**, evaluate **rules**, or **notify**.

- [x] **A1. Events engine** ✅ — sessionized events from detections; `GET /v1/events`. → migration `0014`, `hushai-backend/src/events.rs`.
- [x] **A2. Alert-rules engine** ✅ — rules (event-type + camera + subject watchlist + severity +
      time-of-day/day-of-week window + cooldown + channels); CRUD API + feed/ack. → `0014`, `events.rs`.
- [x] **A3. Rule evaluation + producer** ✅ — worker `events_producer.rs` emits events from both lanes;
      `alerts.rs` single-statement evaluator (tz window, cooldown, channel fan-out, idempotent via
      `ON CONFLICT`). → migration `0015`, hooks in `process.rs` + `vision/write.rs`. *Adversarially reviewed.*
- [x] **A4. Notification delivery** ✅ — `hushai-worker/src/delivery.rs`: a crash-safe outbox loop
      drains `alert_deliveries` → outbound **webhook** POSTs (retry+backoff, at-least-once w/ idempotency
      key, optional HMAC signing, SSRF guard, concurrent batch). → migration `0016`. In-app feed = A5;
      **mobile push** = A7; email = future. *Integration-tested + adversarially reviewed.*
- [x] **A5. Web event feed UI** ✅ — viewer "🔔 Events" page (`events.html` + `js/events/events.js`):
      alerts feed w/ acknowledge, filterable event stream (camera/type/min-severity), deep-link to the
      timeline at the event instant, and a full alert-rule manager (create/enable-disable/delete).
      *Headless-Chrome verified + adversarially reviewed.* Remaining: **scrub-bar event markers** (deferred).
- [x] **A6. Watchlists / "of interest"** ✅ — flag a person or plate as watched → a managed
      `alert_rule` scoped to it fires on any sighting (reuses the A3 evaluator). `watchlist` table
      (migration `0019`) + `hushai-backend/src/watchlist.rs` + a ☆/★ toggle on the People & Plates
      modals. Self-heals if the rule is deleted; survives person/plate merges (reconciles in the merge
      tx). *Backend + headless-UI verified + adversarially reviewed.* (Verkada "People of Interest".)
- [x] **A7. Android push + event feed screen** ✅ — FCM-free: `AlertNotifier` polls `/v1/events/feed`
      and raises system notifications (hosted by the always-on capture service); an `EventsScreen`
      mirrors the web feed (view + acknowledge). `EventsClient` + nav. Dedupe by a persisted server-time
      `created_unix_nanos` high-water mark so alerts during downtime aren't lost on restart.
      *Built, unit-tested, and VERIFIED ON THE REAL DEVICE (Galaxy S8) + adversarially reviewed.*

## Pillar B — cloud-native operability

Make the stack observable, packaged, and horizontally honest — the table stakes for "cloud native."

- [x] **B1. Observability: Prometheus metrics** ✅ — dependency-free exporter (`hushai-backend/src/observe.rs`):
      `/metrics` on all four services (axum for backend/rag/viewer; a raw-tokio server for the
      port-less worker). Instrumented: ingest rate+bytes, per-lane queue depth, segments processed,
      events/alerts/deliveries, RAG requests, viewer proxy fan-out, `build_info`. Label cardinality
      bounded (no device-controlled free text as labels). *Live-verified + adversarially reviewed.*
- [ ] **B2. Structured tracing / OpenTelemetry** — OTLP export behind an env flag; request + worker
      span context. (We already use `tracing`; add the exporter + spans.)
- [ ] **B3. Containerization** — `Dockerfile` per service + `docker-compose.yml` (Postgres+pgvector,
      Ollama, all four services) so the whole stack is `docker compose up`. Multi-arch. **NOTE: needs a
      Docker-capable host to author+verify (worker/rag images link whisper.cpp+sherpa+onnxruntime native
      libs + gitignored models); deferred from the sandbox where Docker is unavailable.**
- [ ] **B4. Object-storage blob backend** — abstract `storage` over an `S3`/MinIO backend (still
      self-hostable / no-egress via MinIO) so footage can live off the single node. Keep local FS default.
- [x] **B5. Health/readiness probes** ✅ — `/healthz` on all four (worker via its raw metrics server);
      `/readyz` (DB ping) added to rag + viewer (backend already had it). k8s-ready. *(Graceful drain
      already exists via the TLS serve helper's shutdown signal.)*
- [x] **B6. Audit log** ✅ — append-only `audit_log` (migrations `0017`+`0018`, DB-enforced no-UPDATE),
      written at the GATEWAY (viewer proxy) for all mutating admin actions + login/logout + footage
      export, with `who/what/when/where(ip)/outcome` and a bearer-authed `GET /v1/audit`. Position-aware
      action classifier (`hushai-backend/src/audit.rs`). *Unit-tested + live-verified + adversarially
      reviewed.* Known gap: a direct call to the backend port bypasses the gateway (documented).
- [ ] **B7. Public API + API keys** — documented, versioned, key-scoped REST surface for integrators
      (the webhook's inbound twin). OpenAPI spec.
- [ ] **B8. Helm chart / k8s manifests** — once B3/B4 land, package for a cluster.

## Pillar C — stretch / differentiators (post-parity)

- [ ] **C1. Low-latency live view (WebRTC / LL-HLS)** — sub-second live, vs today's segment latency.
- [ ] **C2. Privacy zones + face/region blurring** — GDPR; redact on export.
- [ ] **C3. Multi-tenancy + RBAC + sites/orgs** — only if the product needs multi-org; heavy.
- [ ] **C4. Floor-plan / camera map view.**
- [ ] **C5. Time-limited share links** — share a clip without an account.
- [~] **C6. Intelligence layer (entity graph · link analysis · anomaly baselines · agentic
  "Detective") → [`design/gotham.md`](design/gotham.md).** Wave 1 / Pillar G1 data layer landed
  (migrations 0028–0030, `graph.rs`/`graph_pass.rs`/`graph_api.rs`) + its deterministic eval
  `graph` modality (harness built + SQL-validated; F1–F3 in staging pending rig calibration →
  train). Later waves add baselines/anomalies (G2), the tool-calling runtime (G3), UI + voice
  (G4), journeys (G5).

- [ ] **C7. Custom-trained detection + edge deployment → [`design/osprey.md`](design/osprey.md).**
  Our own detectors trained on our own labeled footage (`hushai-train/`), a human-in-the-loop
  Label mode + hard-example queue, a bbox-level `detections` eval modality (mAP, AP-small,
  recall at the operating point), an additive `CUSTOM_DET_*` lane in the worker, and quantized
  on-device inference on the phone + a dedicated edge box. New capability lanes: wildlife,
  small/far objects, smoke/fire.

---

## Sequencing

A (VSaaS events/alerts) first — highest product value, builds directly on detection data we
already have. Then B (cloud-native ops) to make it deployable and observable. C is opportunistic.

Order within A: **A1 → A2 → A3 → A5 → A4 → A6 → A7** (schema+API, then producer/eval, then the
visible feed, then push the deliveries out, then watchlists, then mobile).
</content>
</invoke>
