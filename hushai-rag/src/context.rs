//! The assistant context layer: everything the chat model should know about the world beyond
//! the passages retrieved for one question.
//!
//! Two chokepoints, both bounded and failure-tolerant so a lane going wrong never fails a turn:
//!
//!   1. [`assemble_briefing`] — a compact "Facts (reliable, from the system)" block (today's
//!      date/time, the known-voice and known-person rosters, the cameras, the most recent
//!      recorded activity) prepended to the answer prompt. One small SQL per lane.
//!   2. [`enrich_sources_with_vision`] — annotates retrieved transcript passages with the OTHER
//!      things the same segment saw (who was on camera, what objects, which plates), via one
//!      batched `segment_id = ANY(...)` query per vision lane.
//!
//! ## Extension contract
//! A NEW detection lane plugs in with ZERO changes to `chat.rs`/`routes.rs` dispatch:
//!   * to add a fact to the briefing — write one `async fn <lane>_line(...) -> Result<Option<String>>`
//!     and push it into the provider list in [`assemble_briefing`];
//!   * to annotate passages — add one batched query + one `push_str` into the composer in
//!     [`enrich_sources_with_vision`].
//! Each is independently env-gated (see `RagConfig::context_*`) and never panics the request.

use std::collections::HashMap;

use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::retrieve::Source;
use crate::state::AppState;

/// Assemble the per-turn briefing. Each provider lane is best-effort: a lane that errors is logged
/// and skipped, never failing the turn. The result is one fact per line under a header, hard-capped
/// at `cfg.context_max_chars`. Empty string when nothing is known yet (the caller then prepends
/// nothing, so the prompt is byte-identical to the pre-feature behaviour).
pub async fn assemble_briefing(st: &AppState, tz_offset_secs: i64, now_unix_nanos: i64) -> String {
    let mut lines: Vec<String> = Vec::new();

    // Non-DB: always available.
    lines.push(format!(
        "Today is {}.",
        crate::humanize::absolute_time(now_unix_nanos, tz_offset_secs)
    ));

    // DB lanes — each logged-and-skipped on error.
    for lane in [
        voices_line(&st.pool, st.cfg.context_roster_max).await,
        people_line(&st.pool, st.cfg.context_roster_max).await,
        cameras_line(&st.pool).await,
        last_activity_line(&st.pool, now_unix_nanos, tz_offset_secs).await,
    ] {
        match lane {
            Ok(Some(l)) => lines.push(l),
            Ok(None) => {}
            Err(e) => tracing::warn!(error = format!("{e:#}"), "briefing lane skipped"),
        }
    }

    let mut out = lines.join("\n");
    // Hard char cap (defensive for a small local model); truncate on a char boundary.
    if out.chars().count() > st.cfg.context_max_chars {
        out = out.chars().take(st.cfg.context_max_chars).collect();
    }
    out
}

/// "Known voices in the recordings: Morgan, Sarah (and 3 other unnamed voices)." Named speakers by
/// sample count, plus a count of the still-unnamed. `None` when the catalog is empty.
async fn voices_line(pool: &PgPool, roster_max: i64) -> anyhow::Result<Option<String>> {
    let named: Vec<String> = sqlx::query(
        "SELECT display_name FROM speakers \
         WHERE display_name IS NOT NULL AND archived_at IS NULL \
         ORDER BY n_samples DESC LIMIT $1",
    )
    .bind(roster_max.max(1))
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| r.get::<String, _>("display_name"))
    .collect();
    let unnamed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM speakers WHERE display_name IS NULL AND archived_at IS NULL",
    )
    .fetch_one(pool)
    .await?;
    Ok(roster_sentence("Known voices in the recordings", &named, unnamed, "unnamed voice"))
}

