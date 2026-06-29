-- 0015_alert_delivery_dedup.sql — hardening for the alert evaluator (roadmap A3), from the
-- adversarial review of the events/alerts work. Two changes:
--
--  1. UNIQUE(rule_id, event_id, channel) on alert_deliveries. The evaluator's `INSERT … SELECT …
--     ON CONFLICT (rule_id, event_id, channel) DO NOTHING` (hushai-worker/src/alerts.rs) relies on
--     this index to be IDEMPOTENT and RACE-SAFE: an event fires a given rule's given channel AT MOST
--     ONCE. Without it, (a) re-processing/backfilling a segment re-UPSERTs the same event_id and the
--     evaluator would mint a duplicate delivery dated now(), and (b) the cooldown is a check-then-
--     insert, so two concurrent audio-worker tasks evaluating the SAME continuing event (same
--     event_id, UPSERTed by dedup_key) could both pass the `NOT EXISTS` and double-fire. The unique
--     index turns the loser into a no-op. (NULLs are distinct in a btree unique index, so any future
--     non-evaluator deliveries with a NULL rule_id/event_id are unaffected.)
--
--  2. alert_deliveries.event_id FK → ON DELETE SET NULL (was CASCADE). The delivery row carries
--     DENORMALIZED event facets (device_id/event_type/severity/subject_label) precisely so the feed +
--     audit trail SURVIVE an event purge (0014's own comment says so); CASCADE contradicted that by
--     deleting the delivery when its event is retention-purged. SET NULL keeps the notification
--     history with its denormalized snapshot. (rule_id stays CASCADE: deleting a rule is an explicit
--     admin action that tears down its deliveries, per devices.rs's delete_rule contract.)

CREATE UNIQUE INDEX alert_deliveries_rule_event_channel_idx
    ON alert_deliveries (rule_id, event_id, channel);

ALTER TABLE alert_deliveries DROP CONSTRAINT alert_deliveries_event_id_fkey;
ALTER TABLE alert_deliveries
    ADD CONSTRAINT alert_deliveries_event_id_fkey
    FOREIGN KEY (event_id) REFERENCES events (event_id) ON DELETE SET NULL;
