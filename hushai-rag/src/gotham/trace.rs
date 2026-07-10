//! Tool-trace accumulation + persistence (Gotham.md §2.4/§2.6). One entry per tool the agent ran
//! this turn — SHAPE, not payloads: the full tool results are never persisted (they are large and
//! re-derivable); `audit_log` carries the args for forensics. The trace lands in
//! `chat_messages.tool_trace` (jsonb, migration 0031) via a targeted UPDATE after the assistant
//! message row is inserted, so `insert_message`'s signature is unchanged.

use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// One tool invocation's record. `args` is the JSON the model emitted (or a compact summary);
/// `outcome` is `ok` | `skipped` | `error`.
#[derive(Debug, Clone, Serialize)]
pub struct ToolTraceEntry {
    pub seq: u32,
    pub tool: String,
    pub args: serde_json::Value,
    pub ok: bool,
    pub elapsed_ms: u64,
    pub result_chars: usize,
    pub outcome: String,
}

/// Per-turn accumulator. `fell_back` marks the whole-turn fallback to the auto pipeline.
#[derive(Debug, Default)]
pub struct TurnTrace {
    pub entries: Vec<ToolTraceEntry>,
    pub fell_back: bool,
}

impl TurnTrace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a completed tool call. `seq` is 1-based in call order.
    pub fn record(
        &mut self,
        tool: &str,
        args: serde_json::Value,
        ok: bool,
        elapsed_ms: u64,
        result_chars: usize,
        outcome: &str,
    ) {
        self.entries.push(ToolTraceEntry {
            seq: self.entries.len() as u32 + 1,
            tool: tool.to_string(),
            args,
            ok,
            elapsed_ms,
            result_chars,
            outcome: outcome.to_string(),
        });
    }

    pub fn tool_calls(&self) -> usize {
        self.entries.len()
    }

    /// The jsonb value to persist. `null` (SQL NULL) when nothing ran and no fallback — keeps
    /// non-agent-shaped turns clean. When the turn fell back, a trailing sentinel entry records it.
    pub fn to_json(&self) -> Option<serde_json::Value> {
        if self.entries.is_empty() && !self.fell_back {
            return None;
        }
        let mut arr: Vec<serde_json::Value> = self
            .entries
            .iter()
            .map(|e| serde_json::to_value(e).unwrap_or(serde_json::Value::Null))
            .collect();
        if self.fell_back {
            arr.push(serde_json::json!({
                "seq": arr.len() + 1,
                "tool": "(runtime)",
                "outcome": "fell_back",
            }));
        }
        Some(serde_json::Value::Array(arr))
    }
}

/// Persist the trace onto an already-inserted assistant message. Best-effort: a failure here never
/// fails the turn (the answer was already streamed + the message row already exists).
pub async fn persist(pool: &PgPool, message_id: Uuid, trace: &TurnTrace) {
    let Some(json) = trace.to_json() else { return };
    if let Err(e) =
        sqlx::query("UPDATE chat_messages SET tool_trace = $1 WHERE message_id = $2")
            .bind(json)
            .bind(message_id)
            .execute(pool)
            .await
    {
        tracing::warn!(error = %e, %message_id, "gotham: persisting tool_trace failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_quiet_turn_persists_null() {
        let t = TurnTrace::new();
        assert!(t.to_json().is_none());
    }

    #[test]
    fn records_in_call_order_with_seq() {
        let mut t = TurnTrace::new();
        t.record("search_transcripts", serde_json::json!({"query": "x"}), true, 12, 340, "ok");
        t.record("people_sightings", serde_json::json!({}), true, 8, 90, "ok");
        let v = t.to_json().unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["seq"], 1);
        assert_eq!(arr[0]["tool"], "search_transcripts");
        assert_eq!(arr[1]["seq"], 2);
        assert_eq!(t.tool_calls(), 2);
    }

    #[test]
    fn fallback_appends_sentinel() {
        let mut t = TurnTrace::new();
        t.fell_back = true;
        let v = t.to_json().unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.last().unwrap()["outcome"], "fell_back");
    }
}
