//! Minimal, deterministic natural-language time-window parser for the recency path.
//!
//! "what did we discuss YESTERDAY / THIS MORNING / LAST WEEK" needs a concrete
//! `[after, before)` UTC-nanosecond window. This module recognizes a deliberately CLOSED set
//! of everyday phrases and computes the window in local civil time using the same fixed-offset
//! arithmetic as `humanize.rs`/`analytics::bucket` (a UTC offset, not full DST). It is the only
//! NL time parsing in the codebase — kept tiny on purpose; other paths can adopt it later.
//!
//! No phrase → `None` (the caller falls back to "the most recent activity, whenever it was").

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Utc};

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Resolve a supported time phrase in `query` to a `[after, before)` window in UTC nanoseconds,
/// computed in the caller's local civil time (`tz_offset_secs`). `None` when the query names no
/// supported window. More-specific phrases are checked before broader ones ("this morning" and
/// "last night" before "today"/"this week") so the narrowest match wins.
pub fn window_in_query(query: &str, now_unix_nanos: i64, tz_offset_secs: i64) -> Option<(i64, i64)> {
    let q = query.to_lowercase();
    let now_local = local_civil(now_unix_nanos, tz_offset_secs);
    let today = now_local.date_naive();
    let yesterday = today.pred_opt()?;

    // Relative durations first ("last 10 minutes", "past 2 hours", "last hour"): they're the
    // narrowest ask and purely now-anchored, so no civil-time arithmetic applies. Checked before
    // the calendar phrases so "in the last 10 minutes today" gets the 10-minute window.
    if let Some(win) = relative_window(&q, now_unix_nanos) {
        return Some(win);
    }

    // Part-of-day and "last night" first — they're narrower than the day/week windows and their
    // key words ("morning", "night") don't collide with the broader phrases.
    if q.contains("this morning") {
        return Some(day_span(today, 0, 12, tz_offset_secs));
    }
    if q.contains("this afternoon") {
        return Some(day_span(today, 12, 18, tz_offset_secs));
    }
    if q.contains("this evening") || q.contains("tonight") {
        return Some(day_span(today, 18, 24, tz_offset_secs));
    }
    if q.contains("last night") {
        // The night that just passed: yesterday 20:00 through today 04:00 (local).
        let after = civil_to_utc_nanos(yesterday.and_hms_opt(20, 0, 0)?, tz_offset_secs);
        let before = civil_to_utc_nanos(today.and_hms_opt(4, 0, 0)?, tz_offset_secs);
        return Some((after, before));
    }
    if q.contains("yesterday") {
        return Some(day_span(yesterday, 0, 24, tz_offset_secs));
    }
    if q.contains("today") {
        return Some(day_span(today, 0, 24, tz_offset_secs));
    }
    // Calendar weeks, Monday-start (ISO). "last week" before "this week" (substring safety).
    if q.contains("last week") {
        let this_monday = monday_of(today);
        let last_monday = this_monday - Duration::days(7);
        return Some((
            civil_to_utc_nanos(last_monday.and_hms_opt(0, 0, 0)?, tz_offset_secs),
            civil_to_utc_nanos(this_monday.and_hms_opt(0, 0, 0)?, tz_offset_secs),
        ));
    }
    if q.contains("this week") {
        let this_monday = monday_of(today);
        let next_monday = this_monday + Duration::days(7);
        return Some((
            civil_to_utc_nanos(this_monday.and_hms_opt(0, 0, 0)?, tz_offset_secs),
            civil_to_utc_nanos(next_monday.and_hms_opt(0, 0, 0)?, tz_offset_secs),
        ));
    }
    None
}

/// "last/past N minutes|hours|days" (or a bare "last minute/hour/day" = 1 unit) as a
/// `[now - N·unit, now)` window. Same closed-set philosophy as the calendar phrases: only
/// minute/hour/day units — "last week"/"last night" stay with their calendar handlers, and
/// anything else after last/past ("last time I saw Bob") is no match.
fn relative_window(q: &str, now_unix_nanos: i64) -> Option<(i64, i64)> {
    let words: Vec<&str> = q
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect();
    for (i, w) in words.iter().enumerate() {
        if *w != "last" && *w != "past" {
            continue;
        }
        let (n, unit_word) = match words.get(i + 1) {
            Some(next) => match next.parse::<i64>() {
                Ok(n) if (1..=100_000).contains(&n) => (n, words.get(i + 2).copied()),
                Ok(_) => continue,
                // Bare unit: "the last hour" / "the past minute" = one of that unit.
                Err(_) => (1, Some(*next)),
            },
            None => continue,
        };
        let unit_secs = match unit_word {
            Some("minute") | Some("minutes") | Some("min") | Some("mins") => 60,
            Some("hour") | Some("hours") | Some("hr") | Some("hrs") => 3600,
            Some("day") | Some("days") => 86_400,
            _ => continue,
        };
        let span = n.saturating_mul(unit_secs).saturating_mul(NANOS_PER_SEC);
        return Some((now_unix_nanos.saturating_sub(span), now_unix_nanos));
    }
    None
}

