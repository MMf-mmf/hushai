//! Append-only audit log (roadmap B6): who did what, when, from where, with what outcome. Shared by
//! the viewer (the gateway that writes most entries) and the backend (which owns the schema + the
//! read API). Writes are BEST-EFFORT — an audit failure logs and is swallowed, never failing the
//! action being audited. Runtime sqlx + `IngestError`, bearer-authed + viewer-proxied like the other
//! `/v1/*` admin surfaces. See `migrations/0017_audit_log.sql`.

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::IngestError;
use crate::state::AppState;

/// One audit entry to record. Build via [`proxied`] for a gateway-proxied HTTP mutation, or
/// construct directly for a non-HTTP event (login/logout/export).
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub actor: String,
    pub actor_ip: Option<String>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub method: Option<String>,
    pub path: Option<String>,
    pub status: Option<i32>,
    pub detail: JsonValue,
}

impl AuditEntry {
    /// An audit entry for a gateway-proxied mutating HTTP request. Derives a semantic `action` +
    /// `target_type`/`target_id` from (method, path) via [`classify`].
    pub fn proxied(actor: impl Into<String>, actor_ip: Option<String>, method: &str, path: &str, status: u16) -> Self {
        let (action, target_type, target_id) = classify(method, path);
        AuditEntry {
            actor: actor.into(),
            actor_ip,
            action,
            target_type,
            target_id,
            method: Some(method.to_string()),
            path: Some(path.to_string()),
            status: Some(status as i32),
            detail: JsonValue::Object(Default::default()),
        }
    }

    /// A non-HTTP audit event (e.g. `auth.login`, `footage.export`).
    pub fn event(actor: impl Into<String>, actor_ip: Option<String>, action: impl Into<String>) -> Self {
        AuditEntry {
            actor: actor.into(),
            actor_ip,
            action: action.into(),
            target_type: None,
            target_id: None,
            method: None,
            path: None,
            status: None,
            detail: JsonValue::Object(Default::default()),
        }
    }

    pub fn with_target(mut self, target_type: impl Into<String>, target_id: impl Into<String>) -> Self {
        self.target_type = Some(target_type.into());
        self.target_id = Some(target_id.into());
        self
    }
    pub fn with_detail(mut self, detail: JsonValue) -> Self {
        self.detail = detail;
        self
    }
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status as i32);
        self
    }
}

/// Map (method, path) → (action, target_type, target_id) for the proxied admin surfaces.
///
/// POSITION-AWARE (not last-segment-based): the device id lives at seg[2] and its sub-action at
/// seg[3], so a device literally NAMED `footage`/`retention` (device_id is free client text) is NOT
/// mistaken for a footage purge. Collection-level literal routes (`recluster`, `merge-group`,
/// `unattributed`, …) at seg[2] are recognized so they aren't recorded as ids, and the real ack
/// delivery id (`/v1/events/feed/{id}/ack`) is captured from its actual position.
pub fn classify(method: &str, path: &str) -> (String, Option<String>, Option<String>) {
    let p = path.split('?').next().unwrap_or(path).trim_end_matches('/');
    let seg: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    let m = method.to_uppercase();

    let collection = seg.get(1).copied().unwrap_or("");
    let s2 = seg.get(2).copied(); // id slot, OR a collection-level literal route name
    let s3 = seg.get(3).copied(); // sub-action slot (after an id), e.g. .../{id}/footage
    let s4 = seg.get(4).copied();

    let (entity, tt): (&str, &str) = match collection {
        "devices" => ("device", "device"),
        "speakers" => ("speaker", "speaker"),
        "persons" => ("person", "person"),
        "plates" => ("plate", "plate"),
        "alert-rules" => ("alert_rule", "alert_rule"),
        "events" => ("event", "event"),
        _ => return (format!("{m} {p}"), None, None),
    };
    let tts = Some(tt.to_string());
    // seg[2] is an id only when it's not a collection-level literal route name.
    let id = s2.filter(|s| !is_collection_literal(s)).map(str::to_string);

    match (collection, m.as_str(), s2, s3, s4) {
        // devices: sub-action at seg[3], AFTER the id at seg[2]
        ("devices", "DELETE", Some(_), Some("footage"), None) => ("footage.delete".into(), tts, id),
        ("devices", "POST", Some(_), Some("footage"), Some("bulk-delete")) => ("footage.bulk_delete".into(), tts, id),
        ("devices", "PUT", Some(_), Some("retention"), None) => ("device.retention".into(), tts, id),
        ("devices", "DELETE", Some(_), None, None) => ("device.delete".into(), tts, id),
        ("devices", "PATCH", Some(_), None, None) => ("device.rename".into(), tts, id),

        // alert-rules
        ("alert-rules", "POST", None, None, None) => ("alert_rule.create".into(), tts, None),
        ("alert-rules", "PATCH", Some(_), None, None) => ("alert_rule.update".into(), tts, id),
        ("alert-rules", "DELETE", Some(_), None, None) => ("alert_rule.delete".into(), tts, id),

        // events ack: /v1/events/feed/{delivery_id}/ack — capture the delivery id from seg[3]
        ("events", "POST", Some("feed"), Some(did), Some("ack")) => {
            ("alert.ack".into(), tts, Some(did.to_string()))
        }

        // speaker catalog-level POSTs (the literal action sits at seg[2])
        ("speakers", "POST", Some("recluster"), _, _) => ("speaker.recluster".into(), tts, None),
        ("speakers", "POST", Some("recluster-deep"), _, _) => ("speaker.recluster_deep".into(), tts, None),
        ("speakers", "POST", Some("merge-group"), _, _) => ("speaker.merge_group".into(), tts, None),
        ("speakers", "POST", Some("unattributed"), Some("name"), _) => ("speaker.name_unattributed".into(), tts, None),

        // generic per-entity rename / merge (speakers / persons / plates)
        (_, "PATCH", Some(_), None, _) => (format!("{entity}.rename"), tts, id),
        (_, "POST", Some(_), Some("merge"), _) => (format!("{entity}.merge"), tts, id),

        // fallback: entity + lowercased method
        _ => (format!("{entity}.{}", m.to_lowercase()), tts, id),
    }
}

