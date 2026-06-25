-- Transcription + embedding pipeline — phase 2 (the embedding-pipeline ticket).
--
-- Adds:
--   1. segment_transcription_status: per-segment bookkeeping so the worker can
--      claim work durably (FOR UPDATE SKIP LOCKED + a claim lease), retry with
--      backoff, and surface failures. A status row exists for every segment the
--      worker has seen; status transitions pending -> processing -> done|error.
--   2. The HNSW index on transcript_sentences.embedding the initial ticket
--      deferred. Cosine ops: mxbai/bge embeddings are L2-normalized, so cosine
--      distance (`<=>`) is the right operator and retrieval MUST use the same.

CREATE TABLE segment_transcription_status (
    segment_id  uuid PRIMARY KEY REFERENCES segments (segment_id),
    status      text        NOT NULL DEFAULT 'pending',   -- pending | processing | done | error
    attempts    integer     NOT NULL DEFAULT 0,
    last_error  text,
    claimed_at  timestamptz,
    updated_at  timestamptz NOT NULL DEFAULT now()
);

-- Supports the claim query's WHERE (status, claim lease) scans.
CREATE INDEX segment_transcription_status_claim_idx
    ON segment_transcription_status (status, claimed_at);

-- Vector index for nearest-neighbour retrieval (deferred from phase 1 to here).
-- vector_cosine_ops + the `<=>` operator in the RAG retrieval query must agree.
CREATE INDEX transcript_sentences_embedding_hnsw
    ON transcript_sentences USING hnsw (embedding vector_cosine_ops);
