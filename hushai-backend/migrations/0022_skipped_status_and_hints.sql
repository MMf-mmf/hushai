-- 'skipped' becomes a first-class TERMINAL status on both AI work queues
-- (pending | processing | done | error | skipped): the content gates decided there is
-- nothing to infer (static video / silent audio), so the segment never needs a worker.
--
-- No DDL beyond new columns is required: status is free text (0002/0009 define no CHECK
-- constraint), and both the claim queries and the 0020 partial claimable indexes enumerate
-- the claimable statuses ('pending','error','processing') explicitly — a 'skipped' row is
-- never claimable and costs nothing at claim time. `reconcile_missing_speaker_segments`
-- matches 'done' only, so skipped audio is never resurrected on worker restart.
--
-- skip_reason encodes WHO decided and why:
--   'silent_hint' / 'static_hint'  — ingest gate, from device-reported attrs hints
--   'silent_gate' / 'static_gate'  — worker backstop gate (unhinted sources)
--
-- hint_audit marks rows the ingest gate WOULD have skipped but enqueued anyway as an
-- audit sample; the worker fills audit_verdict ('agree' = its own gate also skips,
-- 'disagree' = it found real content — the device hints are miscalibrated or lying).
--
-- measured_* are worker-side calibration telemetry (the gate inputs it computed),
-- persisted on done AND skipped marks. Device hint values are NOT denormalized here —
-- they live in segments.attrs; calibration joins the two.

ALTER TABLE segment_transcription_status
    ADD COLUMN IF NOT EXISTS skip_reason          text,
    ADD COLUMN IF NOT EXISTS hint_audit           boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS audit_verdict        text,
    ADD COLUMN IF NOT EXISTS measured_rms         real,
    ADD COLUMN IF NOT EXISTS measured_speech_secs real;

ALTER TABLE segment_vision_status
    ADD COLUMN IF NOT EXISTS skip_reason              text,
    ADD COLUMN IF NOT EXISTS hint_audit               boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS audit_verdict            text,
    ADD COLUMN IF NOT EXISTS measured_motion_distance real;