/// Literal collection-level route names that occupy seg[2] but are NOT ids.
fn is_collection_literal(s: &str) -> bool {
    matches!(
        s,
        "recluster" | "recluster-deep" | "merge-group" | "duplicates" | "unattributed" | "search" | "feed"
    )
}

/// Record an audit entry. Best-effort AND latency-bounded: the INSERT is awaited inline by every
/// caller (durable when the DB is healthy — no fire-and-forget loss), but wrapped in a short timeout
/// so a degraded audit DB can never hang or fail the action being audited (login/export/proxy).
pub async fn record(pool: &PgPool, entry: AuditEntry) {
    let insert = sqlx::query(
        r#"
        INSERT INTO audit_log
            (audit_id, actor, actor_ip, action, target_type, target_id, method, path, status, detail)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(&entry.actor)
    .bind(&entry.actor_ip)
    .bind(&entry.action)
    .bind(&entry.target_type)
    .bind(&entry.target_id)
    .bind(&entry.method)
    .bind(&entry.path)
    .bind(entry.status)
    .bind(&entry.detail)
    .execute(pool);
    match tokio::time::timeout(std::time::Duration::from_secs(3), insert).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, action = %entry.action, "audit: insert failed (continuing)"),
        Err(_) => tracing::warn!(action = %entry.action, "audit: insert timed out (continuing)"),
    }
}

/// True for the HTTP methods that mutate state (the ones worth auditing at the gateway).
pub fn is_mutating(method: &str) -> bool {
    matches!(method.to_uppercase().as_str(), "POST" | "PUT" | "PATCH" | "DELETE")
}

// ---------------------------------------------------------------------------
// GET /v1/audit — read the trail (newest first, filterable)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct AuditRow {
    pub audit_id: Uuid,
    pub ts_unix_nanos: i64,
    pub actor: String,
    pub actor_ip: Option<String>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub method: Option<String>,
    pub path: Option<String>,
    pub status: Option<i32>,
    pub detail: JsonValue,
}

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    pub action: Option<String>,
    pub actor: Option<String>,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub since_unix_nanos: Option<i64>,
    pub limit: Option<i64>,
}

