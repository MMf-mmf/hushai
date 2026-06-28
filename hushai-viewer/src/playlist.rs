//! HLS playlist generation (VOD). Segments carry `#EXT-X-PROGRAM-DATE-TIME` (absolute
//! wall clock from `capture_start_unix_nanos`) at the start of each continuous run, and
//! `#EXT-X-DISCONTINUITY` at every real break (session change, `gap_before`, a stream
//! change, or a non-contiguous wall-clock step). That lets hls.js map wall-clock time to
//! media position and play across sessions/gaps/resolution-changes seamlessly.
//!
//! Android uploads separate video + audio streams, served as a video variant plus an HLS
//! alternate-audio rendition (synced by PDT). `fmp4` muxed sessions are a single variant.

use std::fmt::Write;

use crate::timeline::SegmentRow;

/// Tolerance for "contiguous" — within this many ns of the previous segment's end, a
/// segment continues the current run; otherwise it's a discontinuity (gap/overlap).
const CONTIGUITY_TOLERANCE_NANOS: i64 = 300_000_000; // 300ms

fn iso_from_nanos(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let sub = ns.rem_euclid(1_000_000_000) as u32;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, sub)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// A media playlist for one kind (variant), built from already-ordered rows.
pub fn media_playlist(rows: &[SegmentRow], variant: &str) -> String {
    let target = rows
        .iter()
        .map(|r| (r.duration_nanos + 999_999_999) / 1_000_000_000)
        .max()
        .unwrap_or(1)
        .max(1);

    let mut out = String::with_capacity(128 + rows.len() * 96);
    let _ = write!(
        out,
        "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:0\n",
    );

    let mut prev: Option<&SegmentRow> = None;
    for row in rows {
        let mut discontinuity = false;
        if let Some(p) = prev {
            let prev_end = p.capture_start_unix_nanos.saturating_add(p.duration_nanos);
            let delta = row.capture_start_unix_nanos - prev_end;
            discontinuity = p.stream_id != row.stream_id
                || p.session_id != row.session_id
                || row.gap_before
                || delta.abs() > CONTIGUITY_TOLERANCE_NANOS;
        }
        let run_start = prev.is_none() || discontinuity;

        if discontinuity {
            out.push_str("#EXT-X-DISCONTINUITY\n");
        }
        if run_start {
            let _ = writeln!(
                out,
                "#EXT-X-PROGRAM-DATE-TIME:{}",
                iso_from_nanos(row.capture_start_unix_nanos)
            );
        }
        let secs = row.duration_nanos as f64 / 1_000_000_000.0;
        let _ = write!(out, "#EXTINF:{secs:.3},\n");
        let _ = writeln!(out, "/hls/seg/{}.{}.ts", row.sha_hex, variant);

        prev = Some(row);
    }

    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// The master playlist for a device window. `media_types` is the distinct set present
/// (1=audio, 2=video, 3=muxed). Child playlist URLs are relative to the master.
pub fn master_playlist(media_types: &[i32], from: i64, to: i64) -> String {
    let has_video = media_types.contains(&2);
    let has_audio = media_types.contains(&1);
    let has_muxed = media_types.contains(&3);
    let q = format!("from={from}&to={to}");

    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:6\n");

    if has_video {
        if has_audio {
            let _ = writeln!(
                out,
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Audio\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio.m3u8?{q}\""
            );
            let _ = writeln!(
                out,
                "#EXT-X-STREAM-INF:BANDWIDTH=4500000,CODECS=\"avc1.640028,mp4a.40.2\",AUDIO=\"aud\""
            );
        } else {
            let _ = writeln!(out, "#EXT-X-STREAM-INF:BANDWIDTH=4000000,CODECS=\"avc1.640028\"");
        }
        let _ = writeln!(out, "video.m3u8?{q}");
    } else if has_muxed {
        let _ = writeln!(
            out,
            "#EXT-X-STREAM-INF:BANDWIDTH=4500000,CODECS=\"avc1.640028,mp4a.40.2\""
        );
        let _ = writeln!(out, "muxed.m3u8?{q}");
    } else if has_audio {
        let _ = writeln!(out, "#EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.2\"");
        let _ = writeln!(out, "audio.m3u8?{q}");
    } else {
        // No media in window; emit a (will-be-empty) video variant so hls.js has a target.
        let _ = writeln!(out, "#EXT-X-STREAM-INF:BANDWIDTH=4000000");
        let _ = writeln!(out, "video.m3u8?{q}");
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn row(stream: &str, session: Uuid, mt: i32, start: i64, dur: i64, gap: bool) -> SegmentRow {
        SegmentRow {
            stream_id: stream.into(),
            session_id: session,
            sequence: 0,
            media_type: mt,
            container: "mp4".into(),
            sha_hex: "ab".repeat(32),
            capture_start_unix_nanos: start,
            duration_nanos: dur,
            gap_before: gap,
        }
    }

    #[test]
    fn contiguous_run_has_one_pdt_no_discontinuity() {
        let s = Uuid::now_v7();
        let d = 2_000_000_000;
        let rows = vec![
            row("cam0-video", s, 2, 0, d, false),
            row("cam0-video", s, 2, d, d, false),
        ];
        let pl = media_playlist(&rows, "video");
        assert_eq!(pl.matches("#EXT-X-PROGRAM-DATE-TIME").count(), 1);
        assert_eq!(pl.matches("#EXT-X-DISCONTINUITY").count(), 0);
        assert_eq!(pl.matches("#EXTINF").count(), 2);
        assert!(pl.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn gap_inserts_discontinuity_and_new_pdt() {
        let s = Uuid::now_v7();
        let d = 2_000_000_000;
        let rows = vec![
            row("cam0-video", s, 2, 0, d, false),
            row("cam0-video", s, 2, 100 * d, d, true),
        ];
        let pl = media_playlist(&rows, "video");
        assert_eq!(pl.matches("#EXT-X-DISCONTINUITY").count(), 1);
        assert_eq!(pl.matches("#EXT-X-PROGRAM-DATE-TIME").count(), 2);
    }

    #[test]
    fn master_with_video_and_audio_declares_alt_audio() {
        let m = master_playlist(&[2, 1], 10, 20);
        assert!(m.contains("#EXT-X-MEDIA:TYPE=AUDIO"));
        assert!(m.contains("AUDIO=\"aud\""));
        assert!(m.contains("video.m3u8?from=10&to=20"));
        assert!(m.contains("audio.m3u8?from=10&to=20"));
    }
}
