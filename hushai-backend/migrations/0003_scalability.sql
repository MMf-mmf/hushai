-- Scale transcript_sentences for always-on, multi-device capture.
--
-- Three changes, consolidated into one table recreate (the table is tiny and its
-- rows are regenerable by re-running the worker, so a recreate is the cheapest path
-- and lets the partitioned shape exist from the start):
--
--   1. Denormalize device_id onto the table. The RAG filter previously lived on the
--      JOINed `segments` table while the HNSW index is on transcript_sentences.embedding,
--      so the planner could not combine them -> a selective device filter either
--      returned < top_k rows (recall cliff) or abandoned the index for a full scan.
--      With device_id local, filters sit on the same table as the vector index.
--   2. Index segment_id. The FK creates no index, so the worker's idempotent
--      `DELETE FROM transcript_sentences WHERE segment_id = $1` sequentially scanned
--      the whole table on every (re)write.
--   3. RANGE-partition by created_at (monthly). Each partition's HNSW index stays
--      small enough to be RAM-resident, and retention becomes a cheap DETACH/DROP of
--      old months instead of a mass DELETE + VACUUM over one ever-growing index.
--
-- Requires pgvector >= 0.8 (HNSW on a partitioned parent + iterative_scan used by the
-- RAG query). Verified against vector 0.8.0.
--
-- NOTE: created_at is INSERT time, which tracks capture time for a live always-on
-- worker. If time-window queries (after/before on start_unix_nanos) ever dominate,
-- partitioning by a capture-time column would additionally enable partition pruning
-- for those queries; created_at is the simpler choice and is what retention needs.

-- 1. Move the phase-1/2 table + its HNSW index aside (renamed, dropped at the end).
ALTER TABLE transcript_sentences RENAME TO transcript_sentences_legacy;
ALTER INDEX transcript_sentences_embedding_hnsw
    RENAME TO transcript_sentences_legacy_embedding_hnsw;

-- 2. Partitioned parent. A partitioned table's PK must include the partition key.
CREATE TABLE transcript_sentences (
    id               bigserial,
    segment_id       uuid REFERENCES segments (segment_id),
    device_id        text,                                  -- denormalized for filter locality
    text             text,
    start_unix_nanos bigint,
    end_unix_nanos   bigint,
    sentiment        text,
    emotion          text,
    embedding        vector(1024),
    embedding_model  text,
    embedding_dim    integer,
    created_at       timestamptz NOT NULL DEFAULT now(),    -- partition key
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

-- 3. Parent indexes — propagate to every existing and future partition.
CREATE INDEX transcript_sentences_segment_id_idx
    ON transcript_sentences (segment_id);
CREATE INDEX transcript_sentences_device_time_idx
    ON transcript_sentences (device_id, start_unix_nanos);
CREATE INDEX transcript_sentences_embedding_hnsw
    ON transcript_sentences USING hnsw (embedding vector_cosine_ops);

-- 4. Partition maintenance helpers.
--    ensure_transcript_partitions(): create monthly partitions for the current month
--    through `months_ahead` months out (idempotent). Run at worker startup and from a
--    scheduled job (cron / pg_cron) so inserts always land in a real month partition.
CREATE OR REPLACE FUNCTION ensure_transcript_partitions(months_ahead int DEFAULT 2)
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
        part_name := format('transcript_sentences_%s', to_char(start_d, 'YYYY_MM'));
        IF NOT EXISTS (SELECT 1 FROM pg_class WHERE relname = part_name) THEN
            EXECUTE format(
                'CREATE TABLE %I PARTITION OF transcript_sentences FOR VALUES FROM (%L) TO (%L)',
                part_name, start_d, end_d
            );
        END IF;
    END LOOP;
END;
$$;

--    drop_transcript_partitions_before(): retention — drop whole monthly partitions
--    whose month is strictly before `cutoff`. O(1) DROP vs a mass DELETE + VACUUM.
CREATE OR REPLACE FUNCTION drop_transcript_partitions_before(cutoff date)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
    FOR r IN
        SELECT c.relname
        FROM pg_inherits inh
        JOIN pg_class c ON c.oid = inh.inhrelid
        JOIN pg_class p ON p.oid = inh.inhparent
        WHERE p.relname = 'transcript_sentences'
          AND c.relname ~ '^transcript_sentences_[0-9]{4}_[0-9]{2}$'
          AND to_date(right(c.relname, 7), 'YYYY_MM') < date_trunc('month', cutoff)::date
    LOOP
        EXECUTE format('DROP TABLE %I', r.relname);
    END LOOP;
END;
$$;

-- 5. Create the current + next 3 monthly partitions, then a DEFAULT catch-all.
--    Months are created before DEFAULT and before any data, so DEFAULT stays empty
--    and future ATTACH/CREATE never has to scan it for conflicting rows.
SELECT ensure_transcript_partitions(3);
CREATE TABLE transcript_sentences_default PARTITION OF transcript_sentences DEFAULT;

-- 6. Carry the existing (tiny) corpus over, backfilling device_id from segments.
--    created_at defaults to now() -> lands in the current month partition.
INSERT INTO transcript_sentences
    (segment_id, device_id, text, start_unix_nanos, end_unix_nanos,
     sentiment, emotion, embedding, embedding_model, embedding_dim)
SELECT l.segment_id, s.device_id, l.text, l.start_unix_nanos, l.end_unix_nanos,
       l.sentiment, l.emotion, l.embedding, l.embedding_model, l.embedding_dim
FROM transcript_sentences_legacy l
LEFT JOIN segments s ON s.segment_id = l.segment_id;

DROP TABLE transcript_sentences_legacy;
