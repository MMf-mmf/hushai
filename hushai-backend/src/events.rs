//! The proactive layer: events, alert rules, and the notification feed (the VSaaS-giant parity
//! surface — see `docs/feature-parity-roadmap.md`, Pillar A). This module owns the read/CRUD HTTP
//! surface (`/v1/events*`, `/v1/alert-rules*`) and the [`record_event`] producer helper that the
//! worker calls as it materializes events from detections.
//!
//! Runtime sqlx (`query`/`query_as` + `.bind`, `Row::get`), NOT the `query!` macros — same reason
//! as devices.rs/speakers.rs/persons.rs: these tables aren't in the committed `.sqlx/` cache.
//! Bearer-authenticated and proxied through the viewer exactly like `/v1/devices*`.
//!
//! Schema + design rationale: `migrations/0014_events_and_alerts.sql`. The rule EVALUATOR (matching
//! events against rules, cooldown, writing deliveries) is built in the worker (roadmap A3); this
//! module provides the storage + API both sides share.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::IngestError;
use crate::state::AppState;

/// Allowed severities, lowest→highest. The evaluator's `min_severity` gate compares by index.
pub const SEVERITIES: [&str; 3] = ["info", "warning", "critical"];

/// Severity rank (0=info..2=critical); unknown → 0 so a bad value never silently escalates.
pub fn severity_rank(s: &str) -> usize {
    SEVERITIES.iter().position(|&x| x == s).unwrap_or(0)
}

fn validate_severity(s: &str) -> Result<(), IngestError> {
    if SEVERITIES.contains(&s) {
        Ok(())
    } else {
        Err(IngestError::BadRequest(format!(
            "severity must be one of {SEVERITIES:?}, got {s:?}"
        )))
    }
}

// ===========================================================================
// events — the materialized event stream
// ===========================================================================

#[derive(Debug, Serialize)]
pub struct EventRow {
    pub event_id: Uuid,
    pub device_id: Option<String>,
    pub event_type: String,
    pub severity: String,
    pub subject_type: Option<String>,
    pub subject_id: Option<Uuid>,
    pub subject_label: Option<String>,
    pub segment_id: Option<Uuid>,
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
    pub score: Option<f32>,
    pub metadata: JsonValue,
    pub created_unix_nanos: i64,
}

fn event_from_row(r: &sqlx::postgres::PgRow) -> EventRow {
    EventRow {
        event_id: r.get("event_id"),
        device_id: r.get("device_id"),
        event_type: r.get("event_type"),
        severity: r.get("severity"),
        subject_type: r.get("subject_type"),
        subject_id: r.get("subject_id"),
        subject_label: r.get("subject_label"),
        segment_id: r.get("segment_id"),
        start_unix_nanos: r.get("start_unix_nanos"),
        end_unix_nanos: r.get("end_unix_nanos"),
        score: r.get("score"),
        metadata: r.get("metadata"),
        created_unix_nanos: r.get("created_unix_nanos"),
    }
}

#[derive(Debug, Deserialize)]
pub struct EventQuery {
    pub device_id: Option<String>,
    pub event_type: Option<String>,
    pub severity: Option<String>,
    pub subject_type: Option<String>,
    pub subject_id: Option<Uuid>,
    pub since_unix_nanos: Option<i64>,
    pub until_unix_nanos: Option<i64>,
    pub limit: Option<i64>,
}

/// `GET /v1/events` — newest-first event feed with optional facet/time filters. Every filter is
/// AND-ed; NULL filter = "any". `limit` is clamped to [1, 500] (default 100).
pub async fn list_events(
    State(st): State<AppState>,
    Query(q): Query<EventQuery>,
) -> Result<Json<Vec<EventRow>>, IngestError> {
    if let Some(s) = &q.severity {
        validate_severity(s)?;
    }
    let limit = q.limit.unwrap_or(100).clamp(1, 500);

    // One static query with NULL-guarded predicates ($n IS NULL OR col = $n) so the planner can
    // still use events_time_idx for the ORDER BY while every filter is optional.
    let rows = sqlx::query(
        r#"
        SELECT
            event_id, device_id, event_type, severity, subject_type, subject_id,
            subject_label, segment_id, start_unix_nanos, end_unix_nanos, score, metadata,
            (extract(epoch FROM created_at) * 1e9)::bigint AS created_unix_nanos
        FROM events
        WHERE ($1::text  IS NULL OR device_id    = $1)
          AND ($2::text  IS NULL OR event_type   = $2)
          -- severity is a FLOOR (the UI labels it "Min severity"): show this level and higher.
          AND ($3::text  IS NULL OR
               COALESCE(array_position(ARRAY['info','warning','critical'], severity), 1)
               >= COALESCE(array_position(ARRAY['info','warning','critical'], $3), 1))
          AND ($4::text  IS NULL OR subject_type = $4)
          AND ($5::uuid  IS NULL OR subject_id   = $5)
          AND ($6::bigint IS NULL OR start_unix_nanos >= $6)
          AND ($7::bigint IS NULL OR start_unix_nanos <  $7)
        ORDER BY start_unix_nanos DESC
        LIMIT $8
        "#,
    )
    .bind(&q.device_id)
    .bind(&q.event_type)
    .bind(&q.severity)
    .bind(&q.subject_type)
    .bind(q.subject_id)
    .bind(q.since_unix_nanos)
    .bind(q.until_unix_nanos)
    .bind(limit)
    .fetch_all(&st.pool)
    .await?;

    Ok(Json(rows.iter().map(event_from_row).collect()))
}

