-- Device management: a human-facing name and an optional retention policy per device.
--
-- Until now `devices` carried only the raw client-assigned `device_id` (e.g. "iphone-bob")
-- and was append-only — there was no way to rename a device, delete its footage, or cap how
-- long it is kept. This adds the two columns the management surface needs; the deletion logic
-- itself is pure DML (see hushai-backend/src/devices.rs) and needs no schema change because
-- `DELETE FROM segments` already cascades every derived child table (0004/0006/0009) and the
-- 0005 `segments_device_capture_start_idx` already backs the window delete + usage queries.
--
-- Both columns are additive and nullable, so this is a metadata-only ALTER (no table rewrite)
-- and `IF NOT EXISTS` keeps it idempotent.

ALTER TABLE devices ADD COLUMN IF NOT EXISTS display_name text;

-- NULL = no retention policy (keep forever). N>=1 = keep the last N days; the backend's
-- retention task deletes segments whose footage ends before now()-N days. The CHECK forbids
-- a 0/negative window (which would delete everything, including live footage).
ALTER TABLE devices ADD COLUMN IF NOT EXISTS retention_days integer;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'devices_retention_days_positive'
    ) THEN
        ALTER TABLE devices
            ADD CONSTRAINT devices_retention_days_positive
            CHECK (retention_days IS NULL OR retention_days >= 1);
    END IF;
END $$;
