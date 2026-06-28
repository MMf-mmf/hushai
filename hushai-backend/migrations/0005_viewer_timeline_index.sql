-- Backs the timeline viewer's windowed-by-device queries (hushai-viewer):
--   WHERE device_id = $1 AND capture_start_unix_nanos <range> ...
-- `segments` is a PLAIN table (unlike the 0003 RANGE-partitioned transcript_sentences),
-- so an ordinary composite btree applies cleanly — none of the partition/sqlx caveats
-- from 0003 are relevant here. Additive and idempotent.
CREATE INDEX IF NOT EXISTS segments_device_capture_start_idx
    ON segments (device_id, capture_start_unix_nanos);
