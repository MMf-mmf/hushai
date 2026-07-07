-- Ahithophel advisor consultations: session state machine + turns + Q&A memory.
--
-- Three pieces (all owned by hushai-advisor; DDL shape adapted from 0008 chat_sessions):
--   1. advisor_sessions: one row per consultation window. Unlike chat_sessions there is no
--      agent binding — the advisor IS the pipeline — but there is a PHASE state machine:
--        'gathering'  the sufficiency gate is still collecting context (follow-up questions
--                     have been asked; the next user turn feeds the same gate),
--        'answering'  transient: set while the routing/draft/critique loop runs (a crash
--                     mid-answer leaves this; the next turn re-enters the gate),
--        'done'       a final answer was delivered; the next user turn starts a fresh
--                     gathering cycle in the same session (history is kept).
--      followup_rounds counts Yenta rounds this cycle (capped in config so a flaky judge
--      can never wedge a session in 'gathering'). refined_question is the Message-Refiner
--      output the answer cycle ran on (kept for transparency/eval).
--   2. advisor_messages: the turns. seq is 0-based gap-free (0008 contract). `kind`
--      discriminates what an assistant turn IS — 'followup_questions' (a Yenta round,
--      content = the numbered questions) vs 'final_answer' — so a reloaded session
--      re-renders the consultation flow faithfully. `chapters` holds the cited chapter
--      list ([{no, title}]) for final answers, NULL otherwise.
--   3. advisor_memories: the long-term Q&A memory (spec agents 9/10). One row per
--      delivered answer: the refined question + an LLM answer summary, embedded into the
--      system-wide 1024-dim space for cosine retrieval into later consultations.
--      embedding_model/embedding_dim follow the 0001 generation-tracking contract.
--
-- PARTITIONING: intentionally NONE — low-volume, human-authored, user-OWNED history
-- (the 0008 rationale verbatim).

CREATE TABLE advisor_sessions (
    session_id       uuid PRIMARY KEY,             -- UUIDv7, minted by hushai-advisor
    title            text,                         -- first-message-derived
    phase            text        NOT NULL DEFAULT 'gathering',
    followup_rounds  integer     NOT NULL DEFAULT 0,
    refined_question text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT advisor_sessions_phase_chk CHECK (phase IN ('gathering', 'answering', 'done'))
);

CREATE INDEX advisor_sessions_recent_idx ON advisor_sessions (updated_at DESC);

CREATE TABLE advisor_messages (
    message_id  uuid PRIMARY KEY,                  -- UUIDv7
    session_id  uuid        NOT NULL REFERENCES advisor_sessions (session_id) ON DELETE CASCADE,
    seq         integer     NOT NULL,              -- 0-based per session; gap-free ordering key
    role        text        NOT NULL,
    kind        text        NOT NULL DEFAULT 'message',
    content     text        NOT NULL,
    chapters    jsonb,                             -- final answers: [{no, title}]; NULL otherwise
    created_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT advisor_messages_role_chk CHECK (role IN ('user', 'assistant')),
    CONSTRAINT advisor_messages_kind_chk
        CHECK (kind IN ('message', 'followup_questions', 'final_answer')),
    UNIQUE (session_id, seq)
);

CREATE INDEX advisor_messages_session_seq_idx ON advisor_messages (session_id, seq);

CREATE TABLE advisor_memories (
    memory_id       uuid PRIMARY KEY,              -- UUIDv7
    session_id      uuid,                          -- provenance; no FK so memories outlive sessions
    question        text        NOT NULL,          -- the refined question
    answer_summary  text        NOT NULL,
    chapters        jsonb,                         -- [{no, title}] the answer drew on
    embedding       vector(1024),
    embedding_model text,
    embedding_dim   integer,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX advisor_memories_embedding_hnsw
    ON advisor_memories USING hnsw (embedding vector_cosine_ops);
