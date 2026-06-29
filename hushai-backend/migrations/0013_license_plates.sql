-- 0013_license_plates.sql — ALPR: a catalog of distinct license plates + the raw per-read
-- detections, the VEHICLE sibling of 0009's persons/person_segments.
--
-- KEY DIVERGENCE from faces/speakers: a plate's identity IS its text, so the catalog is matched by
-- a NORMALIZED STRING (exact + edit-distance fuzzy for OCR noise), NOT by a k-NN over an embedding.
-- Two photos of "ABC123" must collapse to one row even with zero visual similarity (night/day,
-- angle, dirt). We still keep the 0009 machinery the plate lane reuses: monthly RANGE partitioning,
-- per-segment idempotency (delete-by-segment), and ensure/drop partition helpers. An optional CLIP
-- embedding column is reserved for a future "find visually-similar unreadable plates" path; it is
-- nullable and intentionally has NO HNSW index in this migration (string is the matcher).
--
-- MIGRATION SAFETY: same notes as 0009 (CREATE INDEX on a partitioned parent locks each partition;
-- fine for empty dev tables). Requires pg_trgm + fuzzystrmatch (both ship with Postgres contrib).

CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS fuzzystrmatch;

------------------------------------------------------------------------------------------------
-- 1. license_plates: the global catalog of distinct plates. `plate_text` is the human-facing
--    canonical (voted) string; `plate_text_norm` is the confusable-folded matching key
--    (O→0, I→1, …) carrying the UNIQUE constraint so the worker's match-or-mint is race-safe.
------------------------------------------------------------------------------------------------
CREATE TABLE license_plates (
    plate_id              uuid PRIMARY KEY,
    plate_text            text NOT NULL,                         -- canonical, voted (display)
    plate_text_norm       text NOT NULL,                         -- confusable-folded matching key
    region_hint           text,                                  -- optional country/state guess (generic)
    n_samples             bigint NOT NULL DEFAULT 0,             -- raw per-read count
    n_sightings           bigint NOT NULL DEFAULT 0,             -- maintained/sessionized sighting count
    display_name          text,                                  -- human label ("Mom's car"); nameable later
    first_seen_unix_nanos bigint,
    last_seen_unix_nanos  bigint,
    first_seen_device_id  text REFERENCES devices (device_id),
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now()
);

-- Exact-match + race-safe mint key, name resolution, and trigram-backed fuzzy candidate search.
CREATE UNIQUE INDEX license_plates_norm_idx ON license_plates (plate_text_norm);
CREATE INDEX license_plates_name_idx ON license_plates (lower(display_name));
CREATE INDEX license_plates_trgm_idx ON license_plates USING gin (plate_text_norm gin_trgm_ops);

------------------------------------------------------------------------------------------------
-- 2. plate_detections: raw per-read rows, RANGE-partitioned by created_at (mirror of
--    person_segments). One row PER OCR READ (many per segment). Durable idempotency source
--    (keyed by segment_id) + the substrate for temporal re-voting of a plate's canonical text.
------------------------------------------------------------------------------------------------
CREATE TABLE plate_detections (
    id                  bigserial,
    segment_id          uuid REFERENCES segments (segment_id) ON DELETE CASCADE,
    device_id           text,
    plate_id            uuid,                                    -- nullable; NULL = read below the catalog gate
    start_unix_nanos    bigint,
    end_unix_nanos      bigint,
    frame_offset_nanos  bigint,                                  -- source frame within the segment
    vehicle_bbox        jsonb,                                   -- [x,y,w,h] orig-frame px (parent vehicle)
    vehicle_label       text,                                    -- 'car'|'truck'|'bus'|'motorcycle'
    plate_bbox          jsonb,                                   -- [x,y,w,h] orig-frame px
    plate_corners       jsonb,                                   -- [[x,y]×4] orig-frame px (rectify source)
    ocr_text            text,                                    -- this read's raw string
    ocr_text_norm       text,                                    -- confusable-folded
    ocr_confidence      real,                                    -- mean per-char confidence (0..1)
    char_confidences    jsonb,                                   -- [c0,c1,…] for temporal voting
    det_score           real,                                    -- plate-detector confidence
    quality             text,                                    -- 'clean' | 'marginal' (only 'clean' mints)
    embedding           vector(512),                             -- OPTIONAL CLIP of rectified plate; nullable, no HNSW
    crop_uri            text,                                    -- persisted rectified plate JPEG (thumbnail)
    is_best_shot        boolean NOT NULL DEFAULT false,
    created_at          timestamptz NOT NULL DEFAULT now(),      -- partition key
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

CREATE INDEX plate_detections_segment_id_idx ON plate_detections (segment_id);
CREATE INDEX plate_detections_plate_time_idx ON plate_detections (plate_id, start_unix_nanos);
CREATE INDEX plate_detections_normtext_time_idx ON plate_detections (ocr_text_norm, start_unix_nanos);

CREATE OR REPLACE FUNCTION ensure_plate_detection_partitions(months_ahead int DEFAULT 2)
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
        part_name := format('plate_detections_%s', to_char(start_d, 'YYYY_MM'));
        IF NOT EXISTS (SELECT 1 FROM pg_class WHERE relname = part_name) THEN
            EXECUTE format(
                'CREATE TABLE %I PARTITION OF plate_detections FOR VALUES FROM (%L) TO (%L)',
                part_name, start_d, end_d
            );
        END IF;
    END LOOP;
END;
$$;

CREATE OR REPLACE FUNCTION drop_plate_detection_partitions_before(cutoff date)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
    FOR r IN
        SELECT c.relname
        FROM pg_inherits inh
        JOIN pg_class c ON c.oid = inh.inhrelid
        JOIN pg_class p ON p.oid = inh.inhparent
        WHERE p.relname = 'plate_detections'
          AND c.relname ~ '^plate_detections_[0-9]{4}_[0-9]{2}$'
          AND to_date(right(c.relname, 7), 'YYYY_MM') < date_trunc('month', cutoff)::date
    LOOP
        EXECUTE format('DROP TABLE %I', r.relname);
    END LOOP;
END;
$$;

SELECT ensure_plate_detection_partitions(3);
CREATE TABLE plate_detections_default PARTITION OF plate_detections DEFAULT;
