-- Vision: person/face identity (anonymous-now / named-later / cross-device) + the raw
-- per-face templates the matcher needs, PLUS an objects-only scene_objects recreation for
-- open-vocabulary object retrieval, PLUS a separate vision work-queue status table.
--
-- This is the VISUAL SIBLING of 0006_speaker_identity.sql and mirrors it deliberately:
--   * persons          ~ speakers          (global cross-device catalog; running-mean centroid)
--   * person_segments  ~ speaker_segments  (raw per-observation templates; idempotency + k-NN substrate)
--   * a new media-type-specific work queue (segment_vision_status ~ segment_transcription_status)
-- so a vision failure and an ASR failure are claimed/retried independently. Faces reuse the entire
-- speaker match-or-mint playbook (advisory-locked, multi-vector k-NN vote, mint-guard hysteresis,
-- self-healing centroid) — see hushai-worker/src/speaker_match.rs.
--
-- TWO 512-d SPACES (load-bearing design decision): ArcFace FACE embeddings and CLIP OBJECT
-- embeddings are DIFFERENT vector spaces. A single HNSW index over both would return garbage
-- nearest-neighbors (a face query "matching" an object). So FACES live in person_segments
-- (vector(512) = ArcFace) with their own HNSW, and OBJECTS live in scene_objects
-- (vector(512) = CLIP image) with their own HNSW. This DIVERGES from 0001_init.sql, which put
-- person_id AND object_label in one scene_objects table — that single-space assumption is wrong
-- once objects are CLIP-embedded. scene_objects is empty today, so a recreate is the cheapest
-- path (the 0003 rationale: "the table is tiny / regenerable").
--
-- TYPE CONTRACT (load-bearing — runtime queries, no compile-time catch, mirrors 0006):
-- persons.person_id and person_segments.person_id are `uuid`; the RAG/denormalized filter binds
-- stringified ids as text[] only where it filters a text column. Cast accordingly at every boundary.
--
-- MANY ROWS PER segment_id (multiple faces / objects per segment) — unlike speaker_segments
-- (one voice per segment). Idempotency is delete-by-segment-then-insert (the transcript_sentences
-- idiom), enforced by the worker inside the vision write tx, NOT a UNIQUE(segment_id).
--
-- MIGRATION SAFETY: CREATE INDEX on a partitioned parent is NOT concurrent and locks each
-- partition while it builds. Fine for these empty dev tables; on a production-sized corpus build
-- the HNSW/time indexes out-of-band (CONCURRENTLY, per partition) before deploy. Requires
-- pgvector >= 0.8 (HNSW on a partitioned parent + iterative_scan), already installed (0.8.0).

------------------------------------------------------------------------------------------------
-- 1. persons: the global, cross-device catalog of distinct faces (mirror of `speakers`).
--    centroid is vector(512) — ArcFace's own dim, NOT the 1024-d text-embedding space and NOT
--    the 192-d voiceprint space. Stored already L2-normalized (the worker normalizes before write).
------------------------------------------------------------------------------------------------
CREATE TABLE persons (
    person_id            uuid PRIMARY KEY,
    centroid             vector(512),
    n_samples            bigint NOT NULL DEFAULT 0,
    display_name         text,
    first_seen_device_id text REFERENCES devices (device_id),
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now()
);

-- Global name -> id resolution (case-insensitive), used by RAG and the backend.
CREATE INDEX persons_name_idx ON persons (lower(display_name));

