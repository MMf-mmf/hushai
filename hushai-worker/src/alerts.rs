//! Roadmap A3 — the ALERT EVALUATOR. Given one freshly-produced event, match it against every
//! enabled `alert_rules` row and write an `alert_deliveries` outbox row per (matching rule × channel),
//! honoring the rule's local time-of-day window, day-of-week gate, severity floor, and per-(rule,
//! subject) cooldown. Called by `events_producer::emit_all` right after `record_event`.
//!
//! ONE SQL STATEMENT (atomic, race-tolerant): all rule matching + the tz time-window + the cooldown
//! recency check + the channel fan-out happen server-side. The worker has no chrono-tz, so the
//! per-rule local time is computed in Postgres via `AT TIME ZONE r.tz` (each rule carries its own
//! IANA tz). `delivery_id` uses `gen_random_uuid()` (Postgres-core since PG13) so the single
//! INSERT…SELECT can mint ids for an unknown number of matched rows without a per-row Rust id.
//!
//! Cooldown is enforced by the `NOT EXISTS` recency check over `alert_deliveries.cooldown_key`
//! (= `<rule_id>:<subject>`). Because `record_event` returns the SAME event_id for a continuing
//! appearance (UPSERT by dedup_key), this evaluator may re-run against that id every ~2s segment;
//! the cooldown is the ONLY thing preventing a per-segment fire, so its key MUST be a stable
//! non-NULL string for anonymous subjects too — hence `COALESCE(subject_id::text, subject_type,
//! 'any')`. See `docs/feature-parity-roadmap.md` + AGENTS.md "Events & alerts".

use sqlx::PgPool;
use uuid::Uuid;

/// The match + cooldown + channel-fan-out statement. `$1` = event_id. Returns rows created.
const EVALUATE_SQL: &str = r#"
WITH ev AS (
    SELECT event_id, device_id, event_type, severity, subject_type, subject_id, subject_label,
           to_timestamp(start_unix_nanos / 1e9) AS ts_utc
    FROM events
    WHERE event_id = $1
),
evloc AS (
    SELECT
        ev.event_id, ev.device_id, ev.event_type, ev.severity, ev.subject_type, ev.subject_id,
        ev.subject_label,
        r.rule_id, r.cooldown_secs, r.channels,
        r.time_start_minutes AS ts_min, r.time_end_minutes AS te_min, r.days_of_week,
        (extract(hour   FROM (ev.ts_utc AT TIME ZONE r.tz)) * 60
       +  extract(minute FROM (ev.ts_utc AT TIME ZONE r.tz)))::int AS lmin,
        extract(dow FROM (ev.ts_utc AT TIME ZONE r.tz))::int        AS ldow
    FROM ev
    CROSS JOIN alert_rules r
    WHERE r.enabled
      AND jsonb_typeof(r.channels) = 'array'   -- skip a malformed (non-array) channels rather than error the whole eval
      AND (cardinality(r.event_types) = 0 OR ev.event_type = ANY(r.event_types))
      AND (cardinality(r.device_ids)  = 0 OR ev.device_id  = ANY(r.device_ids))
      AND (r.subject_type IS NULL OR r.subject_type = ev.subject_type)
      AND (cardinality(r.subject_ids) = 0 OR ev.subject_id = ANY(r.subject_ids))
      AND COALESCE(array_position(ARRAY['info','warning','critical'], ev.severity), 1)
          >= COALESCE(array_position(ARRAY['info','warning','critical'], r.min_severity), 1)
)
INSERT INTO alert_deliveries
    (delivery_id, rule_id, event_id, channel, status, target,
     device_id, event_type, severity, subject_label, cooldown_key)
SELECT
    gen_random_uuid(), e.rule_id, e.event_id, ch->>'type', 'pending',
    COALESCE(ch->>'url', ch->>'target', ch->>'token'),
    e.device_id, e.event_type, e.severity, e.subject_label,
    -- cooldown_key: per (rule, SUBJECT). For identified subjects that's the uuid; for anonymous
    -- ones (object_seen, uncatalogued plate_seen) subject_id is NULL, so fall back to subject_label
    -- (the object label / plate text) — otherwise ALL objects (or all uncatalogued plates) of a type
    -- would share one cooldown and a second distinct subject would be silently suppressed.
    e.rule_id::text || ':' || COALESCE(e.subject_id::text, e.subject_label, e.subject_type, 'any')
FROM evloc e
CROSS JOIN LATERAL jsonb_array_elements(e.channels) AS ch
WHERE
    -- valid channel object (skip a malformed one rather than violate channel NOT NULL)
    ch->>'type' IS NOT NULL
    -- local time-of-day window [start,end); both NULL = always-on; start>end wraps past midnight
    AND ( e.ts_min IS NULL OR e.te_min IS NULL
          OR ( e.ts_min <= e.te_min AND e.lmin >= e.ts_min AND e.lmin < e.te_min )
          OR ( e.ts_min >  e.te_min AND ( e.lmin >= e.ts_min OR e.lmin < e.te_min ) ) )
    -- day-of-week gate (0=Sun..6=Sat aligns with extract(dow)); {} = every day
    AND ( cardinality(e.days_of_week) = 0 OR e.ldow = ANY(e.days_of_week) )
    -- cooldown: skip if the same (rule,subject) fired within cooldown_secs (across DIFFERENT events,
    -- e.g. a later session bucket). Must mirror the cooldown_key composition in the SELECT exactly.
    AND NOT EXISTS (
        SELECT 1 FROM alert_deliveries d
        WHERE d.cooldown_key =
              e.rule_id::text || ':' || COALESCE(e.subject_id::text, e.subject_label, e.subject_type, 'any')
          AND d.created_at > now() - make_interval(secs => e.cooldown_secs)
    )
-- At-most-once per (rule, event, channel): makes the evaluator IDEMPOTENT (a reprocess/backfill that
-- re-UPSERTs the same event_id and re-evaluates is a no-op) AND closes the check-then-insert race
-- (two concurrent audio workers evaluating the SAME continuing event both pass the cooldown NOT
-- EXISTS, but the unique index lets only one delivery per channel land). Also collapses a rule that
-- accidentally lists the same channel twice. Backed by alert_deliveries_rule_event_channel_idx (0015).
ON CONFLICT (rule_id, event_id, channel) DO NOTHING
"#;

/// Evaluate alert rules for one event; returns the number of `alert_deliveries` rows created.
/// An invalid rule tz raises a Postgres error that aborts (only) this statement — caught by the
/// caller (`events_producer::emit_all`), logged, and the alert dropped; core processing is unaffected.
pub async fn evaluate(pool: &PgPool, event_id: Uuid) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(EVALUATE_SQL).bind(event_id).execute(pool).await?;
    Ok(res.rows_affected())
}
