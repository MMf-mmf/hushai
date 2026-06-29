-- 0014_events_and_alerts.sql — the proactive layer: turn raw detections into queryable EVENTS,
-- let operators define ALERT RULES over them, and record every fired notification in a delivery
-- outbox. This is the VSaaS-giant parity feature (Verkada/Rhombus/Eagle Eye are fundamentally
-- "alert me + show me a feed"); Hushai already *detects* everything (faces/plates/objects/speech),
-- it just never materialized events or notified. See docs/feature-parity-roadmap.md, Pillar A.
--
-- DESIGN NOTES (load-bearing — read before touching the event producer or evaluator):
--
--  * `events` is a PLAIN (non-partitioned) table, deliberately UNLIKE person_segments/scene_objects/
--    plate_detections. Those are RAW per-detection rows (one per face/object/read per 2s segment —
--    huge). `events` are SESSIONIZED (one row per continuous appearance — orders of magnitude fewer),
--    so a plain table is fine AND lets us carry a real UNIQUE(dedup_key) for idempotent producers (a
--    partitioned UNIQUE must include the partition key, which would break cross-partition dedup). If
--    event volume ever rivals detections, partition by `created_at` like 0009 — noted in the roadmap.
--    Same precedent as chat_sessions/chat_messages (0008): plain tables, retention by DELETE.
--
--  * IDEMPOTENT PRODUCER: the worker computes a stable `dedup_key` per sessionized event
--    (e.g. "seen:person:<person_id>:<device>:<session_bucket>") and INSERTs with
--    ON CONFLICT (dedup_key) DO UPDATE so re-processing a segment extends an open event's
--    end-time / sample rather than spawning duplicates. NULL dedup_key = never-deduped (rare).
--
--  * EVALUATION (cooldown) lives in `alert_deliveries`: a rule fires at most once per
--    `cooldown_secs` per (rule, subject), enforced by a partial-unique on `dedup_key` + a recency
--    check in the evaluator. The outbox doubles as the in-app feed (status='pending'→shown) and the
--    webhook/push send queue (status drives retry).
--
--  * TYPE CONTRACT: subject_id is `uuid` (person_id / plate_id) but nullable (anonymous person,
--    object, motion). Runtime sqlx binds it; cast at the boundary, same as 0006/0009.

------------------------------------------------------------------------------------------------
-- 1. events: the materialized, queryable event stream. One row per sessionized occurrence.
--
--    event_type vocabulary (free text; documented here, validated in code):
--      person_seen | unknown_person | known_person | vehicle_seen | plate_seen |
--      plate_of_interest | object_seen | speech | motion | system
--    severity: 'info' | 'warning' | 'critical'  (drives alert routing + feed prominence)
------------------------------------------------------------------------------------------------
CREATE TABLE events (
    event_id          uuid PRIMARY KEY,
    device_id         text REFERENCES devices (device_id),
    event_type        text NOT NULL,
    severity          text NOT NULL DEFAULT 'info',
    subject_type      text,                       -- 'person' | 'plate' | 'object' | 'speaker' | NULL
    subject_id        uuid,                       -- person_id / plate_id; NULL = anonymous / N/A
    subject_label     text,                       -- denormalized display (name / plate text / object label)
    segment_id        uuid REFERENCES segments (segment_id) ON DELETE SET NULL, -- deep-link anchor
    start_unix_nanos  bigint NOT NULL,
    end_unix_nanos    bigint NOT NULL,
    score             real,                       -- detection confidence / salience 0..1
    metadata          jsonb NOT NULL DEFAULT '{}'::jsonb,  -- bbox, attrs, source detail
    dedup_key         text,                       -- stable per sessionized event; UNIQUE when present
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now()
);

-- Idempotent producer key. Partial (WHERE NOT NULL) so a NULL key never collides.
CREATE UNIQUE INDEX events_dedup_key_idx ON events (dedup_key) WHERE dedup_key IS NOT NULL;
-- Feed query: newest-first, filterable by device / type / time.
CREATE INDEX events_time_idx        ON events (start_unix_nanos DESC);
CREATE INDEX events_device_time_idx ON events (device_id, start_unix_nanos DESC);
CREATE INDEX events_type_time_idx   ON events (event_type, start_unix_nanos DESC);
CREATE INDEX events_subject_idx     ON events (subject_type, subject_id);

