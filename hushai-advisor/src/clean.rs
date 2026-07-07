//! Deterministic OCR cleanup for the ingested chapter texts.
//!
//! The chapter files (`Agent Ahithophel/books/chapters_text/N.txt`) are raw PDF text
//! extraction with predictable artifacts (all observed in the corpus):
//!   - a leading line holding just the chapter number,
//!   - the chapter-title question hard-wrapped across a few lines,
//!   - body lines hard-wrapped mid-sentence, with `-` splits at line breaks ("writ- / ers"),
//!   - stray page-number lines (a bare `9` mid-chapter),
//!   - a decorative drop-cap split off its word at the body start ("C) alleen Szot").
//!
//! Deterministic-first is the repo idiom (cf. hushai-rag's analytics digest): these
//! heuristics fix the mechanical artifacts; an optional LLM pass in `ingest.rs` handles
//! the long tail, guarded so it can never rewrite the chapter wholesale.

/// A cleaned chapter: the extracted title question (when the standard layout is found)
/// and the body re-flowed into paragraphs.
#[derive(Debug, Clone, PartialEq)]
pub struct Cleaned {
    pub title: Option<String>,
    pub body: String,
}

/// Clean one raw chapter text. `strip_lines` are running-head lines (the book title
/// repeated at page tops — e.g. a lone "Yes!" mid-chapter) dropped wherever a whole
/// line equals one, the same way page numbers are. Never fails; worst case the body is
/// the trimmed input.
pub fn clean_chapter(raw: &str, strip_lines: &[&str]) -> Cleaned {
    let lines: Vec<&str> = raw.lines().map(str::trim_end).collect();
    let mut idx = 0;

    // Skip leading blank lines and the chapter-number line ("1").
    while idx < lines.len() && lines[idx].trim().is_empty() {
        idx += 1;
    }
    if idx < lines.len() && is_page_number(lines[idx]) {
        idx += 1;
    }

    // Title: the opening question, hard-wrapped over the next few non-empty lines and
    // ending with '?'. Only claimed when it appears within the first 8 lines of the
    // chapter — otherwise leave the text alone and let the body flow handle it.
    let mut title: Option<String> = None;
    {
        while idx < lines.len() && lines[idx].trim().is_empty() {
            idx += 1;
        }
        let start = idx;
        let mut parts: Vec<&str> = Vec::new();
        let mut end = None;
        for (offset, line) in lines[start..].iter().take(8).enumerate() {
            let t = line.trim();
            if t.is_empty() {
                break;
            }
            parts.push(t);
            if t.ends_with('?') {
                end = Some(start + offset + 1);
                break;
            }
        }
        if let Some(e) = end {
            title = Some(parts.join(" "));
            idx = e;
        }
    }

    // Body: re-flow hard-wrapped lines into paragraphs. Blank lines delimit paragraphs;
    // lone page-number lines are dropped; a trailing `-` followed by a lowercase
    // continuation is a hyphenated line-break split, joined without the hyphen.
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in &lines[idx.min(lines.len())..] {
        let t = line.trim();
        if t.is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
            continue;
        }
        if is_page_number(t) || strip_lines.iter().any(|s| t == s.trim()) {
            continue;
        }
        if current.is_empty() {
            current.push_str(t);
        } else if current.ends_with('-') && t.starts_with(|c: char| c.is_lowercase()) {
            // "writ-\ners" -> "writers". A hyphen before an Uppercase continuation is
            // kept (it's likely a real compound, e.g. "Nordic-\nTrac" stays hyphenated).
            current.pop();
            current.push_str(t);
        } else {
            current.push(' ');
            current.push_str(t);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }

    // Merge falsely-split paragraphs. The extractor breaks paragraphs mid-sentence
    // around page numbers and drop-caps (observed: "...successful writ-" / blank /
    // "/ ers in the paid..."): a paragraph ending unfinished (hyphen, comma, or a bare
    // lowercase letter) followed by one starting lowercase (or with a quote) is one
    // paragraph. A leading "/ " on the continuation is drop-cap debris — dropped.
    let paragraphs = merge_continuations(paragraphs);
    let mut paragraphs = paragraphs;

    if let Some(first) = paragraphs.first_mut() {
        *first = fix_drop_cap(first);
    }

    let body = paragraphs
        .iter()
        .map(|p| collapse_spaces(p))
        .collect::<Vec<_>>()
        .join("\n\n");

    Cleaned { title, body }
}