/// "People recognized on camera: Morgan, Bob (and 1 other unrecognized face)."
async fn people_line(pool: &PgPool, roster_max: i64) -> anyhow::Result<Option<String>> {
    let named: Vec<String> = sqlx::query(
        "SELECT display_name FROM persons \
         WHERE display_name IS NOT NULL AND archived_at IS NULL \
         ORDER BY n_samples DESC LIMIT $1",
    )
    .bind(roster_max.max(1))
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| r.get::<String, _>("display_name"))
    .collect();
    let unnamed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM persons WHERE display_name IS NULL AND archived_at IS NULL",
    )
    .fetch_one(pool)
    .await?;
    Ok(roster_sentence(
        "People recognized on camera",
        &named,
        unnamed,
        "unrecognized face",
    ))
}

/// Compose a roster sentence from named entries + an unnamed count, or `None` when there's nothing
/// at all. E.g. `("Known voices", ["A","B"], 3, "unnamed voice")` → "Known voices: A, B (and 3
/// other unnamed voices)."
fn roster_sentence(label: &str, named: &[String], unnamed: i64, unnamed_noun: &str) -> Option<String> {
    if named.is_empty() && unnamed == 0 {
        return None;
    }
    let list = if named.is_empty() {
        format!(
            "{unnamed} {unnamed_noun}{}",
            if unnamed == 1 { "" } else { "s" }
        )
    } else {
        let mut s = named.join(", ");
        if unnamed > 0 {
            s.push_str(&format!(
                " (and {unnamed} other {unnamed_noun}{})",
                if unnamed == 1 { "" } else { "s" }
            ));
        }
        s
    };
    Some(format!("{label}: {list}."))
}

/// "Recording devices: Kitchen camera, a phone, a web browser." Prefers each device's display_name;
/// falls back to a natural phrase from `source_kind` so a raw `web-<uuid>` id is NEVER printed
/// (the no-identifiers prompt contract). `None` when no devices are registered.
async fn cameras_line(pool: &PgPool) -> anyhow::Result<Option<String>> {
    let rows = sqlx::query("SELECT device_id, display_name, source_kind FROM devices")
        .fetch_all(pool)
        .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut names: Vec<String> = Vec::with_capacity(rows.len());
    for r in &rows {
        let display: Option<String> = r.try_get("display_name").unwrap_or(None);
        let kind: String = r.try_get::<Option<String>, _>("source_kind").unwrap_or(None).unwrap_or_default();
        names.push(match display {
            Some(d) if !d.trim().is_empty() => d.trim().to_string(),
            _ => source_kind_phrase(&kind),
        });
    }
    let list = match names.len() {
        1 => names[0].clone(),
        2 => format!("{} and {}", names[0], names[1]),
        n => format!("{}, and {}", names[..n - 1].join(", "), names[n - 1]),
    };
    Ok(Some(format!("Recording devices: {list}.")))
}

/// A human phrase for a device with no display name, from its `source_kind` (never its id).
fn source_kind_phrase(kind: &str) -> String {
    match kind.to_lowercase().as_str() {
        k if k.contains("web") || k.contains("browser") => "a web browser".to_string(),
        k if k.contains("phone") || k.contains("android") || k.contains("mobile") => {
            "a phone".to_string()
        }
        _ => "a camera".to_string(),
    }
}

/// "The most recent recorded conversation was yesterday at 5:14 PM." Anchors recency questions even
/// when the recency detector doesn't fire. `None` when nothing is transcribed yet.
async fn last_activity_line(
    pool: &PgPool,
    now_unix_nanos: i64,
    tz_offset_secs: i64,
) -> anyhow::Result<Option<String>> {
    let latest: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(start_unix_nanos) FROM transcript_sentences WHERE text IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    Ok(latest.map(|t| {
        format!(
            "The most recent recorded conversation was {}.",
            crate::humanize::humanize_time(t, now_unix_nanos, tz_offset_secs)
        )
    }))
}