pub async fn list_audit(
    State(st): State<AppState>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<Vec<AuditRow>>, IngestError> {
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let rows = sqlx::query(
        r#"
        SELECT audit_id,
               -- exact integer nanos (avoid float8 epoch*1e9 quantization); now() is µs-resolution.
               (extract(epoch FROM ts) * 1e6)::bigint * 1000 AS ts_unix_nanos,
               actor, actor_ip, action, target_type, target_id, method, path, status, detail
        FROM audit_log
        WHERE ($1::text IS NULL OR action = $1)
          AND ($2::text IS NULL OR actor = $2)
          AND ($3::text IS NULL OR target_type = $3)
          AND ($4::text IS NULL OR target_id = $4)
          -- sargable: compare on the ts column directly so the ts index can be used.
          AND ($5::bigint IS NULL OR ts >= to_timestamp($5 / 1e9))
        ORDER BY ts DESC, audit_id DESC   -- deterministic tiebreaker (now_v7 ids are monotonic)
        LIMIT $6
        "#,
    )
    .bind(&q.action)
    .bind(&q.actor)
    .bind(&q.target_type)
    .bind(&q.target_id)
    .bind(q.since_unix_nanos)
    .bind(limit)
    .fetch_all(&st.pool)
    .await?;

    Ok(Json(
        rows.iter()
            .map(|r| AuditRow {
                audit_id: r.get("audit_id"),
                ts_unix_nanos: r.get("ts_unix_nanos"),
                actor: r.get("actor"),
                actor_ip: r.get("actor_ip"),
                action: r.get("action"),
                target_type: r.get("target_type"),
                target_id: r.get("target_id"),
                method: r.get("method"),
                path: r.get("path"),
                status: r.get("status"),
                detail: r.get("detail"),
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn classifies_admin_actions() {
        let cases: &[(&str, &str, &str, Option<&str>, Option<&str>)] = &[
            // method, path, expected action, expected target_type, expected target_id
            ("DELETE", "/v1/devices/cam-1", "device.delete", Some("device"), Some("cam-1")),
            ("PATCH", "/v1/devices/cam-1", "device.rename", Some("device"), Some("cam-1")),
            ("PUT", "/v1/devices/cam-1/retention", "device.retention", Some("device"), Some("cam-1")),
            ("DELETE", "/v1/devices/cam-1/footage?tz=UTC&day=2024-01-01", "footage.delete", Some("device"), Some("cam-1")),
            ("POST", "/v1/devices/cam-1/footage/bulk-delete", "footage.bulk_delete", Some("device"), Some("cam-1")),
            ("POST", "/v1/alert-rules", "alert_rule.create", Some("alert_rule"), None),
            ("PATCH", "/v1/alert-rules/r1", "alert_rule.update", Some("alert_rule"), Some("r1")),
            ("DELETE", "/v1/alert-rules/r1", "alert_rule.delete", Some("alert_rule"), Some("r1")),
            ("POST", "/v1/events/feed/d1/ack", "alert.ack", Some("event"), Some("d1")),
            ("PATCH", "/v1/speakers/s1", "speaker.rename", Some("speaker"), Some("s1")),
            ("POST", "/v1/speakers/s1/merge", "speaker.merge", Some("speaker"), Some("s1")),
            ("POST", "/v1/speakers/merge-group", "speaker.merge_group", Some("speaker"), None),
            ("POST", "/v1/speakers/recluster", "speaker.recluster", Some("speaker"), None),
            ("POST", "/v1/speakers/recluster-deep", "speaker.recluster_deep", Some("speaker"), None),
            ("POST", "/v1/speakers/unattributed/name", "speaker.name_unattributed", Some("speaker"), None),
            ("PATCH", "/v1/persons/p1", "person.rename", Some("person"), Some("p1")),
            ("POST", "/v1/plates/x9/merge", "plate.merge", Some("plate"), Some("x9")),
            // COLLISION: a device literally named "footage" is a whole-device delete, NOT a purge.
            ("DELETE", "/v1/devices/footage", "device.delete", Some("device"), Some("footage")),
            ("PATCH", "/v1/devices/retention", "device.rename", Some("device"), Some("retention")),
            // trailing slash tolerated
            ("POST", "/v1/alert-rules/", "alert_rule.create", Some("alert_rule"), None),
        ];
        for (m, path, want_action, want_tt, want_id) in cases {
            let (action, tt, id) = classify(m, path);
            assert_eq!(&action, want_action, "action for {m} {path}");
            assert_eq!(tt.as_deref(), *want_tt, "target_type for {m} {path}");
            assert_eq!(id.as_deref(), *want_id, "target_id for {m} {path}");
        }
        // Unknown collection (e.g. rag) → generic "METHOD path" (we never audit these, but be safe).
        assert_eq!(classify("POST", "/v1/rag/chat").0, "POST /v1/rag/chat");
    }
}
