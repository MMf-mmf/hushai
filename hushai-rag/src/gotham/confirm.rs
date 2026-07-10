//! Two-phase confirmation for mutating tools (Gotham.md §2.5). A mutate tool call does NOT execute
//! on first sight: the runtime persists a `gotham_pending_actions` row + speaks a DETERMINISTIC
//! summary (composed by code from parsed args, never by the model) and ends the turn. The NEXT user
//! turn is intercepted BEFORE condensation/routing by [`is_affirmative`]: affirmative → execute
//! directly (no LLM); anything else → cancel + process normally.
//!
//! Read-only Phase 1 (`GOTHAM_MUTATIONS_ENABLED=false`) never creates a pending action, so the
//! intercept is a cheap no-op (the fetch returns `None`). The DB helpers + detector ship now so the
//! Wave-3 mutate path is a small addition, and the detector is unit-tested here.

use sqlx::{PgPool, Row};
use uuid::Uuid;

/// A pending mutating action awaiting the user's yes/no.
#[derive(Debug, Clone)]
pub struct PendingAction {
    pub action_id: Uuid,
    pub tool: String,
    pub args: serde_json::Value,
    pub summary: String,
}

/// Deterministic affirmative detector for the confirm intercept. Intentionally STRICT: only a clear
/// yes confirms a (potentially irreversible) mutation; everything else cancels and is processed as a
/// fresh message. Mirrors the `is_*_query` idiom used elsewhere in the crate.
pub fn is_affirmative(message: &str) -> bool {
    let m = message
        .trim()
        .trim_end_matches(['.', '!'])
        .to_lowercase();
    matches!(
        m.as_str(),
        "yes" | "y"
            | "yeah"
            | "yep"
            | "yup"
            | "sure"
            | "ok"
            | "okay"
            | "confirm"
            | "confirmed"
            | "do it"
            | "go ahead"
            | "yes please"
            | "please do"
            | "affirmative"
            | "correct"
            | "that's right"
    )
}

/// Fetch the session's outstanding pending action, if any (the partial-unique index guarantees ≤ 1).
pub async fn fetch_pending(
    pool: &PgPool,
    session_id: Uuid,
) -> anyhow::Result<Option<PendingAction>> {
    let row = sqlx::query(
        "SELECT action_id, tool, args, summary FROM gotham_pending_actions \
         WHERE session_id = $1 AND status = 'pending' AND expires_at > now() \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| PendingAction {
        action_id: r.get("action_id"),
        tool: r.get("tool"),
        args: r.get("args"),
        summary: r.get("summary"),
    }))
}

/// Persist a new pending action (supersede any stale pending row for the session first, so the
/// partial-unique index never rejects the insert).
pub async fn insert_pending(
    pool: &PgPool,
    session_id: Uuid,
    tool: &str,
    args: &serde_json::Value,
    summary: &str,
    ttl_secs: i64,
) -> anyhow::Result<Uuid> {
    sqlx::query(
        "UPDATE gotham_pending_actions SET status = 'cancelled' \
         WHERE session_id = $1 AND status = 'pending'",
    )
    .bind(session_id)
    .execute(pool)
    .await?;
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO gotham_pending_actions \
           (action_id, session_id, tool, args, summary, status, expires_at) \
         VALUES ($1, $2, $3, $4, $5, 'pending', now() + make_interval(secs => $6))",
    )
    .bind(id)
    .bind(session_id)
    .bind(tool)
    .bind(args)
    .bind(summary)
    .bind(ttl_secs.max(1) as f64)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Mark a pending action terminal (`confirmed` | `cancelled` | `expired`).
pub async fn mark(pool: &PgPool, action_id: Uuid, status: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE gotham_pending_actions SET status = $2 WHERE action_id = $1")
        .bind(action_id)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affirmatives_confirm() {
        for s in ["yes", "Yes.", "yeah", "sure", "confirm", "do it", "Go ahead!", "OK", "correct"] {
            assert!(is_affirmative(s), "{s:?} should confirm");
        }
    }

    #[test]
    fn everything_else_cancels() {
        for s in [
            "no",
            "cancel",
            "not now",
            "actually who was here yesterday",
            "yes but only for weekdays",
            "",
            "maybe",
        ] {
            assert!(!is_affirmative(s), "{s:?} must NOT auto-confirm a mutation");
        }
    }
}
