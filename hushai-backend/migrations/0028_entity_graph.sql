-- 0028_entity_graph.sql — the Gotham intelligence layer's entity/link graph (spec: Gotham.md §1.2).
--
-- Materialized inter-entity relationships the perception pipeline perceives but never joins:
-- who is co-present with whom, who converses with whom, who arrives with which vehicle, which
-- voice belongs to which face (the review-queued binding), who frequents which place (device).
-- Nodes are the EXISTING catalogs (persons/speakers/license_plates/devices) — there is no node
-- table; an edge endpoint is a (node_type, node_id) pair with NO FK (the events.subject_id /
-- 0024 contract: merges may orphan, merge hooks fold, a stale pointer never blocks a write).
--
-- Folded incrementally by hushai-backend::graph_pass from the already-sessionized `events`
-- (0014) + CLOSED `conversations` (0025) — never the per-detection firehose tables. Fully
-- deterministic (integer/ratio math, no LLM — the profiles.rs/threading.rs doctrine); the RAG
-- service narrates at chat time.
--
-- DERIVED DATA: rebuildable from events + conversations + catalogs. Deleting a person/speaker/
-- plate deletes or orphan-tombstones its edges in the same transaction (the 0024 precedent);
-- a rebuild from surviving sources must never resurrect a deleted entity's links.
CREATE TABLE entity_edges (
    edge_id           uuid PRIMARY KEY,          -- Uuid::now_v7 (house standard)
    edge_type         text NOT NULL CHECK (edge_type IN
        ('co_present',              -- person|speaker <-> person|speaker: overlapping visits, same device
         'conversed_with',          -- speaker <-> speaker: pairs from closed conversations.speaker_ids
         'arrived_with_vehicle',    -- person -> plate: temporal correlation on one device
         'same_identity_candidate', -- speaker <-> person: the voice<->face binding (§1.4)
         'visits_place')),          -- person|speaker|plate -> device
    src_type          text NOT NULL CHECK (src_type IN ('person','speaker','plate','device')),
    src_id            text NOT NULL,
    dst_type          text NOT NULL CHECK (dst_type IN ('person','speaker','plate','device')),
    dst_id            text NOT NULL,
    observation_count bigint NOT NULL DEFAULT 0,
    first_seen_unix_nanos bigint,
    last_seen_unix_nanos  bigint,
    confidence        real,                      -- edge-type-specific; rounded to 4 decimals
    -- Newest-N provenance samples, capped at GRAPH_EDGE_SAMPLE_CAP:
    --   [{"event_id":..,"segment_id":..,"t":<unix_nanos>}, ...]
    -- plus per-type counters (binding: {"together":N,"speaker_only":N,"person_only":N}).
    evidence          jsonb NOT NULL DEFAULT '[]'::jsonb,
    metadata          jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- Binding review-queue state machine; NULL for every other edge type:
    status            text CHECK (status IN ('candidate','confirmed','rejected')),
    config_hash       text,                      -- GRAPH_* fingerprint last touching this row
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    CHECK (status IS NULL OR edge_type = 'same_identity_candidate')
);

-- Idempotent upsert key (the events.dedup_key idiom, structural): endpoints of undirected
-- edge types are producer-canonicalized (lexicographically smaller endpoint stored as src),
-- so this unique index dedups with no mirror-row problem.
CREATE UNIQUE INDEX entity_edges_identity_idx
    ON entity_edges (edge_type, src_type, src_id, dst_type, dst_id);
CREATE INDEX entity_edges_src_idx  ON entity_edges (src_type, src_id, edge_type);
CREATE INDEX entity_edges_dst_idx  ON entity_edges (dst_type, dst_id, edge_type);
CREATE INDEX entity_edges_type_seen_idx ON entity_edges (edge_type, last_seen_unix_nanos DESC);
CREATE INDEX entity_edges_binding_queue_idx ON entity_edges (updated_at DESC)
    WHERE edge_type = 'same_identity_candidate' AND status = 'candidate';

-- Singleton watermark row for the drain (the threader_state / 0025 precedent). Watermarks are
-- WALL clock (events/conversations.updated_at), not capture time — survives late reprocessing
-- of old-capture backlogs (0024/0025 reasoning).
CREATE TABLE graph_state (
    id                      smallint PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    events_watermark        timestamptz NOT NULL DEFAULT to_timestamp(0),
    conversations_watermark timestamptz NOT NULL DEFAULT to_timestamp(0),
    config_hash             text,
    updated_at              timestamptz NOT NULL DEFAULT now()
);
INSERT INTO graph_state (id) VALUES (1);