------------------------------------------------------------------------------------------------
-- 2. alert_rules: operator-defined "notify me when…". A rule matches an event when EVERY set
--    filter is satisfied (empty/NULL array or column = "any"), within the optional local-time
--    window, then fires (subject to cooldown) onto the configured channels.
------------------------------------------------------------------------------------------------
CREATE TABLE alert_rules (
    rule_id            uuid PRIMARY KEY,
    name               text NOT NULL,
    enabled            boolean NOT NULL DEFAULT true,
    event_types        text[] NOT NULL DEFAULT '{}',   -- match these types; {} = any
    device_ids         text[] NOT NULL DEFAULT '{}',   -- scope to cameras; {} = all
    subject_type       text,                            -- optional: 'person' | 'plate' | ...
    subject_ids        uuid[] NOT NULL DEFAULT '{}',   -- watchlist (specific persons/plates); {} = any
    min_severity       text NOT NULL DEFAULT 'info',   -- 'info' | 'warning' | 'critical'
    -- Local-time-of-day gate, minutes since local midnight [0,1440). NULLs = always-on. A window
    -- with start > end wraps past midnight (e.g. 1320..360 = 22:00–06:00). tz comes from the rule.
    time_start_minutes integer,
    time_end_minutes   integer,
    days_of_week       integer[] NOT NULL DEFAULT '{}', -- 0=Sun..6=Sat; {} = every day
    tz                 text NOT NULL DEFAULT 'UTC',     -- IANA tz the window/days are evaluated in
    cooldown_secs      integer NOT NULL DEFAULT 300,    -- min seconds between fires per (rule,subject)
    -- Delivery fan-out: [{"type":"feed"},{"type":"webhook","url":"https://…"},{"type":"push"}].
    channels           jsonb NOT NULL DEFAULT '[{"type":"feed"}]'::jsonb,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX alert_rules_enabled_idx ON alert_rules (enabled) WHERE enabled;

------------------------------------------------------------------------------------------------
-- 3. alert_deliveries: the notification outbox + in-app feed. One row per (rule, event, channel).
--    Drives BOTH the feed (status='pending'/'sent' shown until acknowledged) and the send/retry
--    loop for webhook/push. Cooldown is enforced here: the evaluator skips a fire whose
--    (rule_id, subject) fired within cooldown_secs (recency check over this table).
------------------------------------------------------------------------------------------------
CREATE TABLE alert_deliveries (
    delivery_id      uuid PRIMARY KEY,
    rule_id          uuid REFERENCES alert_rules (rule_id) ON DELETE CASCADE,
    event_id         uuid REFERENCES events (event_id) ON DELETE CASCADE,
    channel          text NOT NULL,               -- 'feed' | 'webhook' | 'push'
    status           text NOT NULL DEFAULT 'pending', -- pending | sent | failed | acknowledged
    target           text,                        -- webhook URL / push token (NULL for feed)
    attempts         integer NOT NULL DEFAULT 0,
    last_error       text,
    -- Denormalized event facets so the feed renders without a join and survives event purge.
    device_id        text,
    event_type       text,
    severity         text,
    subject_label    text,
    cooldown_key     text,                        -- "<rule_id>:<subject>" — recency lookup key
    created_at       timestamptz NOT NULL DEFAULT now(),
    sent_at          timestamptz,
    acknowledged_at  timestamptz
);

-- Feed: newest-first, and "what's unacknowledged". Send loop: find pending non-feed deliveries.
CREATE INDEX alert_deliveries_created_idx   ON alert_deliveries (created_at DESC);
CREATE INDEX alert_deliveries_status_idx    ON alert_deliveries (status, channel);
CREATE INDEX alert_deliveries_cooldown_idx  ON alert_deliveries (cooldown_key, created_at DESC);
CREATE INDEX alert_deliveries_rule_idx      ON alert_deliveries (rule_id);
