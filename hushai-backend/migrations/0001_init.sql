-- Hushai data-intake schema — phase 1.
--
-- The FULL schema is created now (including empty-but-ready vector tables) so we
-- never need a migration when local transcription / vision / embedding models
-- arrive. Phase 1 writes ONLY devices / sessions / streams / segments; the four
-- vector tables exist but stay empty until the embedding-pipeline ticket.
--
-- Conventions:
--   * uint64 proto fields (sequence, *_nanos, byte_len) -> Postgres bigint (i64).
--     Documented cast at the decode boundary in src/proto.rs.
--   * media_type stored as the proto enum's integer value (never branched on).
--   * source_kind is stored but NEVER drives backend logic (contract §7).
--   * NO HNSW/IVFFlat indexes in phase 1 (tables empty); indexing ships with the
--     embedding-pipeline ticket.

CREATE EXTENSION IF NOT EXISTS vector;

-- ---------------------------------------------------------------------------
-- Phase-1 tables (written on every accepted segment)
-- ---------------------------------------------------------------------------

CREATE TABLE devices (
    device_id   text PRIMARY KEY,
    source_kind text        NOT NULL,                       -- descriptive only (§7)
    first_seen  timestamptz NOT NULL DEFAULT now(),
    last_seen   timestamptz NOT NULL DEFAULT now(),
    attrs       jsonb       NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE sessions (
    session_id uuid PRIMARY KEY,
    device_id  text        NOT NULL REFERENCES devices (device_id),
    started_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE streams (
    session_id      uuid    NOT NULL REFERENCES sessions (session_id),
    stream_id       text    NOT NULL,
    device_id       text    NOT NULL REFERENCES devices (device_id),
    media_type      integer NOT NULL,                       -- hushai.v1.MediaType value
    codec           text    NOT NULL,
    container       text    NOT NULL,
    codec_init_data bytea,
    PRIMARY KEY (session_id, stream_id)
);

CREATE TABLE segments (
    segment_id               uuid PRIMARY KEY,              -- 16-byte UUIDv7; global idempotency key
    device_id                text    NOT NULL REFERENCES devices (device_id),
    stream_id                text    NOT NULL,
    session_id               uuid    NOT NULL,
    sequence                 bigint  NOT NULL,              -- monotonic per (session_id, stream_id) from 0
    media_type               integer NOT NULL,
    codec                    text    NOT NULL,
    container                text    NOT NULL,
    codec_init_data          bytea,
    capture_start_unix_nanos bigint  NOT NULL,              -- raw device wall clock
    monotonic_start_nanos    bigint  NOT NULL,              -- raw device monotonic clock
    duration_nanos           bigint  NOT NULL,
    content_sha256           bytea   NOT NULL,              -- 32 bytes
    byte_len                 bigint  NOT NULL,
    gap_before               boolean NOT NULL DEFAULT false,
    blob_uri                 text    NOT NULL,              -- e.g. file://.../blobs/ab/cd/<sha256>
    storage_backend          text    NOT NULL,              -- e.g. "file" (swap to "s3" later, no schema change)
    attrs                    jsonb   NOT NULL DEFAULT '{}'::jsonb,
    received_at              timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (session_id, stream_id) REFERENCES streams (session_id, stream_id),
    UNIQUE (session_id, stream_id, sequence)               -- gap / duplicate detection
);

CREATE INDEX segments_session_stream_seq_idx
    ON segments (session_id, stream_id, sequence);

-- ---------------------------------------------------------------------------
-- Empty-but-ready vector tables (NO writes in phase 1 — created so we never migrate)
--
-- Embedding dims: vector(1024) for text/summary, vector(512) for face/object.
-- Always store embedding_model + embedding_dim so multiple model generations
-- coexist; a future model needing a different dim gets an ADDITIVE new column,
-- never a destructive migration. (HNSW caps at 2000 dims; use halfvec later if
-- ever needed.)
-- ---------------------------------------------------------------------------

CREATE TABLE transcript_sentences (
    id              bigserial PRIMARY KEY,
    segment_id      uuid REFERENCES segments (segment_id),
    text            text,
    start_unix_nanos bigint,
    end_unix_nanos   bigint,
    sentiment       text,
    emotion         text,
    embedding       vector(1024),
    embedding_model text,
    embedding_dim   integer
);

CREATE TABLE video_events (
    id              bigserial PRIMARY KEY,
    segment_id      uuid REFERENCES segments (segment_id),
    scene_label     text,
    vibe            text,
    disposition     text,
    embedding       vector(1024),
    embedding_model text,
    embedding_dim   integer
);

CREATE TABLE scene_objects (
    id              bigserial PRIMARY KEY,
    segment_id      uuid REFERENCES segments (segment_id),
    object_label    text,
    person_id       text,
    action          text,
    bbox            jsonb,
    embedding       vector(512),
    embedding_model text,
    embedding_dim   integer
);

CREATE TABLE rolling_summaries (
    id                    bigserial PRIMARY KEY,
    window_start_unix_nanos bigint,
    window_end_unix_nanos   bigint,
    granularity           text,
    summary_text          text,
    embedding             vector(1024),
    embedding_model       text,
    embedding_dim         integer
);
