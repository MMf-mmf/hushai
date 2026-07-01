# Feature-parity initiative — status & handoff

**Goal (from the `/loop` brief):** greater feature parity with industry-giant VSaaS platforms
(Verkada, Rhombus, Eagle Eye, Genetec) **and** cloud-native operability — without giving up Hushai's
local-first / no-egress posture.

**Status as of this checkpoint:** the **VSaaS pillar is complete (A1–A7)**; the **cloud-native pillar
has its observability + audit foundation (B1/B5/B6)**; the remaining cloud-native items (B2/B3/B4) are
designed but gated on an infrastructure-capable host. This doc is the handoff index — the full backlog
with checkboxes lives in [`feature-parity-roadmap.md`](feature-parity-roadmap.md); per-subsystem
architecture detail lives in the dated sections of [`../AGENTS.md`](../AGENTS.md).

> **Working state:** all of the below is **uncommitted** on branch `feat/hushai-voice-assistant`
> (nothing has been committed during the initiative). The stack runs locally; nothing is deployed.

---

## What shipped (built · verified · adversarially reviewed)

Every item below was built **understand → implement → verify → adversarial-review**; the review was a
multi-agent workflow whose findings were fixed and re-verified (100+ findings across the initiative).

### Pillar A — VSaaS proactive alerting ✅ COMPLETE

| # | Feature | Where | Verified by |
|---|---------|-------|-------------|
| A1 | **Events engine** — sessionized events from detections (`known_person`/`unknown_person`/`plate_of_interest`/`plate_seen`/`object_seen`/`speech`); `GET /v1/events` | migration `0014`; `hushai-backend/src/events.rs` | live HTTP + SQL (UPSERT dedup) |
| A2 | **Alert-rules engine** — event-type/camera/subject/severity + local time-of-day window + day-of-week + cooldown + channels; CRUD + feed/ack | `0014`; `events.rs` | live API matrix |
| A3 | **Producer + evaluator** — worker emits events from both lanes; single-SQL evaluator (tz windows incl. midnight-wrap, cooldown, channel fan-out, `ON CONFLICT` idempotent) | `0015`; `hushai-worker/src/events_producer.rs`, `alerts.rs`; hooks in `process.rs` + `vision/write.rs` | SQL matrix (10 cases) + cooldown + idempotency |
| A4 | **Notification delivery** — crash-safe outbox loop → outbound **webhook** POST (lease + capped backoff, at-least-once + idempotency key, optional HMAC signing, SSRF guard, concurrent batch) | `0016`; `hushai-worker/src/delivery.rs` | integration tests vs a real TCP sink |
| A5 | **Web Events UI** — viewer 🔔 Events page: alert feed + acknowledge, filterable event stream, timeline deep-link (`/?device=&t=`), full alert-rule manager | `hushai-viewer/ui/events.html`, `ui/js/events/events.js`, `api.js`, `app.js` | headless real-Chrome (12 assertions) |
| A6 | **Watchlists** — "People/Plates of Interest": flag a subject → auto-managed alert rule fires on any sighting; survives merges + self-heals | `0019`; `hushai-backend/src/watchlist.rs`; ☆/★ toggle in `ui/js/settings/people.js` + `plates.js` | live backend + headless-Chrome toggle |
| A7 | **Android push + Events screen** — FCM-free: poll `/v1/events/feed` → system notifications (persisted high-water-mark dedupe); on-phone Alerts feed (view + ack) | `hushai-android/.../capture/AlertNotifier.kt`, `net/EventsClient.kt`, `ui/EventsScreen.kt`, nav | unit test + **on-device (Galaxy S8)** dumpsys |

**The chain end-to-end:** detect → event → rule match + cooldown → outbox → in-app feed + web Events UI
+ outbound webhook + Android push.

### Pillar B — cloud-native operability (foundation ✅)

| # | Feature | Where | Verified by |
|---|---------|-------|-------------|
| B1 | **Prometheus `/metrics`** — dependency-free exporter; ingest/queue/events/alerts/deliveries/rag/proxy counters + `build_info`; bounded label cardinality | `hushai-backend/src/observe.rs` (+ per-service instrumentation) | live scrape on all 4 services |
| B5 | **Health/readiness probes** — `/healthz` everywhere; `/readyz` (DB ping) on backend/rag/viewer; worker has a raw `/metrics`+`/healthz` server | `observe.rs`, each service's routes | live (200s) |
| B6 | **Audit log** — append-only `who/what/when/where/outcome` at the gateway (proxy) + login/logout/export; bearer-authed `GET /v1/audit`; DB-enforced immutability | `0017`, `0018`; `hushai-backend/src/audit.rs`; hooks in `hushai-viewer` proxy/auth/export | classify unit test + live drive |

> Note: worker **per-stage latency histograms** + a **30-camera load-test harness + dashboard panel**
> were being added in parallel (separate workstream) — they extend B1; see the `hushai-loadtest`
> memory / `observe.rs` `StageTimer`.

---

## Remaining (designed, not yet built)

### Pillar B — needs an infrastructure-capable host
These were **deferred from the sandbox** because they can't be build-verified here (no Docker daemon;
the worker/rag images link whisper.cpp + sherpa + onnxruntime native libs with gitignored models; no
OTLP collector / MinIO). Run the loop on a Docker-capable host to author + verify them with the same rigor.

- **B3. Docker Compose** — `Dockerfile` per service + `docker-compose.yml` (Postgres+pgvector, Ollama,
  all four services) wired to the new health/readiness probes; a Prometheus scrape config. *Headline
  "cloud native" deliverable.*
- **B4. Object-storage blob backend** — abstract `storage` over S3/MinIO (still self-hostable / no-egress)
  so footage can leave the single node; keep local-FS the default.
- **B2. OpenTelemetry tracing** — OTLP export behind an env flag; request + worker span context.

### Pillar A leftover — verifiable here
- **Scrub-bar event markers** — event ticks on the NVR timeline (jump-to-moment). The next item to pick
  up; frontend, headless-Chrome verifiable.

### Pillar C — stretch / post-parity (not started)
C1 low-latency live view (WebRTC/LL-HLS) · C2 privacy zones + face/region blurring · C3 multi-tenancy +
RBAC (would also give per-user audit actors) · C4 floor-plan/camera map · C5 time-limited share links.

---

## How to resume

Re-run **`/loop based on what we already have done to our app, let's update it to have a greater feature
parity within industry Giants, such as cloud native, and VSAAS`** (or name the item directly). Suggested
order: **scrub-bar event markers** (verifiable here) → then, on a Docker host, **B3 → B4 → B2**. Pillar C
is opportunistic.

## Migrations & key modules added this initiative
- Migrations: `0014` events+alerts · `0015` delivery dedup · `0016` delivery retry clock · `0017` audit_log
  · `0018` audit immutability trigger · `0019` watchlist.
- Backend: `events.rs`, `audit.rs`, `watchlist.rs`, `observe.rs` (+ `ingest.rs`/`routes.rs` wiring).
- Worker: `events_producer.rs`, `alerts.rs`, `delivery.rs` (+ `config.rs` `EVENTS_*`/`ALERT_*`/`WORKER_METRICS_ADDR` knobs, `lib.rs` spawns).
- Viewer: `ui/events.html` + `ui/js/events/events.js`; `proxy.rs`/`auth.rs`/`export.rs` audit + path-allowlist; `api.js`/`app.js` + People/Plates watch toggles; `/metrics`+`/readyz`.
- Android: `net/EventsClient.kt`, `capture/AlertNotifier.kt`, `ui/EventsScreen.kt` (+ nav, `CaptureService` hook, `EventsClientTest`).
