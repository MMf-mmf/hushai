//! Global license-plate text <-> id resolution for RAG attribution — the VEHICLE sibling of
//! `persons.rs` ("when did I see a car with plate ABC123").
//!
//! KEY DIVERGENCE from `persons`/`speakers` (the 0013 contract): a plate's identity IS its text,
//! so resolution matches a NORMALIZED STRING (exact `plate_text_norm`, then a pg_trgm fuzzy fallback
//! for OCR noise), NOT a k-NN over an embedding. `license_plates.plate_id` is native `uuid`, so
//! resolution returns `Vec<Uuid>` and the retrieval filter binds uuid-strings cast to `::uuid[]`
//! (same as `persons`). Runtime sqlx (no `.sqlx` cache).

use std::collections::HashMap;

use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Fold a raw plate string to its matching key: uppercase, strip every non-alphanumeric character.
/// Mirrors the worker's confusable-folded `plate_text_norm` enough for the exact+trigram match here
/// (the DB column carries the same casing/stripping; trigram absorbs the residual OCR confusables).
pub fn normalize_plate(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_uppercase())
        .collect()
}

/// Resolve a raw plate string to plate ids: exact `plate_text_norm` match first; if none, a pg_trgm
/// similarity match (`plate_text_norm % $1`, ordered by similarity) absorbs OCR/typo noise. Unknown
/// plate -> empty Vec (the caller turns that into "matches nothing"). A normalized key shorter than 2
/// chars resolves to empty so a stray fragment can't fuzzy-match the whole catalog.
pub async fn resolve_plate_text(pool: &PgPool, raw: &str) -> anyhow::Result<Vec<Uuid>> {
    let norm = normalize_plate(raw);
    if norm.len() < 2 {
        return Ok(Vec::new());
    }

    // Exact match on the unique norm key (hits `license_plates_norm_idx`).
    let exact = sqlx::query("SELECT plate_id FROM license_plates WHERE plate_text_norm = $1")
        .bind(&norm)
        .fetch_all(pool)
        .await?;
    if !exact.is_empty() {
        return Ok(exact
            .into_iter()
            .map(|r| r.get::<Uuid, _>("plate_id"))
            .collect());
    }

    // Fuzzy fallback: trigram similarity over `license_plates_trgm_idx`, closest first.
    let fuzzy = sqlx::query(
        "SELECT plate_id FROM license_plates \
         WHERE plate_text_norm % $1 \
         ORDER BY similarity(plate_text_norm, $1) DESC \
         LIMIT 5",
    )
    .bind(&norm)
    .fetch_all(pool)
    .await?;
    Ok(fuzzy
        .into_iter()
        .map(|r| r.get::<Uuid, _>("plate_id"))
        .collect())
}

