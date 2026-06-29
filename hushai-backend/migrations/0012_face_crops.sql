-- 0012_face_crops.sql — persist the cleaned/restored best-shot crop + recognition provenance.
--
-- The image-cleanup stage (crop-with-margin → super-resolve → blind-face-restore → align) recovers
-- low-quality faces that the old gate dropped. These additive, nullable columns let us (a) show the
-- RESTORED crop in the UI instead of re-cropping raw frames, and (b) record HOW a face was recognized
-- (restored?, pose) so calibration can audit the recover-then-embed path. All nullable / defaulted so
-- the change is partition-safe (person_segments is RANGE-partitioned monthly) and needs no backfill.

ALTER TABLE person_segments ADD COLUMN IF NOT EXISTS crop_uri      text;
ALTER TABLE person_segments ADD COLUMN IF NOT EXISTS is_best_shot  boolean NOT NULL DEFAULT false;
ALTER TABLE person_segments ADD COLUMN IF NOT EXISTS restored      boolean NOT NULL DEFAULT false;
ALTER TABLE person_segments ADD COLUMN IF NOT EXISTS yaw           real;
ALTER TABLE person_segments ADD COLUMN IF NOT EXISTS pitch         real;
ALTER TABLE person_segments ADD COLUMN IF NOT EXISTS quality_score real;

-- Fast "best stored crop for this person" lookup for the sample-face thumbnail.
CREATE INDEX IF NOT EXISTS person_segments_best_shot_idx
    ON person_segments (person_id, quality_score DESC)
    WHERE person_id IS NOT NULL AND crop_uri IS NOT NULL;