------------------------------------------------------------------------------------------------
-- 2. person_segments: raw per-face templates, RANGE-partitioned by created_at (mirror of
--    speaker_segments). One row PER DETECTED FACE (so many rows per segment). This is the durable
--    idempotency source of truth (keyed by segment_id) AND the substrate for the k-NN matcher and
--    a future recluster. frame_offset_nanos records WHICH sampled frame within the ~2s segment the
--    face came from, so the backend sample-face endpoint can seek + crop the exact frame.
------------------------------------------------------------------------------------------------
CREATE TABLE person_segments (
    id                bigserial,
    segment_id        uuid REFERENCES segments (segment_id) ON DELETE CASCADE,
    device_id         text,
    person_id         uuid,                                  -- nullable; NULL = unassigned
    start_unix_nanos  bigint,
    end_unix_nanos    bigint,
    frame_offset_nanos bigint,                               -- offset of the source frame within the segment
    embedding         vector(512),                           -- raw ArcFace face template (L2-normalized)
    bbox              jsonb,                                  -- [x, y, w, h] in source-frame pixels
    det_score         real,                                  -- face-detector confidence
    quality           text,                                  -- 'clean' | 'marginal'; only 'clean' feeds the centroid
    created_at        timestamptz NOT NULL DEFAULT now(),     -- partition key
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

-- Parent indexes propagate to every partition. segment_id backs ON DELETE CASCADE + the worker's
-- idempotent per-segment delete; (person_id, start_unix_nanos) backs the exhaustive
-- "every time we saw Bob" path + merge/recluster repointing.
CREATE INDEX person_segments_segment_id_idx ON person_segments (segment_id);
CREATE INDEX person_segments_person_time_idx ON person_segments (person_id, start_unix_nanos);
-- HNSW over the raw face templates (the matcher's k-NN + a future raw-level recluster). 512-d
-- ArcFace is L2-normalized, so cosine ops + the `<=>` operator must agree (same as speakers).
CREATE INDEX person_segments_embedding_hnsw
    ON person_segments USING hnsw (embedding vector_cosine_ops);

-- Partition maintenance helpers (clones of the 0006 speaker_segment helpers).
CREATE OR REPLACE FUNCTION ensure_person_segment_partitions(months_ahead int DEFAULT 2)
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
        part_name := format('person_segments_%s', to_char(start_d, 'YYYY_MM'));
        IF NOT EXISTS (SELECT 1 FROM pg_class WHERE relname = part_name) THEN
            EXECUTE format(
                'CREATE TABLE %I PARTITION OF person_segments FOR VALUES FROM (%L) TO (%L)',
                part_name, start_d, end_d
            );
        END IF;
    END LOOP;
END;
$$;

-- Retention / privacy purge for raw face templates (O(1) DROP vs mass DELETE + VACUUM). NB:
-- dropping raw templates does NOT recompute the running-mean persons.centroid — it lingers until
-- a recluster (the same documented cascade gap as speaker_segments).
CREATE OR REPLACE FUNCTION drop_person_segment_partitions_before(cutoff date)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
    FOR r IN
        SELECT c.relname
        FROM pg_inherits inh
        JOIN pg_class c ON c.oid = inh.inhrelid
        JOIN pg_class p ON p.oid = inh.inhparent
        WHERE p.relname = 'person_segments'
          AND c.relname ~ '^person_segments_[0-9]{4}_[0-9]{2}$'
          AND to_date(right(c.relname, 7), 'YYYY_MM') < date_trunc('month', cutoff)::date
    LOOP
        EXECUTE format('DROP TABLE %I', r.relname);
    END LOOP;
END;
$$;

SELECT ensure_person_segment_partitions(3);
CREATE TABLE person_segments_default PARTITION OF person_segments DEFAULT;

------------------------------------------------------------------------------------------------
-- 3. scene_objects: RECREATE as objects-only, partitioned. Holds one row PER DETECTED OBJECT
--    (region) plus an open-vocab whole-frame row, each with a CLIP IMAGE embedding (512-d). RAG
--    answers "when did I see a car / a red mug" by embedding the query phrase with the CLIP TEXT
--    tower and NN-searching this table's HNSW index. The original 0001 single-table (faces+objects)
--    shape is replaced; faces now live in person_segments (see header).
------------------------------------------------------------------------------------------------
DROP TABLE scene_objects;
CREATE TABLE scene_objects (
    id                 bigserial,
    segment_id         uuid REFERENCES segments (segment_id) ON DELETE CASCADE,
    device_id          text,
    object_label       text,                                  -- detector class / CLIP zero-shot top-1; '__frame__' for whole-frame rows
    bbox               jsonb,                                 -- [x, y, w, h] in source-frame pixels; NULL for whole-frame rows
    det_score          real,                                  -- detector confidence; NULL for whole-frame rows
    frame_offset_nanos bigint,                                -- offset of the source frame within the segment
    start_unix_nanos   bigint,
    end_unix_nanos     bigint,
    embedding          vector(512),                           -- CLIP image embedding (L2-normalized)
    embedding_model    text,
    embedding_dim      integer,
    created_at         timestamptz NOT NULL DEFAULT now(),    -- partition key
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

CREATE INDEX scene_objects_segment_id_idx ON scene_objects (segment_id);
CREATE INDEX scene_objects_device_time_idx ON scene_objects (device_id, start_unix_nanos);
CREATE INDEX scene_objects_label_time_idx ON scene_objects (object_label, start_unix_nanos);
CREATE INDEX scene_objects_embedding_hnsw
    ON scene_objects USING hnsw (embedding vector_cosine_ops);

CREATE OR REPLACE FUNCTION ensure_scene_object_partitions(months_ahead int DEFAULT 2)
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
        part_name := format('scene_objects_%s', to_char(start_d, 'YYYY_MM'));
        IF NOT EXISTS (SELECT 1 FROM pg_class WHERE relname = part_name) THEN
            EXECUTE format(
                'CREATE TABLE %I PARTITION OF scene_objects FOR VALUES FROM (%L) TO (%L)',
                part_name, start_d, end_d
            );
        END IF;
    END LOOP;
END;
$$;

CREATE OR REPLACE FUNCTION drop_scene_object_partitions_before(cutoff date)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
    FOR r IN
        SELECT c.relname
        FROM pg_inherits inh
        JOIN pg_class c ON c.oid = inh.inhrelid
        JOIN pg_class p ON p.oid = inh.inhparent
        WHERE p.relname = 'scene_objects'
          AND c.relname ~ '^scene_objects_[0-9]{4}_[0-9]{2}$'
          AND to_date(right(c.relname, 7), 'YYYY_MM') < date_trunc('month', cutoff)::date
    LOOP
        EXECUTE format('DROP TABLE %I', r.relname);
    END LOOP;
END;
$$;

SELECT ensure_scene_object_partitions(3);
CREATE TABLE scene_objects_default PARTITION OF scene_objects DEFAULT;

------------------------------------------------------------------------------------------------
-- 4. segment_vision_status: the vision work queue (mirror of segment_transcription_status). A
--    SEPARATE status table so an ASR failure and a vision failure are claimed/retried/surfaced
--    independently. The worker's claim_one_vision uses the same FOR UPDATE SKIP LOCKED + lease
--    pattern; rows are seeded for media_type IN (2,3) (VIDEO/MUXED) at ingest + backfill.
------------------------------------------------------------------------------------------------
CREATE TABLE segment_vision_status (
    segment_id  uuid PRIMARY KEY REFERENCES segments (segment_id) ON DELETE CASCADE,
    status      text        NOT NULL DEFAULT 'pending',   -- pending | processing | done | error
    attempts    integer     NOT NULL DEFAULT 0,
    last_error  text,
    claimed_at  timestamptz,
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX segment_vision_status_claim_idx
    ON segment_vision_status (status, claimed_at);
