------------------------------------------------------------------------------------------------
-- worker_heartbeat: liveness for hushai-worker, which has no HTTP port.
--
-- The worker writes/updates ONE row per process (not per tokio loop) every few seconds. The
-- viewer's /api/dashboard reads `last_beat` recency to classify the worker up/down — the only
-- way to distinguish "idle but healthy" from "crashed" (an empty work queue looks the same as a
-- dead worker). Throughput/backlog still come from segment_transcription_status / segment_vision_status.
--
-- IF NOT EXISTS so re-runs are harmless (matches the idempotent DDL style used elsewhere).
------------------------------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS worker_heartbeat (
    worker_id   text        PRIMARY KEY,        -- WORKER_ID env, else "<host>:<pid>"
    instance    text        NOT NULL,           -- hostname label (human-readable)
    pid         integer     NOT NULL,
    version     text        NOT NULL,           -- CARGO_PKG_VERSION of hushai-worker
    started_at  timestamptz NOT NULL DEFAULT now(),  -- preserved across beats (uptime)
    last_beat   timestamptz NOT NULL DEFAULT now(),  -- bumped every WORKER_HEARTBEAT_SECS
    concurrency integer     NOT NULL DEFAULT 1, -- cfg.worker_concurrency (context only)
    queue_depth integer,                        -- self-reported pending+processing at last beat
    meta        jsonb       NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX IF NOT EXISTS worker_heartbeat_last_beat_idx ON worker_heartbeat (last_beat DESC);