/// Local civil datetime for a UTC instant, shifted by the fixed offset (mirrors `humanize.rs`).
fn local_civil(now_unix_nanos: i64, tz_offset_secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_nanos(now_unix_nanos) + Duration::seconds(tz_offset_secs)
}

/// UTC nanos for a LOCAL civil datetime: the naive wall-clock value minus the offset. E.g. local
/// 00:00 at −5h is 05:00 UTC.
fn civil_to_utc_nanos(local: NaiveDateTime, tz_offset_secs: i64) -> i64 {
    local.and_utc().timestamp_nanos_opt().unwrap_or(0) - tz_offset_secs * NANOS_PER_SEC
}

/// `[date @ start_hour, date @ end_hour)` as a UTC-nanos window. `end_hour == 24` means the next
/// day's 00:00.
fn day_span(date: NaiveDate, start_hour: u32, end_hour: u32, tz_offset_secs: i64) -> (i64, i64) {
    let start = date.and_hms_opt(start_hour, 0, 0).unwrap_or_default();
    let end = if end_hour >= 24 {
        date.succ_opt()
            .unwrap_or(date)
            .and_hms_opt(0, 0, 0)
            .unwrap_or_default()
    } else {
        date.and_hms_opt(end_hour, 0, 0).unwrap_or_default()
    };
    (
        civil_to_utc_nanos(start, tz_offset_secs),
        civil_to_utc_nanos(end, tz_offset_secs),
    )
}

