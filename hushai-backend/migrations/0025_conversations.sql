-- 0025_conversations.sql — persisted conversation threading.
--
-- Gives the stack a first-class CONVERSATION: a cluster of transcript sentences on one
-- device, bounded by silence gaps and (within a gap block) disentangled by speaker
-- turn-taking + topic so two concurrent group conversations on one mic stay separate.
-- Assigned by the batch threader (hushai-backend::conversations, driven by worker 0),
-- never by the live per-segment write path.
--
-- Three pieces:
--   1. conversations: the catalog (sessionized, low row count -> plain table, the 0014
--      events precedent). Cross-DEVICE overlap is a LINK (link_group_id), never a merge —
--      merging would interleave duplicate ASR text of the same audio into one transcript.
--   2. transcript_sentences.conversation_id + turn_index (denormalized, like speaker_id) so
--      the RAG filter sits on the same table as the HNSW index (the 0003 recall-cliff
--      lesson: NEVER filter via a JOIN). A COLUMN, not a mapping table: the worker's
--      delete-then-insert reprocess re-mints sentence ids, so a mapping table would orphan;
--      the column dies with the row and the fresh created_at re-enters the threader's
--      watermark scan naturally.
--   3. threader_state: singleton watermark row (the profiles.last_event_at idiom,
--      globalized) recording how far the threader has consumed transcript_sentences.
--
-- MUTABILITY CONTRACT (load-bearing for every consumer):
--   * status='open'   -> provisional. Boundaries and sentence assignments may be revised
--                        while inside the threader's lookback window. Do not cache the id.
--   * status='closed' -> frozen. The threader never rewrites a closed conversation except
--                        append-only late-attach of reprocessed sentences whose time falls
--                        inside the closed span.
--   * conversation_id NULL on transcript_sentences = ungrouped (pre-feature history, the
--     always-lagging newest tail, or backfill not run). Same contract as speaker_id NULL:
--     consumers fall back to the query-time gap heuristic, never error.
--
-- TYPE CONTRACT: conversations.conversation_id and transcript_sentences.conversation_id are
-- both `uuid` (unlike speaker_id, which is text on the transcript table for 0006-era `ANY()`
-- reasons — conversation filters are equality/GROUP BY, no text[] binding anywhere).
--
-- No FK from transcript_sentences.conversation_id to conversations (the events.subject_id
-- contract, 0014/0024): the threader may merge/delete conversation rows; a stale pointer on
-- history is repaired by re-thread/backfill, and must never block a transcript write.
--
-- No backfill here: pre-feature rows stay NULL until the operator runs the explicit
-- backfill (THREADER_BACKFILL_ON_START / admin call).
--
-- MIGRATION SAFETY: `ADD COLUMN ... NULL` on the partitioned parent is metadata-only, but
-- `CREATE INDEX` on a partitioned parent is NOT concurrent and locks each partition while
-- it builds. Fine for the dev corpus; on a production-sized corpus build the conversation
-- index out-of-band (CONCURRENTLY, per partition) before deploy.

-- 1. The conversation catalog.
CREATE TABLE conversations (
    conversation_id       uuid PRIMARY KEY,                  -- Uuid::now_v7 (house standard)
    primary_device_id     text REFERENCES devices (device_id),
    -- v1 always [primary_device_id]; kept as an array so a future cross-device MERGE (v2,
    -- on top of links) has a home without another migration.
    device_ids            text[] NOT NULL DEFAULT '{}',
    started_at_unix_nanos bigint NOT NULL,
    ended_at_unix_nanos   bigint NOT NULL,
    status                text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'closed')),
    -- Participant set, denormalized from member sentences (uuid — the speakers-catalog
    -- space, NOT the text speaker_id on transcript_sentences).
    speaker_ids           uuid[] NOT NULL DEFAULT '{}',
    sentence_count        integer NOT NULL DEFAULT 0,
    -- Cross-device link: conversations heard by two mics at once share a link_group_id.
    link_group_id         uuid,
    -- NULL-ready for the follow-on summarization ticket; nothing populates these yet.
    summary               text,
    summary_model         text,
    -- Fingerprint of the threader knobs that produced this row (eval provenance; closed
    -- rows keep the hash they were closed under).
    config_hash           text,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now()
);

-- Newest-first per-device listing (RAG conversations endpoints + the viewer window scan).
CREATE INDEX conversations_device_time_idx
    ON conversations (primary_device_id, started_at_unix_nanos DESC);
-- The threader's own working set: open conversations are few; partial index keeps the
-- every-30s pass O(open), not O(history).
CREATE INDEX conversations_open_idx ON conversations (status) WHERE status = 'open';
CREATE INDEX conversations_link_idx ON conversations (link_group_id) WHERE link_group_id IS NOT NULL;
-- Participant containment queries ("conversations where X and Y both spoke": speaker_ids @> $1).
CREATE INDEX conversations_speakers_gin ON conversations USING gin (speaker_ids);

-- 2. Denormalized assignment on the partitioned transcript table. Parent ALTER + parent
--    index propagate to all existing and future partitions.
ALTER TABLE transcript_sentences
    ADD COLUMN conversation_id uuid,
    ADD COLUMN turn_index      integer;
CREATE INDEX transcript_sentences_conversation_time_idx
    ON transcript_sentences (conversation_id, start_unix_nanos);

-- 3. Threader progress watermark (singleton). Wall-clock created_at watermark — not capture
--    time — so the threader survives the worker reprocessing an old-capture backlog late
--    (reprocessed rows get fresh created_at and re-enter the scan; same reasoning as 0024).
CREATE TABLE threader_state (
    id          smallint PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    watermark   timestamptz NOT NULL DEFAULT to_timestamp(0),
    config_hash text,
    updated_at  timestamptz NOT NULL DEFAULT now()
);
INSERT INTO threader_state (id) VALUES (1);
