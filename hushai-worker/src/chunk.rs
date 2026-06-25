//! Pure transcript chunking: whisper utterances -> sentences with ABSOLUTE timestamps.
//!
//! Absolute time = `capture_start_unix_nanos + relative_ms * 1_000_000`. Within an
//! utterance, time is distributed across its sentences proportionally to character
//! length. Non-speech markers (e.g. `[BLANK_AUDIO]`) and empty text are dropped, so a
//! silent segment yields zero sentences (a valid "no speech" result).
//!
//! Kept side-effect free so the timestamp math + splitting are unit-tested directly.

use crate::asr::Utterance;

/// A sentence ready to embed + persist.
#[derive(Debug, Clone, PartialEq)]
pub struct Sentence {
    pub text: String,
    pub start_unix_nanos: i64,
    pub end_unix_nanos: i64,
}

const NANOS_PER_MS: i64 = 1_000_000;

/// Common whisper "no speech" outputs (compared case-insensitively after trim).
const NON_SPEECH_MARKERS: &[&str] = &[
    "[blank_audio]",
    "[ blank_audio ]",
    "[music]",
    "(music)",
    "[silence]",
    "(silence)",
    "[no speech]",
    "[inaudible]",
    "[ pause ]",
];

fn is_non_speech(text: &str) -> bool {
    let t = text.trim().to_ascii_lowercase();
    t.is_empty()
        || NON_SPEECH_MARKERS.contains(&t.as_str())
        // purely punctuation/symbols with no letters or digits
        || !t.chars().any(|c| c.is_alphanumeric())
        // whisper annotates non-speech audio as a fully bracketed/parenthesized
        // tag, e.g. "(dramatic music)", "[Music]", "(applause)" — drop those.
        || is_bracketed_annotation(&t)
}

/// True if the whole string is wrapped in `(...)` or `[...]` with no other content.
fn is_bracketed_annotation(t: &str) -> bool {
    (t.starts_with('(') && t.ends_with(')') && !t[1..].contains('('))
        || (t.starts_with('[') && t.ends_with(']') && !t[1..].contains('['))
}

/// Convert utterances into sentences anchored to absolute Unix-nanosecond time.
pub fn chunk_into_sentences(
    utterances: &[Utterance],
    capture_start_unix_nanos: i64,
) -> Vec<Sentence> {
    let mut out = Vec::new();
    for u in utterances {
        if is_non_speech(&u.text) {
            continue;
        }
        let pieces = split_sentences(&u.text);
        if pieces.is_empty() {
            continue;
        }
        let total_chars: usize = pieces.iter().map(|p| p.chars().count().max(1)).sum();
        let dur_ms = (u.end_ms - u.start_ms).max(0);

        let mut consumed = 0usize;
        for piece in &pieces {
            let start_frac = consumed as f64 / total_chars as f64;
            consumed += piece.chars().count().max(1);
            let end_frac = consumed as f64 / total_chars as f64;

            let start_ms = u.start_ms + (dur_ms as f64 * start_frac) as i64;
            let end_ms = u.start_ms + (dur_ms as f64 * end_frac) as i64;

            out.push(Sentence {
                text: piece.clone(),
                start_unix_nanos: capture_start_unix_nanos + start_ms * NANOS_PER_MS,
                end_unix_nanos: capture_start_unix_nanos + end_ms * NANOS_PER_MS,
            });
        }
    }
    out
}

/// Split into sentence-ish pieces on `.`/`?`/`!`, keeping the delimiter and trimming.
fn split_sentences(text: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        cur.push(ch);
        if matches!(ch, '.' | '?' | '!') {
            push_trimmed(&mut pieces, &cur);
            cur.clear();
        }
    }
    push_trimmed(&mut pieces, &cur);
    pieces
}

fn push_trimmed(pieces: &mut Vec<String>, s: &str) {
    let t = s.trim();
    if !t.is_empty() {
        pieces.push(t.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utt(text: &str, start_ms: i64, end_ms: i64) -> Utterance {
        Utterance {
            text: text.to_string(),
            start_ms,
            end_ms,
        }
    }

    #[test]
    fn single_sentence_absolute_timestamp_math() {
        let cap = 1_700_000_000_000_000_000; // arbitrary capture-start ns
        let out = chunk_into_sentences(&[utt("Hello world.", 0, 1000)], cap);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "Hello world.");
        // start at relative 0ms, end at relative 1000ms.
        assert_eq!(out[0].start_unix_nanos, cap);
        assert_eq!(out[0].end_unix_nanos, cap + 1000 * NANOS_PER_MS);
    }

    #[test]
    fn nonzero_offset_is_added() {
        let cap = 5_000_000_000;
        let out = chunk_into_sentences(&[utt("Hi there.", 200, 700)], cap);
        assert_eq!(out[0].start_unix_nanos, cap + 200 * NANOS_PER_MS);
        assert_eq!(out[0].end_unix_nanos, cap + 700 * NANOS_PER_MS);
    }

    #[test]
    fn multiple_sentences_split_and_share_time_in_order() {
        // Two equal-length sentences over a 1000ms utterance -> ~50/50 split, monotonic.
        let out = chunk_into_sentences(&[utt("Aaaa. Bbbb.", 0, 1000)], 0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "Aaaa.");
        assert_eq!(out[1].text, "Bbbb.");
        assert_eq!(out[0].start_unix_nanos, 0);
        assert!(out[0].end_unix_nanos <= out[1].start_unix_nanos);
        assert_eq!(out[1].end_unix_nanos, 1000 * NANOS_PER_MS);
    }

    #[test]
    fn non_speech_and_empty_yield_nothing() {
        assert!(chunk_into_sentences(&[utt("[BLANK_AUDIO]", 0, 500)], 0).is_empty());
        assert!(chunk_into_sentences(&[utt("   ", 0, 500)], 0).is_empty());
        assert!(chunk_into_sentences(&[utt("[Music]", 0, 500)], 0).is_empty());
        // bracketed/parenthesized sound annotations whisper emits for non-speech audio
        assert!(chunk_into_sentences(&[utt("(dramatic music)", 0, 500)], 0).is_empty());
        assert!(chunk_into_sentences(&[utt("(upbeat music)", 0, 500)], 0).is_empty());
        assert!(chunk_into_sentences(&[utt("[applause]", 0, 500)], 0).is_empty());
        assert!(chunk_into_sentences(&[], 12345).is_empty());
    }

    #[test]
    fn real_speech_is_kept_even_with_incidental_parens() {
        // A sentence that merely *contains* parentheses is real speech, not an annotation.
        let out = chunk_into_sentences(&[utt("We met (briefly) on Tuesday.", 0, 100)], 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "We met (briefly) on Tuesday.");
    }

    #[test]
    fn text_without_terminator_is_kept() {
        let out = chunk_into_sentences(&[utt("no terminator here", 0, 100)], 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "no terminator here");
    }
}
