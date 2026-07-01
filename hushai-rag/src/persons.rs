//! Global person (face) name <-> id resolution for RAG attribution — the visual sibling of
//! `speakers.rs` ("when did I see Bob" / "who was I with").
//!
//! Identity is cross-device, so resolution is global. Runtime sqlx (no `.sqlx` cache).
//!
//! Type contract (DIFFERS from speakers): `persons.person_id` AND `person_segments.person_id` are
//! both native `uuid` (the 0009 contract) — NOT a denormalized text column. So `resolve_name`
//! returns `Vec<Uuid>` and `name_map`/the retrieval filters bind uuid-strings cast to `::uuid[]`.

use std::collections::HashMap;

use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Resolve a display name to person ids (case-insensitive, hits `persons_name_idx`).
/// Unknown name -> empty Vec (the caller turns that into "matches nothing"). Ambiguous name
/// (multiple persons share it) -> all matching ids (union).
pub async fn resolve_name(pool: &PgPool, name: &str) -> anyhow::Result<Vec<Uuid>> {
    let rows = sqlx::query("SELECT person_id FROM persons WHERE lower(display_name) = lower($1)")
        .bind(name)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| r.get::<Uuid, _>("person_id"))
        .collect())
}

/// Resolve named persons whose display name appears as a substring of free-text `query`
/// ("when did I see Bob" with no explicit filter). Case-insensitive; only names of length >= 2 so a
/// 1-char name can't match everything. Returns the union of matching ids (empty if none mentioned).
pub async fn resolve_names_in_text(pool: &PgPool, query: &str) -> anyhow::Result<Vec<Uuid>> {
    // Word-boundary match (not substring): a name matches only when ALL of its words appear as whole
    // words in the query. A raw `position(name in query)` substring match wrongly attributed e.g.
    // "Cal" to "calendar" or "Ed" to "edited", routing the answer to that person's full sightings.
    let rows = sqlx::query(
        "SELECT person_id, display_name FROM persons \
         WHERE display_name IS NOT NULL AND char_length(display_name) >= 2",
    )
    .fetch_all(pool)
    .await?;
    let q = query.to_lowercase();
    let q_words: std::collections::HashSet<&str> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    let mut ids = Vec::new();
    for r in rows {
        let name: String = r.get("display_name");
        let name_l = name.to_lowercase();
        let name_words: Vec<&str> = name_l
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect();
        if !name_words.is_empty() && name_words.iter().all(|w| q_words.contains(w)) {
            ids.push(r.get::<Uuid, _>("person_id"));
        }
    }
    Ok(ids)
}

/// Map person-id strings to display names for prompt attribution. Batched single query; only named
/// persons are returned. `ids` are uuid strings; cast to `::uuid[]` to compare against the uuid PK.
pub async fn name_map(pool: &PgPool, ids: &[String]) -> anyhow::Result<HashMap<String, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        "SELECT person_id, display_name FROM persons \
         WHERE person_id = ANY($1::uuid[]) AND display_name IS NOT NULL",
    )
    .bind(ids.to_vec())
    .fetch_all(pool)
    .await?;

    let mut map = HashMap::with_capacity(rows.len());
    for r in rows {
        let id: Uuid = r.get("person_id");
        let name: String = r.get("display_name");
        map.insert(id.to_string(), name);
    }
    Ok(map)
}

/// Label for a sighting whose `person_id` is NULL (a detected-but-unassigned face). Deliberately
/// NOT a person name so the LLM never treats it as a known participant.
pub const UNATTRIBUTED_FACE: &str = "an unrecognized face";

/// Distinct, human-readable label for the Nth *unnamed-but-real* person — keeps two different
/// unidentified faces apart in a prompt instead of collapsing them.
pub fn unidentified_person_label(n: usize) -> String {
    format!("unidentified person {n}")
}

/// The display label for one sighting's person. Mirrors `speakers::display_label`:
///   * named person (id in `names`)            -> the human name;
///   * real-but-unnamed person (id, no name)   -> `unidentified person N` (ordinal) else a stable
///     `an unrecognized face (#<6 hex>)`;
///   * NULL person_id                          -> [`UNATTRIBUTED_FACE`].
pub fn display_label(
    person_id: Option<&str>,
    names: &HashMap<String, String>,
    ordinal: Option<usize>,
) -> String {
    match person_id {
        Some(id) => match names.get(id) {
            Some(name) => name.clone(),
            None => match ordinal {
                Some(n) => unidentified_person_label(n),
                None => format!("an unrecognized face (#{})", &id[..id.len().min(6)]),
            },
        },
        None => UNATTRIBUTED_FACE.to_string(),
    }
}

/// Assign a stable 1-based ordinal to each distinct *unnamed* person id, first-seen order over the
/// batch. Named ids and NULL ids get no ordinal. Batch-local (clone of `speakers`').
pub fn assign_unnamed_ordinals<'a>(
    person_ids: impl IntoIterator<Item = Option<&'a str>>,
    names: &HashMap<String, String>,
) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    let mut next = 1usize;
    for pid in person_ids.into_iter().flatten() {
        if !names.contains_key(pid) && !map.contains_key(pid) {
            map.insert(pid.to_string(), next);
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
            "unidentified person 2"
        );
    }

    #[test]
    fn label_unnamed_without_ordinal_is_stable_and_distinct() {
        let a = display_label(Some("aaaaaa1111"), &names(), None);
        let b = display_label(Some("bbbbbb2222"), &names(), None);
        assert_ne!(a, b);
        assert_eq!(a, display_label(Some("aaaaaa1111"), &names(), None)); // stable
    }

    #[test]
    fn label_null_is_unattributed_face() {
        assert_eq!(display_label(None, &names(), Some(1)), UNATTRIBUTED_FACE);
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