/// A sessionized event to materialize. The worker (roadmap A3) builds these from detections and
/// calls [`record_event`]; `dedup_key` makes re-processing idempotent (extends, never duplicates).
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub device_id: Option<String>,
    pub event_type: String,
    pub severity: String,
    pub subject_type: Option<String>,
    pub subject_id: Option<Uuid>,
    pub subject_label: Option<String>,
    pub segment_id: Option<Uuid>,
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
    pub score: Option<f32>,
    pub metadata: JsonValue,
    pub dedup_key: Option<String>,
}

/// Insert (or extend) a sessionized event. With a `dedup_key`, a repeated observation UPSERTs the
/// existing row — pushing `end_unix_nanos` forward and keeping the best `score` — so the same
/// continuous appearance stays ONE event no matter how many 2s segments cover it. Returns the
/// event_id. Idempotent + safe to call from concurrent worker tasks (the partial unique index
/// serializes the conflict). The caller is responsible for evaluating alert rules against the
/// returned event (the worker does this right after; roadmap A3).
pub async fn record_event(pool: &PgPool, ev: &NewEvent) -> Result<Uuid, sqlx::Error> {
    let event_id = Uuid::now_v7();
    let row = sqlx::query(
        r#"
        INSERT INTO events (
            event_id, device_id, event_type, severity, subject_type, subject_id,
            subject_label, segment_id, start_unix_nanos, end_unix_nanos, score, metadata, dedup_key
        )
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
        ON CONFLICT (dedup_key) WHERE dedup_key IS NOT NULL
        DO UPDATE SET
            end_unix_nanos = GREATEST(events.end_unix_nanos, EXCLUDED.end_unix_nanos),
            -- score: GREATEST ignores NULLs, so a NULL-scored regime (e.g. speech) and a
            -- numeric-scored regime never mix under one dedup_key prefix — they have disjoint keys.
            score          = GREATEST(events.score, EXCLUDED.score),
            -- Severity ESCALATES to the worst seen in the session bucket (so a negative-sentiment or
            -- otherwise-elevated later sighting isn't lost to first-write-wins); event_type +
            -- metadata take the LATEST sighting (so a person named mid-bucket flips
            -- unknown_person→known_person, and counts reflect a real recent segment, not a stale one).
            severity       = (ARRAY['info','warning','critical'])[
                GREATEST(
                    COALESCE(array_position(ARRAY['info','warning','critical'], events.severity), 1),
                    COALESCE(array_position(ARRAY['info','warning','critical'], EXCLUDED.severity), 1)
                )],
            event_type     = EXCLUDED.event_type,
            subject_label  = COALESCE(EXCLUDED.subject_label, events.subject_label),
            metadata       = EXCLUDED.metadata,
            updated_at     = now()
        RETURNING event_id
        "#,
    )
    .bind(event_id)
    .bind(&ev.device_id)
    .bind(&ev.event_type)
    .bind(&ev.severity)
    .bind(&ev.subject_type)
    .bind(ev.subject_id)
    .bind(&ev.subject_label)
    .bind(ev.segment_id)
    .bind(ev.start_unix_nanos)
    .bind(ev.end_unix_nanos)
    .bind(ev.score)
    .bind(&ev.metadata)
    .bind(&ev.dedup_key)
    .fetch_one(pool)
    .await?;
    Ok(row.get("event_id"))
}

// ===========================================================================
// alert_rules — operator-defined "notify me when…"
// ===========================================================================

#[derive(Debug, Serialize)]
pub struct AlertRule {
    pub rule_id: Uuid,
    pub name: String,
    pub enabled: bool,
    pub event_types: Vec<String>,
    pub device_ids: Vec<String>,
    pub subject_type: Option<String>,
    pub subject_ids: Vec<Uuid>,
    pub min_severity: String,
    pub time_start_minutes: Option<i32>,
    pub time_end_minutes: Option<i32>,
    pub days_of_week: Vec<i32>,
    pub tz: String,
    pub cooldown_secs: i32,
    pub channels: JsonValue,
}

fn rule_from_row(r: &sqlx::postgres::PgRow) -> AlertRule {
    AlertRule {
        rule_id: r.get("rule_id"),
        name: r.get("name"),
        enabled: r.get("enabled"),
        event_types: r.get("event_types"),
        device_ids: r.get("device_ids"),
        subject_type: r.get("subject_type"),
        subject_ids: r.get("subject_ids"),
        min_severity: r.get("min_severity"),
        time_start_minutes: r.get("time_start_minutes"),
        time_end_minutes: r.get("time_end_minutes"),
        days_of_week: r.get("days_of_week"),
        tz: r.get("tz"),
        cooldown_secs: r.get("cooldown_secs"),
        channels: r.get("channels"),
    }
}

/// Create/replace payload. PATCH is a FULL replace of the editable fields (the editor loads a rule,
/// mutates it, and saves the whole thing back) — this avoids the "null = clear vs. leave" ambiguity
/// a partial update has with the nullable window columns. Omitted optionals fall back to the column
/// defaults on create; on update an omitted optional is written as its default/NULL.
#[derive(Debug, Deserialize)]
pub struct AlertRuleInput {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub event_types: Vec<String>,
    #[serde(default)]
    pub device_ids: Vec<String>,
    pub subject_type: Option<String>,
    #[serde(default)]
    pub subject_ids: Vec<Uuid>,
    #[serde(default = "default_severity")]
    pub min_severity: String,
    pub time_start_minutes: Option<i32>,
    pub time_end_minutes: Option<i32>,
    #[serde(default)]
    pub days_of_week: Vec<i32>,
    #[serde(default = "default_tz")]
    pub tz: String,
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: i32,
    pub channels: Option<JsonValue>,
}

fn default_true() -> bool {
    true
}
fn default_severity() -> String {
    "info".to_string()
}
fn default_tz() -> String {
    "UTC".to_string()
}
fn default_cooldown() -> i32 {
    300
}

impl AlertRuleInput {
    fn validate(&self) -> Result<(), IngestError> {
        if self.name.trim().is_empty() {
            return Err(IngestError::BadRequest("name must not be empty".into()));
        }
        validate_severity(&self.min_severity)?;
        for m in [self.time_start_minutes, self.time_end_minutes].into_iter().flatten() {
            if !(0..1440).contains(&m) {
                return Err(IngestError::BadRequest(
                    "time_*_minutes must be in [0,1440)".into(),
                ));
            }
        }
        // A window needs both ends or neither (one alone is ambiguous).
        if self.time_start_minutes.is_some() != self.time_end_minutes.is_some() {
            return Err(IngestError::BadRequest(
                "time window needs both time_start_minutes and time_end_minutes, or neither".into(),
            ));
        }
        // A zero-width window [t,t) matches nothing (the half-open predicate is never satisfied),
        // so it would silently disable the rule — reject it. (For "always on", omit both ends.)
        if let (Some(a), Some(b)) = (self.time_start_minutes, self.time_end_minutes) {
            if a == b {
                return Err(IngestError::BadRequest(
                    "time_start_minutes and time_end_minutes must differ (a zero-width window never matches; omit both for always-on)".into(),
                ));
            }
        }
        for d in &self.days_of_week {
            if !(0..=6).contains(d) {
                return Err(IngestError::BadRequest("days_of_week entries must be 0..6".into()));
            }
        }
        if self.cooldown_secs < 0 {
            return Err(IngestError::BadRequest("cooldown_secs must be >= 0".into()));
        }
        // Validate the channel fan-out: a JSON array of objects, each with a known `type`, and a
        // webhook must carry an http(s) `url`. Defense-in-depth — the worker's delivery loop (A4)
        // will POST to these, so an operator (or anyone with the session) must not be able to store
        // a bogus/non-http target. (Host-level SSRF hardening is the sender's job.)
        if let Some(ch) = &self.channels {
            let arr = ch
                .as_array()
                .ok_or_else(|| IngestError::BadRequest("channels must be a JSON array".into()))?;
            for c in arr {
                let ty = c.get("type").and_then(|v| v.as_str()).ok_or_else(|| {
                    IngestError::BadRequest("each channel needs a string \"type\"".into())
                })?;
                match ty {
                    "feed" | "push" => {}
                    "webhook" => {
                        let url = c.get("url").and_then(|v| v.as_str()).unwrap_or("");
                        if !(url.starts_with("http://") || url.starts_with("https://")) {
                            return Err(IngestError::BadRequest(
                                "webhook channel needs an http(s) \"url\"".into(),
                            ));
                        }
                    }
                    other => {
                        return Err(IngestError::BadRequest(format!(
                            "unknown channel type {other:?} (expected feed | webhook | push)"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn channels_or_default(&self) -> JsonValue {
        self.channels
            .clone()
            .unwrap_or_else(|| serde_json::json!([{ "type": "feed" }]))
    }
}

/// `GET /v1/alert-rules` — list all rules, newest first.
pub async fn list_rules(State(st): State<AppState>) -> Result<Json<Vec<AlertRule>>, IngestError> {
    let rows = sqlx::query(
        r#"
        SELECT rule_id, name, enabled, event_types, device_ids, subject_type, subject_ids,
               min_severity, time_start_minutes, time_end_minutes, days_of_week, tz,
               cooldown_secs, channels
        FROM alert_rules ORDER BY created_at DESC
        "#,
    )
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows.iter().map(rule_from_row).collect()))
}

/// `POST /v1/alert-rules` — create a rule.
pub async fn create_rule(
    State(st): State<AppState>,
    Json(input): Json<AlertRuleInput>,
) -> Result<Json<AlertRule>, IngestError> {
    input.validate()?;
    let rule_id = Uuid::now_v7();
    let row = sqlx::query(
        r#"
        INSERT INTO alert_rules (
            rule_id, name, enabled, event_types, device_ids, subject_type, subject_ids,
            min_severity, time_start_minutes, time_end_minutes, days_of_week, tz, cooldown_secs, channels
        )
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
        RETURNING rule_id, name, enabled, event_types, device_ids, subject_type, subject_ids,
                  min_severity, time_start_minutes, time_end_minutes, days_of_week, tz,
                  cooldown_secs, channels
        "#,
    )
        .bind(rule_id)
        .bind(input.name.trim())
        .bind(input.enabled)
        .bind(&input.event_types)
        .bind(&input.device_ids)
        .bind(&input.subject_type)
        .bind(&input.subject_ids)
        .bind(&input.min_severity)
        .bind(input.time_start_minutes)
        .bind(input.time_end_minutes)
        .bind(&input.days_of_week)
        .bind(&input.tz)
        .bind(input.cooldown_secs)
        .bind(input.channels_or_default())
        .fetch_one(&st.pool)
        .await?;
    Ok(Json(rule_from_row(&row)))
}

/// `PATCH /v1/alert-rules/{id}` — full replace of the editable fields.
pub async fn update_rule(
    State(st): State<AppState>,
    Path(rule_id): Path<Uuid>,
    Json(input): Json<AlertRuleInput>,
) -> Result<Json<AlertRule>, IngestError> {
    input.validate()?;
    let row = sqlx::query(
        r#"
        UPDATE alert_rules SET
            name = $2, enabled = $3, event_types = $4, device_ids = $5, subject_type = $6,
            subject_ids = $7, min_severity = $8, time_start_minutes = $9, time_end_minutes = $10,
            days_of_week = $11, tz = $12, cooldown_secs = $13, channels = $14, updated_at = now()
        WHERE rule_id = $1
        RETURNING rule_id, name, enabled, event_types, device_ids, subject_type, subject_ids,
                  min_severity, time_start_minutes, time_end_minutes, days_of_week, tz,
                  cooldown_secs, channels
        "#,
    )
        .bind(rule_id)
        .bind(input.name.trim())
        .bind(input.enabled)
        .bind(&input.event_types)
        .bind(&input.device_ids)
        .bind(&input.subject_type)
        .bind(&input.subject_ids)
        .bind(&input.min_severity)
        .bind(input.time_start_minutes)
        .bind(input.time_end_minutes)
        .bind(&input.days_of_week)
        .bind(&input.tz)
        .bind(input.cooldown_secs)
        .bind(input.channels_or_default())
        .fetch_optional(&st.pool)
        .await?
        .ok_or(IngestError::NotFound("alert rule"))?;
    Ok(Json(rule_from_row(&row)))
}

/// `DELETE /v1/alert-rules/{id}` — delete a rule (its deliveries cascade).
pub async fn delete_rule(
    State(st): State<AppState>,
    Path(rule_id): Path<Uuid>,
) -> Result<StatusJson, IngestError> {
    let n = sqlx::query("DELETE FROM alert_rules WHERE rule_id = $1")
        .bind(rule_id)
        .execute(&st.pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(IngestError::NotFound("alert rule"));
    }
    Ok(StatusJson(serde_json::json!({ "deleted": rule_id })))
}

/// Small JSON-body wrapper so a handler can return an ad-hoc `{...}` with a 200.
pub struct StatusJson(JsonValue);
impl axum::response::IntoResponse for StatusJson {
    fn into_response(self) -> axum::response::Response {
        Json(self.0).into_response()
    }
}

// ===========================================================================
// alert_deliveries — the notification feed (read + acknowledge)
// ===========================================================================

#[derive(Debug, Serialize)]
pub struct FeedItem {
    pub delivery_id: Uuid,
    pub rule_id: Option<Uuid>,
    pub event_id: Option<Uuid>,
    pub channel: String,
    pub status: String,
    pub device_id: Option<String>,
    pub event_type: Option<String>,
    pub severity: Option<String>,
    pub subject_label: Option<String>,
    pub created_unix_nanos: i64,
    /// The underlying event's start time (for deep-linking the timeline to the *footage* moment,
    /// not the alert-fire time). NULL when the event was purged (FK SET NULL).
    pub event_start_unix_nanos: Option<i64>,
    pub acknowledged: bool,
}

#[derive(Debug, Deserialize)]
pub struct FeedQuery {
    /// 'pending' | 'sent' | 'failed' | 'acknowledged'; NULL = any.
    pub status: Option<String>,
    /// Only the in-app feed channel by default (webhook/push are machine deliveries).
    pub channel: Option<String>,
    pub limit: Option<i64>,
}

/// `GET /v1/events/feed` — the in-app notification feed (newest first). Defaults to the `feed`
/// channel so the UI shows human-facing notifications, not webhook/push send records.
pub async fn list_feed(
    State(st): State<AppState>,
    Query(q): Query<FeedQuery>,
) -> Result<Json<Vec<FeedItem>>, IngestError> {
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let channel = q.channel.clone().or_else(|| Some("feed".to_string()));
    let rows = sqlx::query(
        r#"
        SELECT
            d.delivery_id, d.rule_id, d.event_id, d.channel, d.status, d.device_id, d.event_type,
            d.severity, d.subject_label,
            (extract(epoch FROM d.created_at) * 1e9)::bigint AS created_unix_nanos,
            e.start_unix_nanos AS event_start_unix_nanos,
            d.acknowledged_at IS NOT NULL AS acknowledged
        FROM alert_deliveries d
        LEFT JOIN events e ON e.event_id = d.event_id
        WHERE ($1::text IS NULL OR d.channel = $1)
          AND ($2::text IS NULL OR d.status = $2)
        ORDER BY d.created_at DESC
        LIMIT $3
        "#,
    )
    .bind(&channel)
    .bind(&q.status)
    .bind(limit)
    .fetch_all(&st.pool)
    .await?;

    Ok(Json(
        rows.iter()
            .map(|r| FeedItem {
                delivery_id: r.get("delivery_id"),
                rule_id: r.get("rule_id"),
                event_id: r.get("event_id"),
                channel: r.get("channel"),
                status: r.get("status"),
                device_id: r.get("device_id"),
                event_type: r.get("event_type"),
                severity: r.get("severity"),
                subject_label: r.get("subject_label"),
                created_unix_nanos: r.get("created_unix_nanos"),
                event_start_unix_nanos: r.get("event_start_unix_nanos"),
                acknowledged: r.get("acknowledged"),
            })
            .collect(),
    ))
}

/// `POST /v1/events/feed/{id}/ack` — mark a feed notification acknowledged (read/dismissed).
pub async fn ack_delivery(
    State(st): State<AppState>,
    Path(delivery_id): Path<Uuid>,
) -> Result<StatusJson, IngestError> {
    let n = sqlx::query(
        "UPDATE alert_deliveries SET status = 'acknowledged', acknowledged_at = now() \
         WHERE delivery_id = $1",
    )
    .bind(delivery_id)
    .execute(&st.pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(IngestError::NotFound("delivery"));
    }
    Ok(StatusJson(serde_json::json!({ "acknowledged": delivery_id })))
}
