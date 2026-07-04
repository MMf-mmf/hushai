//! Human-readable rendering of recording timestamps for the assistant.
//!
//! Turns a raw `start_unix_nanos` into the way a person would say it out loud —
//! "yesterday at 5:14 PM", "three days ago at 2:00 PM", "on March 4th at 9:07 AM".
//! This string is computed once, server-side, and attached to each retrieved `Source`
//! (see `retrieve::enrich_for_display`), so the LLM prompt, the web citation chip, and the
//! persisted transcript all show the IDENTICAL phrasing: the model never sees a raw number
//! or id (so it can't echo one), and the web never re-derives the time (no timezone/DST
//! drift between the spoken answer and the chip).
//!
//! `tz_offset_secs` is the same fixed offset analytics uses (`ANALYSIS_TZ_OFFSET_SECS`): a
//! deterministic UTC offset (no DST), applied before reading civil-time fields, exactly as
//! `analytics::bucket` does.

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};

/// Render `start_unix_nanos` relative to `now_unix_nanos`, both shifted by `tz_offset_secs`
/// into local civil time. See the module docs for the phrasing scheme.
pub fn humanize_time(start_unix_nanos: i64, now_unix_nanos: i64, tz_offset_secs: i64) -> String {
    let offset = Duration::seconds(tz_offset_secs);
    let start: DateTime<Utc> = DateTime::from_timestamp_nanos(start_unix_nanos) + offset;
    let now: DateTime<Utc> = DateTime::from_timestamp_nanos(now_unix_nanos) + offset;

    let clock = clock12(start.hour(), start.minute());
    // Calendar-day difference of the LOCAL dates (not 24h windows), so an 11pm -> 1am
    // crossing still reads "yesterday".
    let delta_days = (now.date_naive() - start.date_naive()).num_days();

    match delta_days {
        d if d < 0 => "just now".to_string(),
        0 => format!("today at {clock}"),
        1 => format!("yesterday at {clock}"),
        2..=6 => format!("{} days ago at {clock}", spell_small(delta_days)),
        7..=13 => format!("last {} at {clock}", start.format("%A")),
        _ => {
            let month = start.format("%B"); // e.g. "March"
            let day = ordinal(start.day());
            if start.year() == now.year() {
                format!("on {month} {day} at {clock}")
            } else {
                format!("on {month} {day}, {} at {clock}", start.year())
            }
        }
    }
}

/// Absolute local civil date-and-time, e.g. "Friday, July 3, 2026 at 2:15 PM" — for the
/// assistant's system briefing so it knows "today" without a raw timestamp. Same fixed-offset
/// arithmetic as [`humanize_time`]; reuses `clock12`/`ordinal`.
pub fn absolute_time(now_unix_nanos: i64, tz_offset_secs: i64) -> String {
    let now: DateTime<Utc> =
        DateTime::from_timestamp_nanos(now_unix_nanos) + Duration::seconds(tz_offset_secs);
    let weekday = now.format("%A"); // e.g. "Friday"
    let month = now.format("%B"); // e.g. "July"
    let day = ordinal(now.day());
    let clock = clock12(now.hour(), now.minute());
    format!("{weekday}, {month} {day}, {} at {clock}", now.year())
}

/// 12-hour clock with AM/PM, e.g. "5:14 PM", "9:07 AM", "12:00 PM" (noon), "12:00 AM"
/// (midnight). Built manually rather than via chrono's `%-I`, which isn't portable.
fn clock12(hour24: u32, minute: u32) -> String {
    let period = if hour24 < 12 { "AM" } else { "PM" };
    let h12 = match hour24 % 12 {
        0 => 12,
        h => h,
    };
    format!("{h12}:{minute:02} {period}")
}

/// Spell out a small day count (the 2..=6 band) so it reads/speaks naturally.
fn spell_small(n: i64) -> &'static str {
    match n {
        2 => "two",
        3 => "three",
        4 => "four",
        5 => "five",
        6 => "six",
        _ => "several", // unreachable for the 2..=6 caller; defensive.
    }
}