/// Merge paragraphs the extractor split mid-sentence (page breaks, drop-cap debris).
fn merge_continuations(paragraphs: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in paragraphs {
        let stripped = p
            .strip_prefix('/')
            .map(str::trim_start)
            .unwrap_or(p.as_str());
        let starts_continuation = stripped
            .chars()
            .next()
            .map(|c| c.is_lowercase() || matches!(c, '"' | '\u{201C}'))
            .unwrap_or(false);
        if let Some(prev) = out.last_mut() {
            let unfinished = prev.ends_with('-')
                || prev.ends_with(',')
                || prev.ends_with(|c: char| c.is_lowercase());
            if unfinished && starts_continuation {
                if prev.ends_with('-') && stripped.starts_with(|c: char| c.is_lowercase()) {
                    prev.pop(); // hyphenated split: join without the hyphen
                } else {
                    prev.push(' ');
                }
                prev.push_str(stripped);
                continue;
            }
        }
        out.push(stripped.to_string());
    }
    out
}

/// A line that is only a page number (digits, possibly surrounded by stray punctuation
/// the extractor left behind). "9" -> true, "9." -> true, "1984 was" -> false.
fn is_page_number(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.len() > 6 {
        return false;
    }
    let mut saw_digit = false;
    for c in t.chars() {
        if c.is_ascii_digit() {
            saw_digit = true;
        } else if !matches!(c, '.' | ',' | '|' | '·') {
            return false;
        }
    }
    saw_digit
}

/// Repair the decorative drop-cap the extractor split off its word at the body start:
/// "C) alleen Szot is..." -> "Colleen Szot is...". The capital may be followed by a stray
/// `)`/`.` glyph. Without such a glyph, "A" and "I" are left alone (real one-letter words).
fn fix_drop_cap(paragraph: &str) -> String {
    let mut chars = paragraph.chars();
    let Some(first) = chars.next() else {
        return paragraph.to_string();
    };
    if !first.is_ascii_uppercase() {
        return paragraph.to_string();
    }
    let rest: String = chars.collect();
    let (glyph, after) = match rest.strip_prefix(')').or_else(|| rest.strip_prefix('.')) {
        Some(a) => (true, a),
        None => (false, rest.as_str()),
    };
    if !glyph && matches!(first, 'A' | 'I') {
        return paragraph.to_string();
    }
    let after = after.trim_start();
    // The split-off remainder of the word: a lowercase run of at least 2 letters.
    let word_len = after.chars().take_while(|c| c.is_ascii_lowercase()).count();
    if word_len < 2 {
        return paragraph.to_string();
    }
    format!("{first}{after}")
}

fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c == ' ' {
            if !prev_space {
                out.push(c);
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// Pack paragraphs into chunks of roughly `target_chars` (never splitting a paragraph;
/// a single over-long paragraph becomes its own chunk). Used by ingest to produce the
/// `book_chunks` rows (~1,500 chars ≈ 350–400 tokens each).
pub fn chunk_paragraphs(body: &str, target_chars: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for para in body.split("\n\n").filter(|p| !p.trim().is_empty()) {
        if !current.is_empty() && current.len() + 2 + para.len() > target_chars {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(para);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::{Cleaned, chunk_paragraphs, clean_chapter};

    #[test]
    fn cleans_the_observed_chapter_one_artifacts() {
        // Mirrors the real 1.txt layout: number line, wrapped title question, drop-cap
        // split with the "/ " debris line after a blank, a lone page number splitting a
        // sentence, and a normal following paragraph.
        let raw = "1\n\nHow can inconveniencing your\naudience increase your\npersuasiveness?\n\n\
                   C) alleen Szot is one of the most successful writ-\n\n/ ers in the paid \
                   programming industry. And for\ngood reason,\n\n9\n\nshe recently authored a \
                   program.\n\nA new paragraph starts here.\n";
        let Cleaned { title, body } = clean_chapter(raw, &[]);
        assert_eq!(
            title.as_deref(),
            Some("How can inconveniencing your audience increase your persuasiveness?")
        );
        // Drop-cap rejoined ("C" + "alleen" — the a/o misread inside the word is the LLM
        // pass's job), hyphenated paragraph split joined, "/" debris dropped.
        assert!(
            body.starts_with("Calleen Szot is one of the most successful writers in the paid"),
            "body was: {body}"
        );
        // The page number is gone and the comma-split sentence merged across it.
        assert!(body.contains("good reason, she recently authored a program."));
        // A genuinely new paragraph (starts uppercase after a finished sentence) survives.
        assert!(body.contains("\n\nA new paragraph starts here."));
    }

    #[test]
    fn dehyphenates_only_lowercase_continuations() {
        let raw = "Some text about a well-\nknown thing and a Nordic-\nTrac machine.\n";
        let body = clean_chapter(raw, &[]).body;
        assert!(body.contains("well-known") == false); // joined without hyphen
        assert!(body.contains("wellknown"));
        assert!(body.contains("Nordic- Trac")); // uppercase continuation: hyphen kept
    }

    #[test]
    fn keeps_paragraph_boundaries_and_collapses_spaces() {
        let raw = "First  paragraph\nwraps  here.\n\nSecond paragraph.\n";
        let body = clean_chapter(raw, &[]).body;
        assert_eq!(body, "First paragraph wraps here.\n\nSecond paragraph.");
    }

    #[test]
    fn leaves_words_a_and_i_alone_without_a_glyph() {
        let raw = "A friend of mine said this.\n";
        assert!(clean_chapter(raw, &[]).body.starts_with("A friend"));
        let raw = "I saw it happen.\n";
        assert!(clean_chapter(raw, &[]).body.starts_with("I saw"));
        // But with the stray glyph the join fires even for A/I.
        let raw = "A) nyone could see it.\n";
        assert!(clean_chapter(raw, &[]).body.starts_with("Anyone could"));
    }

    #[test]
    fn strips_running_head_lines_and_merges_across_them() {
        // A page boundary mid-sentence: page number + the book-title running head
        // between two halves of one sentence (observed shape, e.g. 25.txt line 28).
        let raw = "The first half of a sentence continues,\n\n17\n\nYes!\n\nand this is the rest.\n";
        let body = clean_chapter(raw, &["Yes!"]).body;
        assert_eq!(
            body,
            "The first half of a sentence continues, and this is the rest."
        );
        // Without the strip pattern the head survives as its own junk paragraph.
        let body = clean_chapter(raw, &[]).body;
        assert!(body.contains("Yes!"));
    }

    #[test]
    fn no_title_claimed_when_no_opening_question() {
        let raw = "Just a body paragraph without a question.\n\nAnother.\n";
        let cleaned = clean_chapter(raw, &[]);
        assert_eq!(cleaned.title, None);
        assert!(cleaned.body.starts_with("Just a body"));
    }

    #[test]
    fn chunking_packs_paragraphs_without_splitting() {
        let body = "aaaa\n\nbbbb\n\ncccc\n\ndddd";
        let chunks = chunk_paragraphs(body, 11);
        // 4+2+4=10 <= 11 fits two paragraphs per chunk.
        assert_eq!(chunks, vec!["aaaa\n\nbbbb".to_string(), "cccc\n\ndddd".to_string()]);
        // An over-long paragraph becomes its own chunk.
        let chunks = chunk_paragraphs("short\n\nreallyreallylongparagraph", 10);
        assert_eq!(chunks.len(), 2);
    }
}