/// The Monday (00:00 civil date) of the ISO week containing `date`.
fn monday_of(date: NaiveDate) -> NaiveDate {
    let back = date.weekday().num_days_from_monday() as i64;
    date - Duration::days(back)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    fn nanos(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
            .and_utc()
            .timestamp_nanos_opt()
            .unwrap()
    }

    /// Round-trip a UTC-nanos boundary back to local civil for readable assertions.
    fn civil(n: i64, tz: i64) -> (i32, u32, u32, u32, u32) {
        let d = super::local_civil(n, tz);
        (d.year(), d.month(), d.day(), d.hour(), d.minute())
    }

    // Friday, 2026-07-03 14:00:00 UTC; tz offset 0 unless a test says otherwise.
    fn now() -> i64 {
        nanos(2026, 7, 3, 14, 0)
    }

    #[test]
    fn no_phrase_is_none() {
        assert!(window_in_query("what did we discuss", now(), 0).is_none());
        assert!(window_in_query("who was speaking", now(), 0).is_none());
    }

    #[test]
    fn today_and_yesterday_span_full_local_days() {
        let (a, b) = window_in_query("what did we talk about today", now(), 0).unwrap();
        assert_eq!(civil(a, 0), (2026, 7, 3, 0, 0));
        assert_eq!(civil(b, 0), (2026, 7, 4, 0, 0));

        let (a, b) = window_in_query("anything from yesterday?", now(), 0).unwrap();
        assert_eq!(civil(a, 0), (2026, 7, 2, 0, 0));
        assert_eq!(civil(b, 0), (2026, 7, 3, 0, 0));
    }

    #[test]
    fn parts_of_day() {
        let (a, b) = window_in_query("this morning", now(), 0).unwrap();
        assert_eq!(civil(a, 0), (2026, 7, 3, 0, 0));
        assert_eq!(civil(b, 0), (2026, 7, 3, 12, 0));

        let (a, b) = window_in_query("this afternoon", now(), 0).unwrap();
        assert_eq!(civil(a, 0), (2026, 7, 3, 12, 0));
        assert_eq!(civil(b, 0), (2026, 7, 3, 18, 0));

        let (a, b) = window_in_query("tonight", now(), 0).unwrap();
        assert_eq!(civil(a, 0), (2026, 7, 3, 18, 0));
        assert_eq!(civil(b, 0), (2026, 7, 4, 0, 0));
    }

    #[test]
    fn last_night_crosses_midnight() {
        let (a, b) = window_in_query("what happened last night", now(), 0).unwrap();
        assert_eq!(civil(a, 0), (2026, 7, 2, 20, 0));
        assert_eq!(civil(b, 0), (2026, 7, 3, 4, 0));
    }

    #[test]
    fn calendar_weeks_monday_start() {
        // 2026-07-03 is a Friday; this ISO week's Monday is 2026-06-29.
        let (a, b) = window_in_query("this week", now(), 0).unwrap();
        assert_eq!(civil(a, 0), (2026, 6, 29, 0, 0));
        assert_eq!(civil(b, 0), (2026, 7, 6, 0, 0));

        let (a, b) = window_in_query("last week", now(), 0).unwrap();
        assert_eq!(civil(a, 0), (2026, 6, 22, 0, 0));
        assert_eq!(civil(b, 0), (2026, 6, 29, 0, 0));
    }

    #[test]
    fn more_specific_phrase_wins() {
        // "this morning" must win over the substring "today" appearing elsewhere.
        let (_, b) = window_in_query("did we discuss the plan this morning today?", now(), 0).unwrap();
        assert_eq!(civil(b, 0), (2026, 7, 3, 12, 0), "morning window, not full day");
        // "last week" must win over "this week" (the substring order guard).
        let (a, _) = window_in_query("last week's meeting", now(), 0).unwrap();
        assert_eq!(civil(a, 0), (2026, 6, 22, 0, 0));
    }

    #[test]
    fn relative_durations() {
        let (a, b) = window_in_query("how many people in the last 10 minutes?", now(), 0).unwrap();
        assert_eq!(b, now());
        assert_eq!(a, now() - 10 * 60 * NANOS_PER_SEC);

        let (a, b) = window_in_query("past 2 hours", now(), 0).unwrap();
        assert_eq!(b, now());
        assert_eq!(a, now() - 2 * 3600 * NANOS_PER_SEC);

        // Punctuation and short units: "min?" still parses.
        let (a, _) = window_in_query("anything in the last 10 min?", now(), 0).unwrap();
        assert_eq!(a, now() - 10 * 60 * NANOS_PER_SEC);

        // Bare unit = one of it.
        let (a, _) = window_in_query("what happened in the last hour", now(), 0).unwrap();
        assert_eq!(a, now() - 3600 * NANOS_PER_SEC);

        let (a, _) = window_in_query("deliveries in the last 3 days", now(), 0).unwrap();
        assert_eq!(a, now() - 3 * 86_400 * NANOS_PER_SEC);
    }

    #[test]
    fn relative_duration_beats_calendar_phrase() {
        // Both "last 10 minutes" and "today" present → the narrower relative window wins.
        let (a, b) = window_in_query("who came by today in the last 10 minutes", now(), 0).unwrap();
        assert_eq!((a, b), (now() - 10 * 60 * NANOS_PER_SEC, now()));
    }

    #[test]
    fn relative_negatives_fall_through() {
        // "last week" is a calendar phrase, not a relative-duration match.
        let (a, _) = window_in_query("last week", now(), 0).unwrap();
        assert_eq!(civil(a, 0), (2026, 6, 22, 0, 0));
        // "last night" likewise.
        let (a, _) = window_in_query("last night", now(), 0).unwrap();
        assert_eq!(civil(a, 0), (2026, 7, 2, 20, 0));
        // "last time ..." is no window at all.
        assert!(window_in_query("when was the last time i saw bob", now(), 0).is_none());
        // Zero/absurd counts don't match.
        assert!(window_in_query("last 0 minutes", now(), 0).is_none());
    }

    #[test]
    fn tz_offset_shifts_the_window() {
        // At −5h (EST), "today" local starts at 05:00 UTC and ends at 05:00 UTC next day.
        let tz = -5 * 3600;
        let (a, b) = window_in_query("today", now(), tz).unwrap();
        // Boundaries are local midnights; expressed in UTC that's 05:00.
        assert_eq!(DateTime::from_timestamp_nanos(a).hour(), 5);
        assert_eq!(DateTime::from_timestamp_nanos(b).hour(), 5);
        // Round-tripped to local civil, they're clean day boundaries.
        assert_eq!(civil(a, tz), (2026, 7, 3, 0, 0));
        assert_eq!(civil(b, tz), (2026, 7, 4, 0, 0));
    }
}
