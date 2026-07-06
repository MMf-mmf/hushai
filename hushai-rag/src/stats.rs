//! Deterministic footage statistics — "how many minutes of video do we have today?".
//!
//! Pure segment arithmetic over the `segments` table (`capture_start_unix_nanos` +
//! `duration_nanos`), answered as a `precomputed_answer` with NO LLM narration (same
//! "the model never does arithmetic" principle as `presence.rs`). Before this existed,
//! footage-total questions fell to semantic retrieval over transcripts, matched nothing,
//! and declined with "I don't have information about that in the recordings".
//!
//! Windowing uses the same overlap idiom as `retrieve::window_has_footage`, with the
//! summed duration CLAMPED to the window (`LEAST`/`GREATEST`) so a segment straddling a
//! boundary contributes only its in-window portion.

use sqlx::{PgPool, Postgres, QueryBuilder, Row};

/// Per-device footage totals for one window. `media_type`: AUDIO=1, VIDEO=2, MUXED=3
/// (contracts/cameraToBackendContract.md) — MUXED counts toward both lanes.
#[derive(Debug, Clone, Default)]
pub struct DeviceFootage {
    pub device_id: String,
    pub display_name: Option<String>,
    pub video_nanos: i64,
    pub audio_nanos: i64,
    pub segment_count: i64,
    pub first_ns: Option<i64>,
    pub last_ns: Option<i64>,
}

impl DeviceFootage {
    fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.device_id)
    }
}

/// Sum recorded footage per device over `[after, before)` (either bound optional).
pub async fn footage_stats(
    pool: &PgPool,
    device_id: Option<&str>,
    after: Option<i64>,
    before: Option<i64>,
) -> anyhow::Result<Vec<DeviceFootage>> {
    let after = after.unwrap_or(i64::MIN / 4);
    let before = before.unwrap_or(i64::MAX / 4);
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT s.device_id, d.display_name, \
           COALESCE(SUM(LEAST(s.capture_start_unix_nanos + s.duration_nanos, ",
    );
    qb.push_bind(before)
        .push(") - GREATEST(s.capture_start_unix_nanos, ")
        .push_bind(after)
        .push(
            ")) FILTER (WHERE s.media_type IN (2, 3)), 0)::bigint AS video_nanos, \
             COALESCE(SUM(LEAST(s.capture_start_unix_nanos + s.duration_nanos, ",
        )
        .push_bind(before)
        .push(") - GREATEST(s.capture_start_unix_nanos, ")
        .push_bind(after)
        .push(
            ")) FILTER (WHERE s.media_type IN (1, 3)), 0)::bigint AS audio_nanos, \
             COUNT(*)::bigint AS segment_count, \
             MIN(s.capture_start_unix_nanos) AS first_ns, \
             MAX(s.capture_start_unix_nanos + s.duration_nanos) AS last_ns \
             FROM segments s LEFT JOIN devices d ON d.device_id = s.device_id \
             WHERE s.capture_start_unix_nanos < ",
        )
        .push_bind(before)
        .push(" AND s.capture_start_unix_nanos + s.duration_nanos > ")
        .push_bind(after);
    if let Some(dev) = device_id {
        qb.push(" AND s.device_id = ").push_bind(dev.to_string());
    }
    qb.push(" GROUP BY s.device_id, d.display_name ORDER BY video_nanos DESC, audio_nanos DESC");
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| DeviceFootage {
            device_id: r.get("device_id"),
            display_name: r.get("display_name"),
            video_nanos: r.get("video_nanos"),
            audio_nanos: r.get("audio_nanos"),
            segment_count: r.get("segment_count"),
            first_ns: r.get("first_ns"),
            last_ns: r.get("last_ns"),
        })
        .collect())
}

/// Does the question ask about the AUDIO lane specifically? ("how much audio…") — otherwise
/// the video lane is reported (with an audio-only fallback when there's no video at all).
pub fn wants_audio_lane(question: &str) -> bool {
    let q = question.to_lowercase();
    q.contains("audio") && !q.contains("video") && !q.contains("footage")
}

