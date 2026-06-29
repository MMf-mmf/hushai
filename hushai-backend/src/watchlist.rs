//! Watchlists — "People/Plates of Interest" (roadmap A6). Marking a person/plate of interest means
//! "alert me whenever they're seen". Implemented by REUSING the alert engine: each watchlist entry
//! owns a managed `alert_rules` row (subject_type + subject_ids=[id], min_severity='info', feed
//! channel), so the A3 evaluator fires on any sighting of that subject — no worker change.
//!
//! Runtime sqlx + `IngestError`, bearer-authed + viewer-proxied like the other `/v1/*` admin
//! surfaces. See `migrations/0019_watchlist.sql`.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::IngestError;
use crate::state::AppState;

/// Default cooldown for a watch alert. A watched subject can be continuously visible, so this is
/// deliberately coarse (30 min) — one notification per re-appearance, not a per-segment stream.
const WATCH_COOLDOWN_SECS: i32 = 1800;

/// Mint the managed alert_rule for a watched subject (scoped to it, feed channel, min_severity 'info'
/// so any sighting qualifies — debounced by the cooldown). Returns the new rule_id. Used on first
/// watch AND to self-heal a watch whose rule was deleted out from under it.
async fn mint_managed_rule(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    subject_type: &str,
    subject_id: Uuid,
    label: &Option<String>,
    enabled: bool,
) -> Result<Uuid, sqlx::Error> {
    let rule_id = Uuid::now_v7();
    let rule_name = format!(
        "Watch: {}",
        label.clone().unwrap_or_else(|| format!("{subject_type} {}", short(subject_id)))
    );
    sqlx::query(
        r#"
        INSERT INTO alert_rules
            (rule_id, name, enabled, event_types, device_ids, subject_type, subject_ids,
             min_severity, days_of_week, tz, cooldown_secs, channels)
        VALUES ($1,$2,$3,'{}','{}',$4,$5,'info','{}','UTC',$6,'[{"type":"feed"}]'::jsonb)
        "#,
    )
    .bind(rule_id)
    .bind(&rule_name)
    .bind(enabled)
    .bind(subject_type)
    .bind(vec![subject_id])
    .bind(WATCH_COOLDOWN_SECS)
    .execute(&mut **tx)
    .await?;
    Ok(rule_id)
}

#[derive(Debug, Serialize)]
pub struct WatchEntry {
    pub watch_id: Uuid,
    pub subject_type: String,
    pub subject_id: Uuid,
    /// Display label at add time (denormalized).
    pub label: Option<String>,
    /// Freshly-resolved current name from the catalog (may differ if renamed since).
    pub current_label: Option<String>,
    pub reason: Option<String>,
    pub rule_id: Option<Uuid>,
    pub enabled: bool,
}

fn entry_from_row(r: &sqlx::postgres::PgRow) -> WatchEntry {
    WatchEntry {
        watch_id: r.get("watch_id"),
        subject_type: r.get("subject_type"),
        subject_id: r.get("subject_id"),
        label: r.get("label"),
        current_label: r.get("current_label"),
        reason: r.get("reason"),
        rule_id: r.get("rule_id"),
        enabled: r.get("enabled"),
    }
}

/// `GET /v1/watchlist` — list entries, newest first, with each subject's CURRENT catalog name and the
/// managed rule's enabled state.
pub async fn list_watchlist(State(st): State<AppState>) -> Result<Json<Vec<WatchEntry>>, IngestError> {
    let rows = sqlx::query(
        r#"
        SELECT w.watch_id, w.subject_type, w.subject_id, w.label, w.reason, w.rule_id,
               COALESCE(r.enabled, false) AS enabled,
               CASE w.subject_type
                   WHEN 'person' THEN (SELECT display_name FROM persons p WHERE p.person_id = w.subject_id)
                   WHEN 'plate'  THEN (SELECT COALESCE(display_name, plate_text) FROM license_plates l WHERE l.plate_id = w.subject_id)
                   ELSE NULL
               END AS current_label
        FROM watchlist w
        LEFT JOIN alert_rules r ON r.rule_id = w.rule_id
        ORDER BY w.created_at DESC
        "#,
    )
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows.iter().map(entry_from_row).collect()))
}

#[derive(Debug, Deserialize)]
pub struct AddWatchReq {
    pub subject_type: String,
    pub subject_id: Uuid,
    pub reason: Option<String>,
}

