-- Speaker identity: anonymous-now / named-later / cross-device, plus the raw
-- per-segment voiceprints the matcher and a future recluster need.
--
-- Three pieces:
--   1. transcript_sentences.speaker_id (denormalized text, like device_id) so the RAG
--      filter sits on the same table as the HNSW index (the 0003 recall-cliff lesson:
--      NEVER filter via a JOIN). NULL = unassigned / silent / multi-speaker / pre-feature.
--   2. speakers: the global, cross-device catalog of distinct voices (running-mean
--      centroid + sample count + human-assigned name). Low row count -> not partitioned.
--   3. speaker_segments: one row per processed speech segment holding the raw 192-dim
--      embedding. This is the durable idempotency source of truth (keyed by segment_id)
--      AND the substrate for the Phase C recluster. RANGE-partitioned by created_at like
--      transcript_sentences, so retention is a cheap partition DROP (the privacy
--      mechanism for raw voiceprints).
--
-- TYPE CONTRACT (load-bearing — these are unchecked runtime queries, no compile-time
-- catch): speakers.speaker_id and speaker_segments.speaker_id are `uuid`, but
-- transcript_sentences.speaker_id is `text` (to match the existing text device_id and
-- avoid a uuid-vs-text `= ANY()` runtime type error in RAG). The worker writes
-- speaker_id.to_string(); RAG binds stringified ids as text[] against the text column,
-- and uuid[] against the uuid columns. Cast accordingly at every boundary.
--
-- ONE ROW PER segment_id in speaker_segments: the table is partitioned by created_at, so
-- a plain UNIQUE(segment_id) is impossible. The worker enforces it inside the write tx
-- with `DELETE FROM speaker_segments WHERE segment_id=$1` then INSERT (delete-then-insert,
-- same idiom as transcript_sentences), so reprocessing never appends duplicate vectors.
--
-- No sentiment/emotion DDL here — those columns already exist (0003). No backfill of
-- speaker_id on pre-feature rows (they stay NULL forever; a name filter never matches
-- NULL, so "what did Bob say" correctly excludes pre-feature history).
--
-- MIGRATION SAFETY: `ADD COLUMN ... NULL` on a partitioned parent is metadata-only, but
-- `CREATE INDEX` on a partitioned parent is NOT concurrent and locks each partition while
-- it builds. Fine for the dev corpus; on a production-sized corpus build the speaker/time
-- index out-of-band (CONCURRENTLY, per partition) before deploy.

-- 1. Denormalized speaker_id on the partitioned transcript table. Parent ALTER + parent
--    index propagate to all existing and future partitions.
ALTER TABLE transcript_sentences ADD COLUMN speaker_id text;
CREATE INDEX transcript_sentences_speaker_time_idx
    ON transcript_sentences (speaker_id, start_unix_nanos);

-- 2. Global speaker catalog (cross-device; first_seen_device_id is metadata only).
--    centroid is vector(192) — TitaNet's own dim, NOT the 1024-d text-embedding space.
--    Centroids are stored already L2-normalized (the worker normalizes before write).
CREATE TABLE speakers (
    speaker_id           uuid PRIMARY KEY,
    centroid             vector(192),
    n_samples            bigint NOT NULL DEFAULT 0,
    display_name         text,
    first_seen_device_id text REFERENCES devices (device_id),
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now()
);

-- Global name -> id resolution (case-insensitive), used by RAG and the backend.
CREATE INDEX speakers_name_idx ON speakers (lower(display_name));

-- 3. Per-segment raw voiceprints, RANGE-partitioned by created_at (mirrors 0003).
--    A partitioned table's PK must include the partition key.
CREATE TABLE speaker_segments (
    id               bigserial,
    segment_id       uuid REFERENCES segments (segment_id) ON DELETE CASCADE,
    device_id        text,
    speaker_id       uuid,                                  -- nullable; NULL = unassigned
    start_unix_nanos bigint,
    end_unix_nanos   bigint,
    embedding        vector(192),
    created_at       timestamptz NOT NULL DEFAULT now(),    -- partition key
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

-- Parent indexes propagate to every partition. segment_id index backs both the
-- ON DELETE CASCADE and the worker's idempotent per-segment lookup/delete (so neither
-- scans every monthly partition); speaker_id index backs merge/recluster repointing.
CREATE INDEX speaker_segments_segment_id_idx ON speaker_segments (segment_id);
CREATE INDEX speaker_segments_speaker_idx    ON speaker_segments (speaker_id);

-- 4. Partition maintenance helpers (clones of the transcript helpers in 0003).
--    ensure_speaker_segment_partitions(): create monthly partitions for the current
--    month through `months_ahead` out (idempotent); call at worker startup + from cron.
CREATE OR REPLACE FUNCTION ensure_speaker_segment_partitions(months_ahead int DEFAULT 2)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    base date := date_trunc('month', now())::date;
    i int;
    start_d date;
    end_d date;
    part_name text;
BEGIN
    FOR i IN 0..months_ahead LOOP
        start_d := (base + (i || ' month')::interval)::date;
        end_d   := (base + ((i + 1) || ' month')::interval)::date;
        part_name := format('speaker_segments_%s', to_char(start_d, 'YYYY_MM'));
        IF NOT EXISTS (SELECT 1 FROM pg_class WHERE relname = part_name) THEN
            EXECUTE format(
                'CREATE TABLE %I PARTITION OF speaker_segments FOR VALUES FROM (%L) TO (%L)',
                part_name, start_d, end_d
            );
        END IF;
    END LOOP;
END;
$$;

--    drop_speaker_segment_partitions_before(): retention — drop whole monthly partitions
--    whose month is strictly before `cutoff`. This is the privacy purge mechanism for
--    raw voiceprints (O(1) DROP vs a mass DELETE + VACUUM). NB: dropping raw vectors does
--    NOT recompute the running-mean speakers.centroid — that lingers until a recluster.
CREATE OR REPLACE FUNCTION drop_speaker_segment_partitions_before(cutoff date)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
    FOR r IN
        SELECT c.relname
        FROM pg_inherits inh
        JOIN pg_class c ON c.oid = inh.inhrelid
        JOIN pg_class p ON p.oid = inh.inhparent
        WHERE p.relname = 'speaker_segments'
          AND c.relname ~ '^speaker_segments_[0-9]{4}_[0-9]{2}$'
          AND to_date(right(c.relname, 7), 'YYYY_MM') < date_trunc('month', cutoff)::date
    LOOP
        EXECUTE format('DROP TABLE %I', r.relname);
    END LOOP;
END;
$$;

-- 5. Create the current + next 3 monthly partitions, then a DEFAULT catch-all. Without
--    at least one partition (or DEFAULT) every INSERT into the parent fails outright.
SELECT ensure_speaker_segment_partitions(3);
CREATE TABLE speaker_segments_default PARTITION OF speaker_segments DEFAULT;
