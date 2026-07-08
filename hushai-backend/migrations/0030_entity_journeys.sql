-- 0030_entity_journeys.sql — cross-camera journeys (Gotham.md §1.3, Pillar G5).
--
-- Shipped in Wave 1 so migration numbering is settled; STITCHED in Wave 4 by the events drain.
-- A subject's visit on device B starting within GRAPH_JOURNEY_GAP_SECS of its visit end on
-- device A extends the open journey (the 0025 link_group_id philosophy: LINK across devices,
-- never merge underlying data). Only journeys spanning >= 2 distinct devices persist —
-- single-device visits are already events/profiles territory. Open/closed mutability contract
-- copied from `conversations`. Speakers excluded in v1 (vision lanes only).
--
-- DERIVED DATA: rebuildable from events. Deleting a subject deletes its journeys in the same
-- transaction (§3 posture).
CREATE TABLE entity_journeys (
    journey_id            uuid PRIMARY KEY,
    subject_type          text NOT NULL CHECK (subject_type IN ('person','plate')), -- v1: vision lanes only
    subject_id            uuid NOT NULL,
    started_at_unix_nanos bigint NOT NULL,
    ended_at_unix_nanos   bigint NOT NULL,
    hop_count             integer NOT NULL,
    hops                  jsonb NOT NULL,   -- [{"device_id":..,"arrive_ns":..,"depart_ns":..,"event_id":..}]
    status                text NOT NULL DEFAULT 'open' CHECK (status IN ('open','closed')),
    dedup_key             text,             -- "journey:<subject_id>:<first_hop_bucket>"
    config_hash           text,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX entity_journeys_dedup_idx ON entity_journeys (dedup_key) WHERE dedup_key IS NOT NULL;
CREATE INDEX entity_journeys_subject_time_idx ON entity_journeys (subject_type, subject_id, started_at_unix_nanos DESC);
CREATE INDEX entity_journeys_open_idx ON entity_journeys (status) WHERE status = 'open';

-- camera_adjacency: a plain VIEW over consecutive hop pairs (the C4 floor-plan seed — no table
-- until C4 needs one). Counts observed A->B camera transitions across all closed journeys.
CREATE VIEW camera_adjacency AS
SELECT
    (hop ->> 'device_id')          AS from_device,
    (next_hop ->> 'device_id')     AS to_device,
    count(*)                       AS transitions,
    min((next_hop ->> 'arrive_ns')::bigint - (hop ->> 'depart_ns')::bigint) AS min_gap_nanos,
    max((next_hop ->> 'arrive_ns')::bigint - (hop ->> 'depart_ns')::bigint) AS max_gap_nanos
FROM entity_journeys j
CROSS JOIN LATERAL jsonb_array_elements(j.hops) WITH ORDINALITY AS a(hop, ord)
CROSS JOIN LATERAL jsonb_array_elements(j.hops) WITH ORDINALITY AS b(next_hop, ord2)
WHERE b.ord2 = a.ord + 1
  AND (hop ->> 'device_id') IS DISTINCT FROM (next_hop ->> 'device_id')
GROUP BY from_device, to_device;
