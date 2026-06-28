//! Global speaker name <-> id resolution for RAG attribution.
//!
//! Identity is cross-device, so resolution is global (no device scope). Runtime sqlx
//! (the RAG crate is all QueryBuilder/runtime queries — no compile-time `.sqlx` cache).
//!
//! Type contract: `speakers.speaker_id` is `uuid`; the denormalized
//! `transcript_sentences.speaker_id` the filter binds against is `text`. So `resolve_name`
//! returns `Vec<Uuid>` (the caller stringifies them for the text[] filter), while
//! `name_map` binds uuid-strings cast to `::uuid[]` against the uuid PK column.

use std::collections::HashMap;

use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Resolve a display name to speaker ids (case-insensitive, hits `speakers_name_idx`).
/// Unknown name -> empty Vec (the caller turns that into `Some(vec![])` => matches
/// nothing). Ambiguous name (multiple speakers share it) -> all matching ids (union).
pub async fn resolve_name(pool: &PgPool, name: &str) -> anyhow::Result<Vec<Uuid>> {
    let rows = sqlx::query("SELECT speaker_id FROM speakers WHERE lower(display_name) = lower($1)")
        .bind(name)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| r.get::<Uuid, _>("speaker_id"))
        .collect())
}

/// Map speaker-id strings to display names for prompt attribution. Batched single query;
/// only named speakers are returned (callers default the rest to "unknown speaker").
/// `ids` are uuid strings; we cast to `::uuid[]` to compare against the uuid PK column.
pub async fn name_map(pool: &PgPool, ids: &[String]) -> anyhow::Result<HashMap<String, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        "SELECT speaker_id, display_name FROM speakers \
         WHERE speaker_id = ANY($1::uuid[]) AND display_name IS NOT NULL",
    )
    .bind(ids.to_vec())
    .fetch_all(pool)
    .await?;

    let mut map = HashMap::with_capacity(rows.len());
    for r in rows {
        let id: Uuid = r.get("speaker_id");
        let name: String = r.get("display_name");
        map.insert(id.to_string(), name);
    }
    Ok(map)
}

/// Label for a passage whose `speaker_id` is NULL (no voiceprint at all). Deliberately
/// NOT person-shaped so the LLM never ranks it as a participant ("who talks the most").
pub const UNATTRIBUTED_AUDIO: &str = "unattributed audio";

/// Distinct, human-readable label for the Nth *unnamed-but-real* speaker. Keeps two
/// different unidentified people apart in a prompt/digest instead of collapsing them.
pub fn unidentified_speaker_label(n: usize) -> String {
    format!("unidentified speaker {n}")
}

/// The display label for one passage's speaker. Three cases:
///   * named speaker (id is in `names`)        -> the human name;
///   * real-but-unnamed speaker (id, no name)  -> `unidentified speaker N` when an
///     `ordinal` is supplied (see [`assign_unnamed_ordinals`]), else a stable
///     `an unidentified voice (#<6 hex>)` so two distinct ids still differ;
///   * NULL speaker_id                          -> [`UNATTRIBUTED_AUDIO`].
///
/// This is the single chokepoint that stops the old behaviour where every unresolved
/// speaker rendered with one identical literal — which made the LLM treat several
/// genuinely different people (and unattributed audio) as one "unidentified" person.
pub fn display_label(
    speaker_id: Option<&str>,
    names: &HashMap<String, String>,
    ordinal: Option<usize>,
) -> String {
    match speaker_id {
        Some(id) => match names.get(id) {
            Some(name) => name.clone(),
            None => match ordinal {
                Some(n) => unidentified_speaker_label(n),
                None => format!("an unidentified voice (#{})", &id[..id.len().min(6)]),
            },
        },
        None => UNATTRIBUTED_AUDIO.to_string(),
    }
}

/// Assign a stable 1-based ordinal to each distinct *unnamed* speaker id, in first-seen
/// order over the batch. Named ids (present in `names`) and NULL ids get no ordinal.
/// Numbering is batch-local — the only requirement is that distinct unnamed voices are
/// distinguishable *within one* answer; it is intentionally not stable across requests
/// (a global scheme would need an extra DB read for no user-visible benefit).
pub fn assign_unnamed_ordinals<'a>(
    speaker_ids: impl IntoIterator<Item = Option<&'a str>>,
    names: &HashMap<String, String>,
) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    let mut next = 1usize;
    for sid in speaker_ids.into_iter().flatten() {
        if !names.contains_key(sid) && !map.contains_key(sid) {
            map.insert(sid.to_string(), next);
            next += 1;
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("named-id".to_string(), "Bob".to_string());
        m
    }

    #[test]
    fn label_named_wins() {
        assert_eq!(display_label(Some("named-id"), &names(), Some(1)), "Bob");
    }

    #[test]
    fn label_unnamed_uses_ordinal() {
        assert_eq!(
            display_label(Some("abcdef0123"), &names(), Some(2)),
            "unidentified speaker 2"
        );
    }

    #[test]
    fn label_unnamed_without_ordinal_is_stable_and_distinct() {
        let a = display_label(Some("aaaaaa1111"), &names(), None);
        let b = display_label(Some("bbbbbb2222"), &names(), None);
        assert_ne!(a, b);
        assert_eq!(a, display_label(Some("aaaaaa1111"), &names(), None)); // stable
        assert!(!a.contains("segment ")); // no machine markers leaked
    }

    #[test]
    fn label_null_is_unattributed() {
        assert_eq!(display_label(None, &names(), Some(1)), UNATTRIBUTED_AUDIO);
    }

    #[test]
    fn ordinals_are_per_distinct_unnamed_in_order() {
        let ids = vec![
            Some("named-id"), // named -> skipped
            Some("first"),    // -> 1
            None,             // NULL  -> skipped
            Some("second"),   // -> 2
            Some("first"),    // repeat -> still 1
        ];
        let ord = assign_unnamed_ordinals(ids, &names());
        assert_eq!(ord.get("first"), Some(&1));
        assert_eq!(ord.get("second"), Some(&2));
        assert_eq!(ord.get("named-id"), None);
        assert_eq!(ord.len(), 2);
    }
}
