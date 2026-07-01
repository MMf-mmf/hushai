-- Queue-claim performance hardening (no behavior change).
--
-- Both worker lanes claim the OLDEST processable segment with
--   ... JOIN segments g ON g.segment_id = s.segment_id
--   WHERE g.media_type IN (...) AND (status filter)
--   ORDER BY g.capture_start_unix_nanos
--   FOR UPDATE SKIP LOCKED LIMIT 1
-- (see hushai-worker/src/claim.rs `claim_one` / `claim_one_vision`). Today there is NO index on
-- segments(capture_start_unix_nanos), so that ORDER BY ... LIMIT 1 is an unindexed sort over a
-- table that grows without bound — fine on a dev corpus, a real bottleneck at the 30-camera target.
-- Adding the sort index keeps the always-on claim cheap so the queue itself never becomes the
-- thing that "overloads" the device. The strict oldest-first ordering is UNCHANGED.
--
-- The partial indexes keep the planner's status filter index-only on the small set of CLAIMABLE
-- rows: at steady state 'done' is the overwhelming majority, so excluding it keeps these tiny.
--
-- Index builds here are non-CONCURRENT (matches the 0009 convention). On a production-sized
-- corpus, build them out-of-band with CREATE INDEX CONCURRENTLY before deploying this migration,
-- then this file is a no-op via IF NOT EXISTS.

CREATE INDEX IF NOT EXISTS segments_capture_start_idx
    ON segments (capture_start_unix_nanos);

CREATE INDEX IF NOT EXISTS segment_transcription_status_claimable_idx
    ON segment_transcription_status (segment_id)
    WHERE status IN ('pending', 'error', 'processing');

CREATE INDEX IF NOT EXISTS segment_vision_status_claimable_idx
    ON segment_vision_status (segment_id)
    WHERE status IN ('pending', 'error', 'processing');