/// Annotate transcript `sources` with same-segment VISION context: who was on camera, what objects
/// were in view, which plates were seen. One batched query per lane over the distinct segment ids;
/// composed into `Source.visual_context` capped at 3 persons / 3 objects / 2 plates per segment.
/// Best-effort: a lane that errors is logged and skipped. Sources with no vision hits are untouched.
pub async fn enrich_sources_with_vision(
    pool: &PgPool,
    sources: &mut [Source],
    _max_per_lane: usize,
) -> anyhow::Result<()> {
    // Distinct, non-nil segment ids to probe.
    let seg_ids: Vec<Uuid> = {
        let mut v: Vec<Uuid> = sources
            .iter()
            .map(|s| s.segment_id)
            .filter(|id| !id.is_nil())
            .collect();
        v.sort();
        v.dedup();
        v
    };
    if seg_ids.is_empty() {
        return Ok(());
    }

    // Per-segment accumulators for each lane.
    let persons = persons_by_segment(pool, &seg_ids).await.unwrap_or_else(|e| {
        tracing::warn!(error = format!("{e:#}"), "vision enrich: persons lane skipped");
        HashMap::new()
    });
    let objects = objects_by_segment(pool, &seg_ids).await.unwrap_or_else(|e| {
        tracing::warn!(error = format!("{e:#}"), "vision enrich: objects lane skipped");
        HashMap::new()
    });
    let plates = plates_by_segment(pool, &seg_ids).await.unwrap_or_else(|e| {
        tracing::warn!(error = format!("{e:#}"), "vision enrich: plates lane skipped");
        HashMap::new()
    });

    for s in sources.iter_mut() {
        if s.segment_id.is_nil() {
            continue;
        }
        let mut parts: Vec<String> = Vec::new();
        if let Some(who) = persons.get(&s.segment_id) {
            if !who.is_empty() {
                parts.push(format!("on camera: {}", who.join(", ")));
            }
        }
        if let Some(objs) = objects.get(&s.segment_id) {
            if !objs.is_empty() {
                parts.push(format!("in view: {}", objs.join(", ")));
            }
        }
        if let Some(pl) = plates.get(&s.segment_id) {
            if !pl.is_empty() {
                parts.push(format!("plates: {}", pl.join(", ")));
            }
        }
        if !parts.is_empty() {
            s.visual_context = Some(parts.join("; "));
        }
    }
    Ok(())
}

/// Persons on camera per segment (up to 3), by name; unnamed faces become a count
/// ("2 unrecognized people") so no invented names leak.
async fn persons_by_segment(
    pool: &PgPool,
    seg_ids: &[Uuid],
) -> anyhow::Result<HashMap<Uuid, Vec<String>>> {
    let rows = sqlx::query(
        "SELECT ps.segment_id, ps.person_id, p.display_name \
         FROM person_segments ps \
         LEFT JOIN persons p ON p.person_id = ps.person_id \
         WHERE ps.segment_id = ANY($1::uuid[]) AND ps.person_id IS NOT NULL \
         GROUP BY ps.segment_id, ps.person_id, p.display_name",
    )
    .bind(seg_ids.to_vec())
    .fetch_all(pool)
    .await?;
    let mut named: HashMap<Uuid, Vec<String>> = HashMap::new();
    let mut unnamed: HashMap<Uuid, i64> = HashMap::new();
    for r in &rows {
        let seg: Uuid = r.get("segment_id");
        let name: Option<String> = r.try_get("display_name").unwrap_or(None);
        match name {
            Some(n) if !n.trim().is_empty() => named.entry(seg).or_default().push(n),
            _ => *unnamed.entry(seg).or_default() += 1,
        }
    }
    let mut out: HashMap<Uuid, Vec<String>> = HashMap::new();
    let mut keys: Vec<Uuid> = named.keys().chain(unnamed.keys()).copied().collect();
    keys.sort();
    keys.dedup();
    for seg in keys {
        let mut list: Vec<String> = named.get(&seg).cloned().unwrap_or_default();
        list.truncate(3);
        if let Some(&n) = unnamed.get(&seg) {
            if n > 0 {
                list.push(format!(
                    "{n} unrecognized {}",
                    if n == 1 { "person" } else { "people" }
                ));
            }
        }
        out.insert(seg, list);
    }
    Ok(out)
}

