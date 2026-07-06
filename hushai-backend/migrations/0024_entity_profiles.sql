-- 0024_entity_profiles.sql — accumulated "running memory" per identity.
--
-- One row per (subject_type, subject_id): 'person' = a face identity (persons.person_id),
-- 'speaker' = a voice identity (speakers.speaker_id). The profile accumulates one observation
-- line per coalesced visit (person) / conversation (speaker), folded incrementally from the
-- already-sessionized `events` table by hushai-backend::profiles (deterministic — no LLM; the
-- RAG service narrates at chat time). Anonymous identities accumulate too: the moment one is
-- named, its whole history is already attached (rename never re-keys; merge folds rows).
--
-- DERIVED DATA: rebuildable from events + transcript_sentences. Deleting a row only forgets
-- the narrative, never the identity. No FK on subject_id — same contract as events.subject_id
-- (merges may orphan; the profile merge hook folds what was consumed).
CREATE TABLE entity_profiles (
    subject_type          text NOT NULL CHECK (subject_type IN ('person', 'speaker')),
    subject_id            uuid NOT NULL,
    -- Append-only observation log, one line per visit/conversation; oldest lines are
    -- deterministically compacted into a single rollup line at the char cap.
    profile_text          text NOT NULL DEFAULT '',
    -- Coalesced visits (person) / conversations (speaker) folded so far.
    visit_count           bigint NOT NULL DEFAULT 0,
    first_seen_unix_nanos bigint,
    last_seen_unix_nanos  bigint,
    -- Incremental watermark over events.updated_at (WALL clock, not capture time — robust to
    -- the worker reprocessing an old-capture backlog late).
    last_event_at         timestamptz NOT NULL DEFAULT to_timestamp(0),
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (subject_type, subject_id)
);
