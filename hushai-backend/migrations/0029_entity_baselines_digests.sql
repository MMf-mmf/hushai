-- 0029_entity_baselines_digests.sql — pattern baselines + daily briefing store (Gotham.md §1.3).
--
-- Shipped in Wave 1 so migration numbering is settled; POPULATED in Wave 2 by
-- hushai-backend::patterns (baseline recompute over a trailing window; anomalies emitted as
-- ordinary `events` rows; digest facts rendered by a deterministic template — no LLM at write
-- time, the RAG service narrates at read time).
--
-- Baselines get their own table — NOT an entity_profiles (0024) extension: profiles are
-- append-only narrative; baselines are structured and *recomputed* over a trailing window.
--
-- DERIVED DATA: rebuildable from events + entity_edges. Deleting a subject deletes its baseline
-- in the same transaction (§3 posture).
CREATE TABLE entity_baselines (
    subject_type      text NOT NULL CHECK (subject_type IN ('person','speaker','plate')),
    subject_id        uuid NOT NULL,
    window_days       integer NOT NULL,
    -- 168 hour-of-week buckets, local civil time via the existing fixed-offset convention.
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