/// Ordinal day-of-month, e.g. 1 -> "1st", 2 -> "2nd", 3 -> "3rd", 4 -> "4th", 11 -> "11th".
fn ordinal(day: u32) -> String {
    let suffix = match (day % 10, day % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{day}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UTC nanos for a civil instant (tests pass tz_offset_secs separately).
    fn nanos(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
            .and_utc()
            .timestamp_nanos_opt()
            .unwrap()
    }

    // Friday, 2026-06-26 17:00:00 UTC.
    fn now() -> i64 {
        nanos(2026, 6, 26, 17, 0)
    }

    /// Output must never leak a machine value: no `t=`, no ISO `T`, no long digit run.
    fn assert_human(s: &str) {
        assert!(!s.contains("t="), "leaked t= marker: {s}");
        // ISO timestamps put a `T` between the date and time digits (e.g. "25T17"); a
        // weekday like "Thursday" also contains a 'T', so only flag the digit-T-digit form.
        let has_iso = s
            .as_bytes()
            .windows(3)
            .any(|w| w[0].is_ascii_digit() && w[1] == b'T' && w[2].is_ascii_digit());
        assert!(!has_iso, "leaked ISO date marker: {s}");
        let longest_digit_run = s
            .split(|c: char| !c.is_ascii_digit())
            .map(str::len)
            .max()
            .unwrap_or(0);
        assert!(longest_digit_run <= 4, "leaked long numeric run in: {s}");
    }

    #[test]
    fn today_morning() {
        let s = humanize_time(nanos(2026, 6, 26, 9, 7), now(), 0);
        assert_eq!(s, "today at 9:07 AM");
        assert_human(&s);
    }

    #[test]
    fn yesterday_afternoon() {
        let s = humanize_time(nanos(2026, 6, 25, 17, 14), now(), 0);
        assert_eq!(s, "yesterday at 5:14 PM");
        assert_human(&s);
    }

    #[test]
    fn three_days_ago_spelled_out() {
        let s = humanize_time(nanos(2026, 6, 23, 14, 0), now(), 0);
        assert_eq!(s, "three days ago at 2:00 PM");
        assert_human(&s);
    }

    #[test]
    fn within_two_weeks_uses_weekday() {
        // 2026-06-18 is a Thursday; 8 days before 06-26.
        let s = humanize_time(nanos(2026, 6, 18, 10, 30), now(), 0);
        assert_eq!(s, "last Thursday at 10:30 AM");
        assert_human(&s);
    }

    #[test]
    fn older_same_year_uses_month_and_ordinal() {
        let s = humanize_time(nanos(2026, 3, 4, 17, 0), now(), 0);
        assert_eq!(s, "on March 4th at 5:00 PM");
        assert_human(&s);
    }

    #[test]
    fn prior_year_includes_year() {
        let s = humanize_time(nanos(2025, 12, 31, 23, 50), now(), 0);
        assert_eq!(s, "on December 31st, 2025 at 11:50 PM");
        assert_human(&s);
    }

    #[test]
    fn noon_and_midnight_wording() {
        assert_eq!(
            humanize_time(nanos(2026, 6, 26, 12, 0), now(), 0),
            "today at 12:00 PM"
        );
        assert_eq!(
            humanize_time(nanos(2026, 6, 26, 0, 0), now(), 0),
            "today at 12:00 AM"
        );
    }

    #[test]
    fn future_or_clock_skew_is_just_now() {
        assert_eq!(
            humanize_time(nanos(2026, 6, 28, 12, 0), now(), 0),
            "just now"
        );
        // Exactly now -> today.
        assert_eq!(humanize_time(now(), now(), 0), "today at 5:00 PM");
    }

    #[test]
    fn tz_offset_reaches_both_start_and_now() {
        // 02:00 UTC on 06-26. At offset 0 it's "today at 2:00 AM"; at EST (-5h) the local
        // wall clock is 21:00 on 06-25 while "now" becomes noon 06-26 -> "yesterday".
        let start = nanos(2026, 6, 26, 2, 0);
        assert_eq!(humanize_time(start, now(), 0), "today at 2:00 AM");
        assert_eq!(
            humanize_time(start, now(), -5 * 3600),
            "yesterday at 9:00 PM"
        );
    }

    #[test]
    fn ordinal_suffixes() {
        assert_eq!(ordinal(1), "1st");
        assert_eq!(ordinal(2), "2nd");
        assert_eq!(ordinal(3), "3rd");
        assert_eq!(ordinal(4), "4th");
        assert_eq!(ordinal(11), "11th");
        assert_eq!(ordinal(12), "12th");
        assert_eq!(ordinal(13), "13th");
        assert_eq!(ordinal(21), "21st");
        assert_eq!(ordinal(22), "22nd");
        assert_eq!(ordinal(23), "23rd");
    }
}