/// Resolve plate-shaped tokens mentioned in a free-text query ("when did I see a car with plate
/// ABC123" with no explicit filter). A token is a maximal alphanumeric run of length >= 4 that
/// contains at least one digit (plates aren't pure words) — this avoids matching ordinary words.
/// Each candidate is resolved via [`resolve_plate_text`]; returns the de-duplicated union of ids
/// (empty if no plate is mentioned or none match). (No `regex` dep in this crate — split by hand.)
pub async fn resolve_plates_in_text(pool: &PgPool, query: &str) -> anyhow::Result<Vec<Uuid>> {
    let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    let mut ids = Vec::new();
    for token in plate_tokens(query) {
        for id in resolve_plate_text(pool, &token).await? {
            if seen.insert(id) {
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

/// Extract plate-shaped candidate tokens from free text: maximal alphanumeric runs (split on every
/// non-alphanumeric char) of length >= 4 that contain at least one digit. Pure-letter words and
/// short fragments are dropped so only plate-like strings reach the resolver.
fn plate_tokens(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 4 && t.chars().any(|c| c.is_ascii_digit()))
        .map(|t| t.to_string())
        .collect()
}

/// Label for a detection whose `plate_id` is NULL (a read below the catalog gate / unreadable).
/// Deliberately NOT a plate string so the LLM never treats it as an identified vehicle.
pub const UNREADABLE_PLATE: &str = "an unreadable plate";

/// Map plate-id strings to their display label for prompt attribution. Batched single query; every
/// catalog row is returned (named or not). `ids` are uuid strings; cast to `::uuid[]` to compare
/// against the uuid PK. The value is the [`display_label`] result: a `display_name` ("Mom's car")
/// when set, else the plate string ("plate ABC123").
pub async fn label_map(pool: &PgPool, ids: &[String]) -> anyhow::Result<HashMap<String, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        "SELECT plate_id, display_name, plate_text FROM license_plates \
         WHERE plate_id = ANY($1::uuid[])",
    )
    .bind(ids.to_vec())
    .fetch_all(pool)
    .await?;

    let mut map = HashMap::with_capacity(rows.len());
    for r in rows {
        let id: Uuid = r.get("plate_id");
        let display_name: Option<String> = r.try_get("display_name")?;
        let plate_text: Option<String> = r.try_get("plate_text")?;
        map.insert(
            id.to_string(),
            display_label(display_name.as_deref(), plate_text.as_deref()),
        );
    }
    Ok(map)
}

/// The display label for one plate (the plate analogue of `persons::display_label`, but keyed on the
/// plate's own text rather than a face ordinal — a plate's identity IS its string):
///   * named plate (`display_name` set)   -> the human label, e.g. "Mom's car";
///   * unnamed but readable (`plate_text`) -> `plate ABC123`;
///   * NULL text                           -> [`UNREADABLE_PLATE`].
pub fn display_label(display_name: Option<&str>, plate_text: Option<&str>) -> String {
    match display_name {
        Some(name) if !name.trim().is_empty() => name.to_string(),
        _ => match plate_text {
            Some(text) if !text.trim().is_empty() => format!("plate {}", text.trim()),
            _ => UNREADABLE_PLATE.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_uppercases_and_strips() {
        assert_eq!(normalize_plate("abc-123"), "ABC123");
        assert_eq!(normalize_plate(" a b c 1 2 3 "), "ABC123");
        assert_eq!(normalize_plate("ABC123"), "ABC123");
    }

    #[test]
    fn label_named_wins() {
        assert_eq!(display_label(Some("Mom's car"), Some("ABC123")), "Mom's car");
    }

    #[test]
    fn label_unnamed_uses_plate_string() {
        assert_eq!(display_label(None, Some("ABC123")), "plate ABC123");
        // Blank name falls through to the plate string.
        assert_eq!(display_label(Some("  "), Some("ABC123")), "plate ABC123");
    }

    #[test]
    fn label_null_text_is_unreadable() {
        assert_eq!(display_label(None, None), UNREADABLE_PLATE);
        assert_eq!(display_label(None, Some("  ")), UNREADABLE_PLATE);
    }

    #[test]
    fn tokens_keep_only_plate_shaped_runs() {
        let toks = plate_tokens("when did I see a car with plate ABC123 yesterday?");
        assert_eq!(toks, vec!["ABC123".to_string()]);
    }

    #[test]
    fn tokens_drop_pure_words_and_short_runs() {
        // No digit -> dropped; len < 4 -> dropped.
        assert!(plate_tokens("the quick brown car").is_empty());
        assert!(plate_tokens("a1 b2 c3").is_empty());
    }

    #[test]
    fn tokens_handle_multiple_runs() {
        // Each candidate is a maximal alphanumeric RUN: a hyphen inside a plate splits it (so
        // "XYZ-789" -> "XYZ" [no digit, dropped] + "789" [len 3, dropped]). Two whole runs survive.
        let toks = plate_tokens("ABC123 or maybe XYZ789?");
        assert_eq!(toks, vec!["ABC123".to_string(), "XYZ789".to_string()]);
    }
}
