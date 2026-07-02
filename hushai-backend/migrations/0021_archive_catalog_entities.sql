-- 0021_archive_catalog_entities.sql — "Disregard" (archive) for catalog entities. The labeling
-- surfaces (Voices/People/Plates) accumulate unidentified entries the operator doesn't care
-- about; archiving moves them into a collapsed "Archived" section instead of the active list.
--
-- Archiving is DISPLAY-LEVEL ONLY (NULL = active):
--   * The worker's match/mint still attributes new voiceprints/faces/plate reads to archived
--     identities — excluding them from matching would just re-mint duplicate rows that
--     reappear under "Unidentified", defeating the point.
--   * RAG naming/rosters and the detections overlay are unchanged — chat answers about
--     history stay truthful.
--   * Watchlist is unaffected: an explicit watch is a stronger signal than a display-level
--     archive, so a watched+archived subject still alerts.
-- A timestamp (not a boolean) records WHEN the operator disregarded it and leaves room for a
-- future auto-purge policy. No index: these catalogs are low-row-count and lists return all
-- rows (clients group active/archived at render time).

ALTER TABLE speakers ADD COLUMN archived_at timestamptz;
ALTER TABLE persons ADD COLUMN archived_at timestamptz;
ALTER TABLE license_plates ADD COLUMN archived_at timestamptz;
