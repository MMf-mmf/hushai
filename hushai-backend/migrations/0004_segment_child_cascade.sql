-- Deleting a segment must remove its derived rows (retention, GC, test cleanup).
--
-- The phase-1/2 FKs to `segments` used NO ACTION, so deleting a segment errored once
-- any child row existed. This turned load-bearing once segment ingest began writing a
-- `segment_transcription_status` row in the ingest transaction: a segment can no longer
-- be deleted without first deleting its status row. ON DELETE CASCADE makes segment
-- deletion clean up every derived table in one step.

ALTER TABLE segment_transcription_status
    DROP CONSTRAINT segment_transcription_status_segment_id_fkey,
    ADD  CONSTRAINT segment_transcription_status_segment_id_fkey
        FOREIGN KEY (segment_id) REFERENCES segments (segment_id) ON DELETE CASCADE;

-- transcript_sentences is partitioned; altering the parent constraint covers all
-- partitions. (Its FK was auto-named `_fkey1` when 0003 recreated the table.)
ALTER TABLE transcript_sentences
    DROP CONSTRAINT transcript_sentences_segment_id_fkey1,
    ADD  CONSTRAINT transcript_sentences_segment_id_fkey
        FOREIGN KEY (segment_id) REFERENCES segments (segment_id) ON DELETE CASCADE;

ALTER TABLE video_events
    DROP CONSTRAINT video_events_segment_id_fkey,
    ADD  CONSTRAINT video_events_segment_id_fkey
        FOREIGN KEY (segment_id) REFERENCES segments (segment_id) ON DELETE CASCADE;

ALTER TABLE scene_objects
    DROP CONSTRAINT scene_objects_segment_id_fkey,
    ADD  CONSTRAINT scene_objects_segment_id_fkey
        FOREIGN KEY (segment_id) REFERENCES segments (segment_id) ON DELETE CASCADE;
