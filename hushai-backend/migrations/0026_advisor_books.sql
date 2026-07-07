-- Ahithophel advisor knowledge base: an ingested book, chapter-routed.
--
-- Three pieces (populated by hushai-advisor's one-shot `ingest-book` binary):
--   1. books: one row per ingested book (slug is the ingest CLI's stable handle).
--   2. book_chapters: per-chapter text. BOTH raw OCR text and the cleaned text are kept —
--      the whole book is ~300 KB, and retaining raw_text makes re-cleaning/re-chunking a
--      pure DB operation (no filesystem dependency after ingest). `synopsis` is the 1–2
--      sentence LLM summary the Traffic Controller routes over (50 synopses ≈ 2.5K tokens,
--      one routing prompt).
--   3. book_chunks: paragraph-packed ~1,500-char chunks embedded with mxbai-embed-large
--      into the system-wide 1024-dim space. Used for (a) semantic candidate-widening in
--      chapter routing (anti-tunnel-vision) and (b) representing an over-budget chapter in
--      the draft prompt. embedding_model/embedding_dim follow the 0001 generation-tracking
--      contract so a model swap can find and re-embed stale rows.
--
-- PARTITIONING: intentionally NONE — low-cardinality curated content, never dropped on
-- retention boundaries (the chat_sessions/speakers precedent).

CREATE TABLE books (
    book_id     uuid PRIMARY KEY,                  -- UUIDv7, minted by ingest-book
    slug        text        NOT NULL UNIQUE,       -- stable CLI handle, e.g. 'yes-50-ways'
    title       text        NOT NULL,
    author      text,
    source_path text,                              -- where the chapter texts came from
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE book_chapters (
    chapter_id  uuid PRIMARY KEY,                  -- UUIDv7
    book_id     uuid        NOT NULL REFERENCES books (book_id) ON DELETE CASCADE,
    chapter_no  integer     NOT NULL,              -- 1-based, matches the source filenames
    title       text,                              -- LLM-extracted from the chapter's opening
    synopsis    text,                              -- 1–2 sentence routing summary
    raw_text    text        NOT NULL,              -- as-extracted OCR text (re-clean source)
    clean_text  text        NOT NULL,              -- heuristic + LLM-cleaned text (embedded)
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (book_id, chapter_no)
);

CREATE TABLE book_chunks (
    chunk_id        uuid PRIMARY KEY,              -- UUIDv7
    chapter_id      uuid        NOT NULL REFERENCES book_chapters (chapter_id) ON DELETE CASCADE,
    seq             integer     NOT NULL,          -- 0-based order within the chapter
    content         text        NOT NULL,
    embedding       vector(1024),
    embedding_model text,
    embedding_dim   integer,
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (chapter_id, seq)
);

-- ANN retrieval for routing candidates / over-budget chapter excerpts (cosine, matches
-- the `<=>` operator used everywhere else in the system).
CREATE INDEX book_chunks_embedding_hnsw
    ON book_chunks USING hnsw (embedding vector_cosine_ops);