/// Render the footage totals into the final answer verbatim — the LLM never sees or narrates
/// these numbers. `now`/`tz` anchor the relative-time phrasing of the recording span.
pub fn render_footage_stats(
    rows: &[DeviceFootage],
    wants_audio: bool,
    now_unix_nanos: i64,
    tz_offset_secs: i64,
) -> String {
    if rows.is_empty() {
        return "No footage was recorded for that period.".to_string();
    }
    let lane = |r: &DeviceFootage| if wants_audio { r.audio_nanos } else { r.video_nanos };
    let mut total: i64 = rows.iter().map(lane).sum();
    let mut lane_word = if wants_audio { "audio" } else { "video" };
    let mut fallback_note = String::new();
    if total == 0 && !wants_audio {
        // Audio-only capture: asked about video, but only audio exists — say so honestly
        // instead of "no footage".
        let audio_total: i64 = rows.iter().map(|r| r.audio_nanos).sum();
        if audio_total == 0 {
            return "No footage was recorded for that period.".to_string();
        }
        total = audio_total;
        lane_word = "audio";
        fallback_note = " (audio only — no video was captured)".to_string();
    }

    let mut out = format!(
        "You have about {} of {lane_word} from that period{fallback_note}",
        crate::humanize::humanize_duration(total)
    );
    let active: Vec<&DeviceFootage> = rows
        .iter()
        .filter(|r| (if lane_word == "audio" { r.audio_nanos } else { r.video_nanos }) > 0)
        .collect();
    if active.len() > 1 {
        let parts: Vec<String> = active
            .iter()
            .map(|r| {
                let n = if lane_word == "audio" { r.audio_nanos } else { r.video_nanos };
                format!("{}: {}", r.label(), crate::humanize::humanize_duration(n))
            })
            .collect();
        out.push_str(&format!(", across {} cameras — {}", active.len(), parts.join("; ")));
    } else if let Some(only) = active.first() {
        out.push_str(&format!(" ({})", only.label()));
    }
    out.push('.');
    let first = rows.iter().filter_map(|r| r.first_ns).min();
    let last = rows.iter().filter_map(|r| r.last_ns).max();
    if let (Some(f), Some(l)) = (first, last) {
        out.push_str(&format!(
            " Recording ran from {} to {}.",
            crate::humanize::humanize_time(f, now_unix_nanos, tz_offset_secs),
            crate::humanize::humanize_time(l, now_unix_nanos, tz_offset_secs)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000_000_000;

    fn dev(id: &str, name: Option<&str>, video_mins: i64, audio_mins: i64) -> DeviceFootage {
        DeviceFootage {
            device_id: id.to_string(),
            display_name: name.map(str::to_string),
            video_nanos: video_mins * MIN,
            audio_nanos: audio_mins * MIN,
            segment_count: (video_mins.max(audio_mins)) * 30,
            first_ns: Some(1_781_784_000_000_000_000),
            last_ns: Some(1_781_784_000_000_000_000 + video_mins.max(audio_mins) * MIN),
        }
    }

    #[test]
    fn empty_is_no_footage() {
        assert_eq!(render_footage_stats(&[], false, 0, 0), "No footage was recorded for that period.");
    }

    #[test]
    fn multi_camera_totals_and_breakdown() {
        let rows = vec![dev("cam-a", Some("Front door"), 31, 31), dev("cam-b", Some("Garage"), 11, 11)];
        let line = render_footage_stats(&rows, false, 1_781_784_000_000_000_000 + 86_400_000_000_000, 0);
        assert!(line.contains("42 minutes"), "total stated: {line}");
        assert!(line.contains("Front door: 31 minutes"), "per-device: {line}");
        assert!(line.contains("Garage: 11 minutes"), "per-device: {line}");
        assert!(line.contains("2 cameras"), "{line}");
    }

    #[test]
    fn single_camera_skips_breakdown() {
        let rows = vec![dev("cam-a", None, 4, 4)];
        let line = render_footage_stats(&rows, false, 0, 0);
        assert!(line.contains("4 minutes of video"), "{line}");
        assert!(line.contains("(cam-a)"), "falls back to device id: {line}");
        assert!(!line.contains("across"), "{line}");
    }

    #[test]
    fn audio_only_capture_is_reported_honestly() {
        let rows = vec![dev("cam-a", Some("Kitchen"), 0, 25)];
        let line = render_footage_stats(&rows, false, 0, 0);
        assert!(line.contains("25 minutes of audio"), "{line}");
        assert!(line.contains("audio only"), "{line}");
        // Asking for audio directly names the audio lane without the fallback note.
        let line = render_footage_stats(&rows, true, 0, 0);
        assert!(line.contains("25 minutes of audio") && !line.contains("audio only"), "{line}");
    }

    #[test]
    fn audio_lane_detection() {
        assert!(wants_audio_lane("how much audio do you have from today"));
        assert!(!wants_audio_lane("how many minutes of video do we have"));
        assert!(!wants_audio_lane("how much footage with audio"));
    }
}
