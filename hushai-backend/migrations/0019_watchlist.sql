-- 0019_watchlist.sql — "People/Plates of Interest" (roadmap A6), the VSaaS-giant watchlist feature
-- (Verkada "People of Interest", Rhombus "Faces of Interest"). Marking a person/plate "of interest"
-- means "alert me whenever they're seen".
--
-- It REUSES the alert engine instead of adding a parallel path: each watchlist entry owns a managed
-- `alert_rules` row scoped to that subject (subject_type + subject_ids=[id], min_severity='info' so
-- ANY sighting fires, feed channel). The A3 evaluator already fires on a subject_ids match, so a
-- watched subject's `known_person`/`unknown_person`/`plate_*` event lands in the feed automatically —
-- no worker/producer change. `rule_id` links the entry to its managed rule (ON DELETE SET NULL so a
-- manually-deleted rule just unlinks; the watchlist row stays the source of truth).

CREATE TABLE watchlist (
    watch_id     uuid PRIMARY KEY,
    subject_type text NOT NULL,                         -- 'person' | 'plate'
    subject_id   uuid NOT NULL,
    label        text,                                  -- denormalized display at add time (re-resolved on read)
    reason       text,                                  -- operator note ("BOLO", "VIP", …)
    rule_id      uuid REFERENCES alert_rules (rule_id) ON DELETE SET NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);

-- One watch per subject (race-safe add; idempotent toggle).
CREATE UNIQUE INDEX watchlist_subject_idx ON watchlist (subject_type, subject_id);
