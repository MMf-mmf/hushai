-- Multi-vector speaker matching + raw-level healing: an ANN index over the raw
-- per-segment voiceprints, plus a per-segment quality tag.
--
-- WHY: the online matcher (hushai-worker/src/speaker_match.rs) previously matched a
-- new embedding against a single running-mean speakers.centroid and minted a brand-new
-- speaker on any miss. Under background static that running mean drifts and one person
-- fragments into many "unknown speaker" rows. The fix matches against the k nearest RAW
-- embeddings in speaker_segments (a multi-vector identity that captures a voice's natural
-- spread) and only mints from clean, far audio. That k-NN needs an ANN index here, and
-- the same index backs the backend's raw-level recluster/duplicate-detection.
--
-- Two changes, both metadata-only-ish on the partitioned parent (they propagate to every
-- existing and future partition):
--
--   1. HNSW vector_cosine_ops index on speaker_segments.embedding. Mirrors the
--      transcript_sentences HNSW (0003) and assumes pgvector >= 0.8 (HNSW on a partitioned
--      parent + iterative_scan). Cosine matches the L2-normalized 192-d TitaNet vectors and
--      the `<=>` operator the matcher/recluster use.
--   2. quality text on speaker_segments: 'clean' | 'marginal' (NULL for pre-feature rows).
--      The worker tags each row; only 'clean' rows feed the self-healing centroid recompute,
--      so a noisy embedding can attach to a known speaker without ever poisoning its centroid.
--
-- MIGRATION SAFETY (same caveat as 0006): `ADD COLUMN ... NULL` on a partitioned parent is
-- metadata-only, but `CREATE INDEX` on a partitioned parent is NOT concurrent and locks each
-- partition while it builds. Fine for the dev corpus; on a production-sized corpus build the
-- HNSW out-of-band (CONCURRENTLY, per partition) before deploy.
--
-- DEV GOTCHA: the backend embeds these files at compile time via sqlx::migrate!(). After
-- ADDING this file, `touch hushai-backend/src/lib.rs` (and rebuild the worker too) so the
-- binaries re-embed it, else startup fails "migration 7 ... missing in the resolved migrations".

-- 1. Per-segment quality tag. Pre-feature rows stay NULL (excluded from clean-centroid
--    recompute; they still participate as historical k-NN voters).
ALTER TABLE speaker_segments ADD COLUMN quality text;

-- 2. ANN index backing the k-NN matcher and the raw-level recluster/duplicate queries.
CREATE INDEX speaker_segments_embedding_hnsw
    ON speaker_segments USING hnsw (embedding vector_cosine_ops);