/// Object labels per segment (top 3 by count), excluding the whole-frame `__frame__` marker.
async fn objects_by_segment(
    pool: &PgPool,
    seg_ids: &[Uuid],
) -> anyhow::Result<HashMap<Uuid, Vec<String>>> {
    let rows = sqlx::query(
        "SELECT segment_id, object_label, count(*) AS n \
         FROM scene_objects \
         WHERE segment_id = ANY($1::uuid[]) AND object_label <> '__frame__' \
         GROUP BY segment_id, object_label ORDER BY segment_id, n DESC",
    )
    .bind(seg_ids.to_vec())
    .fetch_all(pool)
    .await?;
    let mut out: HashMap<Uuid, Vec<String>> = HashMap::new();
    for r in &rows {
        let seg: Uuid = r.get("segment_id");
        let label: String = r.try_get::<Option<String>, _>("object_label").unwrap_or(None).unwrap_or_default();
        if label.is_empty() {
            continue;
        }
        let e = out.entry(seg).or_default();
        if e.len() < 3 && !e.contains(&label) {
            e.push(label);
        }
    }
    Ok(out)
}

/// Plate labels per segment (up to 2), via the plate catalog's display label.
async fn plates_by_segment(
    pool: &PgPool,
    seg_ids: &[Uuid],
) -> anyhow::Result<HashMap<Uuid, Vec<String>>> {
    let rows = sqlx::query(
        "SELECT DISTINCT segment_id, plate_id FROM plate_detections \
         WHERE segment_id = ANY($1::uuid[]) AND plate_id IS NOT NULL",
    )
    .bind(seg_ids.to_vec())
    .fetch_all(pool)
    .await?;
    // Resolve labels in one batch.
    let plate_ids: Vec<String> = {
        let mut v: Vec<String> = rows
            .iter()
            .map(|r| r.get::<Uuid, _>("plate_id").to_string())
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let labels = crate::plates::label_map(pool, &plate_ids).await?;
    let mut out: HashMap<Uuid, Vec<String>> = HashMap::new();
    for r in &rows {
        let seg: Uuid = r.get("segment_id");
        let pid: Uuid = r.get("plate_id");
        if let Some(label) = labels.get(&pid.to_string()) {
            let e = out.entry(seg).or_default();
            if e.len() < 2 && !e.contains(label) {
                e.push(label.clone());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_sentence_named_only() {
        let s = roster_sentence("Known voices", &["Morgan".into(), "Sarah".into()], 0, "unnamed voice");
        assert_eq!(s.as_deref(), Some("Known voices: Morgan, Sarah."));
    }

    #[test]
    fn roster_sentence_named_plus_unnamed() {
        let s = roster_sentence("Known voices", &["Morgan".into()], 3, "unnamed voice");
        assert_eq!(
            s.as_deref(),
            Some("Known voices: Morgan (and 3 other unnamed voices).")
        );
    }

    #[test]
    fn roster_sentence_unnamed_only_and_singular() {
        assert_eq!(
            roster_sentence("People recognized on camera", &[], 1, "unrecognized face").as_deref(),
            Some("People recognized on camera: 1 unrecognized face.")
        );
        assert_eq!(
            roster_sentence("People recognized on camera", &[], 2, "unrecognized face").as_deref(),
            Some("People recognized on camera: 2 unrecognized faces.")
        );
    }

    #[test]
    fn roster_sentence_empty_is_none() {
        assert!(roster_sentence("Known voices", &[], 0, "unnamed voice").is_none());
    }

    #[test]
    fn source_kind_phrase_maps_kinds() {
        assert_eq!(source_kind_phrase("web"), "a web browser");
        assert_eq!(source_kind_phrase("android-phone"), "a phone");
        assert_eq!(source_kind_phrase("ip-camera"), "a camera");
        assert_eq!(source_kind_phrase(""), "a camera");
    }
}
