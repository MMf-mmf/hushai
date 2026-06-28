-- Multi-turn chat over recordings: persistent conversation history that powers the
-- webapp's chat panel and is the foundation for future per-agent chat windows.
--
-- Two pieces:
--   1. chat_sessions: one row per conversation window. agent_id is an IMMUTABLE binding
--      to a code-registry agent (hushai-rag/src/agents.rs), stored as plain `text` with
--      NO foreign key — agents live in code, not in the DB, so adding/renaming an agent
--      is a code change, not a migration. title is derived from the first user message.
--   2. chat_messages: the turns. seq is a 0-based, gap-free per-session ordering key (the
--      history loader reads the trailing window by seq). sources holds the cited
--      Vec<retrieve::Source> as jsonb for ASSISTANT turns (NULL for user turns) so a
--      reopened conversation can re-render its citation deep-links into the video timeline.
--
-- PARTITIONING: intentionally NONE. Unlike transcript_sentences / speaker_segments (high
-- cardinality, append-only ingest, retention via partition DROP), chat is low-volume,
-- human-authored, and user-OWNED history we must never silently drop on a month boundary.
-- Following the speakers-table precedent ("low row count -> not partitioned"), these are
-- plain tables; that also keeps the chat_messages -> chat_sessions FK a simple ON DELETE
-- CASCADE and the PK a plain uuid (a partitioned PK would have to include the partition key).

CREATE TABLE chat_sessions (
    session_id  uuid PRIMARY KEY,                  -- UUIDv7, minted by hushai-rag
    agent_id    text        NOT NULL,              -- immutable binding to a code-registry agent
    title       text,                              -- optional; first-message-derived
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

-- Most-recent-first listing for restoring/selecting conversation windows.
CREATE INDEX chat_sessions_recent_idx ON chat_sessions (updated_at DESC);

CREATE TABLE chat_messages (
    message_id  uuid PRIMARY KEY,                  -- UUIDv7
    session_id  uuid        NOT NULL REFERENCES chat_sessions (session_id) ON DELETE CASCADE,
    seq         integer     NOT NULL,              -- 0-based per session; gap-free ordering key
    role        text        NOT NULL,              -- 'user' | 'assistant'
    content     text        NOT NULL,
    sources     jsonb,                             -- assistant turns: Vec<retrieve::Source>; NULL for user
    agent_id    text        NOT NULL,              -- denormalized: agent that owns/produced this turn
    created_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT chat_messages_role_chk CHECK (role IN ('user', 'assistant')),
    -- ordering integrity + concurrency guard: a racing duplicate seq fails loudly rather
    -- than silently reordering the transcript.
    UNIQUE (session_id, seq)
);

-- Load the trailing window of a session in order (the history fetch) and list a transcript.
CREATE INDEX chat_messages_session_seq_idx ON chat_messages (session_id, seq);