/// `POST /v1/watchlist` — watch a subject. Idempotent: re-watching returns the existing entry. In one
/// tx, creates the managed alert_rule (scoped to the subject) and the watchlist row linking it.
pub async fn add_watch(
    State(st): State<AppState>,
    Json(req): Json<AddWatchReq>,
) -> Result<Json<WatchEntry>, IngestError> {
    let subject_type = req.subject_type.trim();
    if subject_type != "person" && subject_type != "plate" {
        return Err(IngestError::BadRequest(
            "subject_type must be 'person' or 'plate'".into(),
        ));
    }

    let label = resolve_label(&st.pool, subject_type, req.subject_id).await;

    // Already watched? Idempotent + SELF-HEALING: re-mint a managed rule that was deleted out from
    // under the watch (rule_id NULL), and re-enable a disabled one, so re-clicking Watch always
    // restores a firing watch (instead of returning a silently-dead entry).
    if let Some(existing) = fetch_by_subject(&st.pool, subject_type, req.subject_id).await? {
        let needs_remint = existing.rule_id.is_none();
        let needs_reenable = existing.rule_id.is_some() && !existing.enabled;
        if needs_remint || needs_reenable {
            let mut tx = st.pool.begin().await?;
            if needs_remint {
                let rid = mint_managed_rule(&mut tx, subject_type, req.subject_id, &label, true).await?;
                sqlx::query("UPDATE watchlist SET rule_id = $2, updated_at = now() WHERE watch_id = $1")
                    .bind(existing.watch_id)
                    .bind(rid)
                    .execute(&mut *tx)
                    .await?;
            } else if let Some(rid) = existing.rule_id {
                sqlx::query("UPDATE alert_rules SET enabled = true, updated_at = now() WHERE rule_id = $1")
                    .bind(rid)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            return fetch_by_subject(&st.pool, subject_type, req.subject_id)
                .await?
                .map(Json)
                .ok_or(IngestError::Internal(anyhow::anyhow!("watch vanished after self-heal")));
        }
        return Ok(Json(existing));
    }

    let mut tx = st.pool.begin().await?;
    let rule_id = mint_managed_rule(&mut tx, subject_type, req.subject_id, &label, true).await?;
    let watch_id = Uuid::now_v7();
    let res = sqlx::query(
        "INSERT INTO watchlist (watch_id, subject_type, subject_id, label, reason, rule_id) \
         VALUES ($1,$2,$3,$4,$5,$6)",
    )
    .bind(watch_id)
    .bind(subject_type)
    .bind(req.subject_id)
    .bind(&label)
    .bind(&req.reason)
    .bind(rule_id)
    .execute(&mut *tx)
    .await;
    match res {
        Ok(_) => {
            tx.commit().await?;
        }
        // Lost a race to a concurrent add (UNIQUE(subject_type,subject_id)) — roll back (drops the
        // just-created rule too) and return the winner's entry.
        Err(e) if is_unique_violation(&e) => {
            tx.rollback().await.ok();
            return fetch_by_subject(&st.pool, subject_type, req.subject_id)
                .await?
                .map(Json)
                .ok_or(IngestError::Internal(anyhow::anyhow!(
                    "watchlist conflict but entry not found"
                )));
        }
        Err(e) => return Err(e.into()),
    }

    fetch_by_subject(&st.pool, subject_type, req.subject_id)
        .await?
        .map(Json)
        .ok_or(IngestError::Internal(anyhow::anyhow!(
            "watchlist row vanished after insert"
        )))
}

#[derive(Debug, Deserialize)]
pub struct UpdateWatchReq {
    pub reason: Option<String>,
    pub enabled: Option<bool>,
}

/// `PATCH /v1/watchlist/{id}` — update the note and/or enable/disable (reflected onto the managed rule).
pub async fn update_watch(
    State(st): State<AppState>,
    Path(watch_id): Path<Uuid>,
    Json(req): Json<UpdateWatchReq>,
) -> Result<Json<WatchEntry>, IngestError> {
    let row: Option<(String, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT subject_type, subject_id, rule_id FROM watchlist WHERE watch_id = $1",
    )
    .bind(watch_id)
    .fetch_optional(&st.pool)
    .await?;
    let (subject_type, subject_id, rule_id) = row.ok_or(IngestError::NotFound("watchlist entry"))?;

    let mut tx = st.pool.begin().await?;
    // COALESCE so an omitted field is left unchanged.
    sqlx::query("UPDATE watchlist SET reason = COALESCE($2, reason), updated_at = now() WHERE watch_id = $1")
        .bind(watch_id)
        .bind(&req.reason)
        .execute(&mut *tx)
        .await?;
    if let Some(enabled) = req.enabled {
        match rule_id {
            Some(rid) => {
                sqlx::query("UPDATE alert_rules SET enabled = $2, updated_at = now() WHERE rule_id = $1")
                    .bind(rid)
                    .bind(enabled)
                    .execute(&mut *tx)
                    .await?;
            }
            // Managed rule was deleted out from under this watch — re-mint it so enable/disable is
            // meaningful (not a silent no-op that reports enabled=false forever).
            None => {
                let label = resolve_label(&st.pool, &subject_type, subject_id).await;
                let rid = mint_managed_rule(&mut tx, &subject_type, subject_id, &label, enabled).await?;
                sqlx::query("UPDATE watchlist SET rule_id = $2 WHERE watch_id = $1")
                    .bind(watch_id)
                    .bind(rid)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    tx.commit().await?;

    fetch_by_subject(&st.pool, &subject_type, subject_id)
        .await?
        .map(Json)
        .ok_or(IngestError::NotFound("watchlist entry"))
}

/// Small JSON status body (mirrors events.rs).
pub struct StatusJson(serde_json::Value);
impl axum::response::IntoResponse for StatusJson {
    fn into_response(self) -> axum::response::Response {
        Json(self.0).into_response()
    }
}

/// `DELETE /v1/watchlist/{id}` — unwatch: remove the entry AND its managed rule (one tx).
pub async fn remove_watch(
    State(st): State<AppState>,
    Path(watch_id): Path<Uuid>,
) -> Result<StatusJson, IngestError> {
    let mut tx = st.pool.begin().await?;
    let rule_id: Option<Option<Uuid>> =
        sqlx::query_scalar("DELETE FROM watchlist WHERE watch_id = $1 RETURNING rule_id")
            .bind(watch_id)
            .fetch_optional(&mut *tx)
            .await?;
    let rule_id = rule_id.ok_or(IngestError::NotFound("watchlist entry"))?;
    if let Some(rid) = rule_id {
        sqlx::query("DELETE FROM alert_rules WHERE rule_id = $1")
            .bind(rid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusJson(serde_json::json!({ "unwatched": watch_id })))
}

/// Reconcile the watchlist when a subject is MERGED (`loser` folded into `into`). Called inside the
/// merge tx of `persons::merge_person` / `plates::merge_plate` BEFORE the loser id is deleted —
/// otherwise the watch + its managed rule keep pointing at the dead loser id and silently never fire
/// again (all future sightings carry the survivor's id). If the survivor is already watched, the
/// loser's watch (+ rule) is dropped (the survivor's watch already covers `into`, and the UNIQUE
/// index forbids two rows for the same subject); otherwise both are repointed to the survivor.
pub async fn reconcile_merge(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    subject_type: &str,
    loser: Uuid,
    into: Uuid,
) -> Result<(), sqlx::Error> {
    let loser_watch: Option<(Uuid, Option<Uuid>)> =
        sqlx::query_as("SELECT watch_id, rule_id FROM watchlist WHERE subject_type = $1 AND subject_id = $2")
            .bind(subject_type)
            .bind(loser)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((loser_watch_id, loser_rule_id)) = loser_watch else {
        return Ok(()); // loser wasn't watched — nothing to do
    };
    let survivor_watched: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM watchlist WHERE subject_type = $1 AND subject_id = $2)",
    )
    .bind(subject_type)
    .bind(into)
    .fetch_one(&mut **tx)
    .await?;

    if survivor_watched {
        // Drop the loser's watch + its managed rule; the survivor's watch already covers `into`.
        sqlx::query("DELETE FROM watchlist WHERE watch_id = $1")
            .bind(loser_watch_id)
            .execute(&mut **tx)
            .await?;
        if let Some(rid) = loser_rule_id {
            sqlx::query("DELETE FROM alert_rules WHERE rule_id = $1")
                .bind(rid)
                .execute(&mut **tx)
                .await?;
        }
    } else {
        // Repoint the watch + its managed rule's subject_ids to the survivor.
        sqlx::query("UPDATE watchlist SET subject_id = $2, updated_at = now() WHERE watch_id = $1")
            .bind(loser_watch_id)
            .bind(into)
            .execute(&mut **tx)
            .await?;
        if let Some(rid) = loser_rule_id {
            sqlx::query("UPDATE alert_rules SET subject_ids = $2, updated_at = now() WHERE rule_id = $1")
                .bind(rid)
                .bind(vec![into])
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(())
}

// --- helpers ---------------------------------------------------------------------

async fn fetch_by_subject(
    pool: &PgPool,
    subject_type: &str,
    subject_id: Uuid,
) -> Result<Option<WatchEntry>, IngestError> {
    let row = sqlx::query(
        r#"
        SELECT w.watch_id, w.subject_type, w.subject_id, w.label, w.reason, w.rule_id,
               COALESCE(r.enabled, false) AS enabled,
               CASE w.subject_type
                   WHEN 'person' THEN (SELECT display_name FROM persons p WHERE p.person_id = w.subject_id)
                   WHEN 'plate'  THEN (SELECT COALESCE(display_name, plate_text) FROM license_plates l WHERE l.plate_id = w.subject_id)
                   ELSE NULL
               END AS current_label
        FROM watchlist w
        LEFT JOIN alert_rules r ON r.rule_id = w.rule_id
        WHERE w.subject_type = $1 AND w.subject_id = $2
        "#,
    )
    .bind(subject_type)
    .bind(subject_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(entry_from_row))
}

async fn resolve_label(pool: &PgPool, subject_type: &str, subject_id: Uuid) -> Option<String> {
    let sql: &'static str = match subject_type {
        "person" => "SELECT display_name FROM persons WHERE person_id = $1",
        "plate" => "SELECT COALESCE(display_name, plate_text) FROM license_plates WHERE plate_id = $1",
        _ => return None,
    };
    sqlx::query_scalar::<_, Option<String>>(sql)
        .bind(subject_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
}

fn short(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}
