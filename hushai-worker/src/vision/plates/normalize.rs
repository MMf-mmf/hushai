//! Plate-string normalization, confusable folding, temporal voting, and quality gating — the pure
//! (model-free) core of the ALPR lane, mirroring `face_embed`'s quality-gate role. Heavily unit
//! tested because OCR noise correctness lives here.

/// One OCR read of a plate: the raw string + per-character confidences (0..1) + their mean.
#[derive(Debug, Clone)]
pub struct PlateRead {
    pub text: String,
    pub char_confidences: Vec<f32>,
    pub mean_conf: f32,
}

impl PlateRead {
    pub fn new(text: String, char_confidences: Vec<f32>) -> Self {
        let mean_conf = if char_confidences.is_empty() {
            0.0
        } else {
            char_confidences.iter().sum::<f32>() / char_confidences.len() as f32
        };
        Self {
            text,
            char_confidences,
            mean_conf,
        }
    }
}

/// Read quality, mirroring `face_embed::FaceQuality`. Only `Mint` may create a NEW catalog plate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlateQuality {
    Mint,
    AttachOnly,
    Reject,
}

/// The gates a plate read must clear (from config).
#[derive(Debug, Clone, Copy)]
pub struct PlateGates {
    pub min_det_score: f32,
    pub min_ocr_conf: f32,
    pub mint_min_ocr_conf: f32,
    pub min_px: f32,
    pub min_len: usize,
}

/// Uppercase and strip everything but `[A-Z0-9]` — the canonical display form.
pub fn normalize(raw: &str) -> String {
    raw.chars()
        .filter_map(|c| {
            let u = c.to_ascii_uppercase();
            if u.is_ascii_alphanumeric() {
                Some(u)
            } else {
                None
            }
        })
        .collect()
}

/// Fold visually-confusable characters to a single canonical form for MATCHING (not display). The
/// default map is region-agnostic and biased toward digits where a glyph is ambiguous
/// (O/Q→0, I/L→1, B→8, S→5, Z→2, G→6, D→0). Two reads that disagree only on confusables collapse to
/// the same key, so a plate doesn't split into duplicates.
pub fn fold_confusables(normalized: &str) -> String {
    normalized
        .chars()
        .map(|c| match c {
            'O' | 'Q' | 'D' => '0',
            'I' | 'L' => '1',
            'B' => '8',
            'S' => '5',
            'Z' => '2',
            'G' => '6',
            _ => c,
        })
        .collect()
}

/// Levenshtein edit distance between two ASCII strings (small strings; simple DP).
pub fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Per-character-position, confidence-weighted majority vote across reads of one plate (across the
/// frames of a segment). Reads are first bucketed by length; the modal length (weighted by mean
/// confidence) wins, then each position takes the highest summed-confidence character. Returns the
/// canonical read with aggregated per-position confidence. Mirrors the speaker rolling-window idea.
pub fn vote(reads: &[PlateRead]) -> Option<PlateRead> {
    let reads: Vec<&PlateRead> = reads.iter().filter(|r| !r.text.is_empty()).collect();
    if reads.is_empty() {
        return None;
    }
    // Choose the dominant length (sum of mean confidences per length).
    let mut len_weight: std::collections::HashMap<usize, f32> = std::collections::HashMap::new();
    for r in &reads {
        *len_weight.entry(r.text.chars().count()).or_default() += r.mean_conf.max(1e-3);
    }
    let target_len = len_weight
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(l, _)| *l)?;
    let chosen: Vec<&PlateRead> = reads
        .into_iter()
        .filter(|r| r.text.chars().count() == target_len)
        .collect();

    let mut out = String::with_capacity(target_len);
    let mut confs = Vec::with_capacity(target_len);
    for pos in 0..target_len {
        let mut score: std::collections::HashMap<char, f32> = std::collections::HashMap::new();
        for r in &chosen {
            let ch = r.text.chars().nth(pos).unwrap();
            let conf = r.char_confidences.get(pos).copied().unwrap_or(r.mean_conf);
            *score.entry(ch).or_default() += conf.max(1e-3);
        }
        let total: f32 = score.values().sum();
        let (best_ch, best_w) = score
            .into_iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;
        out.push(best_ch);
        confs.push(if total > 0.0 { best_w / total } else { 0.0 });
    }
    Some(PlateRead::new(out, confs))
}

/// Classify a plate read. `Mint` needs comfortable headroom (confident detector + high OCR
/// confidence + large enough + long enough) so only trustworthy reads create a new catalog plate.
pub fn assess_quality(
    det_score: f32,
    mean_conf: f32,
    min_side_px: f32,
    len: usize,
    g: &PlateGates,
) -> PlateQuality {
    if det_score < g.min_det_score
        || mean_conf < g.min_ocr_conf
        || min_side_px < g.min_px
        || len < g.min_len
    {
        return PlateQuality::Reject;
    }
    if mean_conf >= g.mint_min_ocr_conf && len >= g.min_len {
        PlateQuality::Mint
    } else {
        PlateQuality::AttachOnly
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_and_uppercases() {
        assert_eq!(normalize(" ab-12 3c "), "AB123C");
        assert_eq!(normalize("7Ω-xyz!"), "7XYZ"); // non-ascii (Ω), punctuation + spaces dropped
    }

    #[test]
    fn confusable_fold_collapses_variants() {
        assert_eq!(fold_confusables("OIBSZG"), "018526");
        // "ABC0O" and "ABCOO" fold to the same key.
        assert_eq!(fold_confusables(&normalize("ABCOO")), fold_confusables(&normalize("ABC00")));
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("ABC123", "ABC123"), 0);
        assert_eq!(edit_distance("ABC123", "ABC128"), 1);
        assert_eq!(edit_distance("ABC", "ABCD"), 1);
    }

    #[test]
    fn vote_picks_per_position_majority() {
        // Three reads, position 3 disagrees (8 vs 8 vs B); high-conf 8s win.
        let reads = vec![
            PlateRead::new("ABC8".into(), vec![0.9, 0.9, 0.9, 0.9]),
            PlateRead::new("ABC8".into(), vec![0.9, 0.9, 0.9, 0.8]),
            PlateRead::new("ABCB".into(), vec![0.9, 0.9, 0.9, 0.4]),
        ];
        let v = vote(&reads).unwrap();
        assert_eq!(v.text, "ABC8");
    }

    #[test]
    fn vote_prefers_dominant_length() {
        let reads = vec![
            PlateRead::new("ABC123".into(), vec![0.9; 6]),
            PlateRead::new("ABC123".into(), vec![0.9; 6]),
            PlateRead::new("ABC12".into(), vec![0.5; 5]), // shorter, lower conf → loses
        ];
        let v = vote(&reads).unwrap();
        assert_eq!(v.text, "ABC123");
    }

    #[test]
    fn quality_gates() {
        let g = PlateGates {
            min_det_score: 0.35,
            min_ocr_conf: 0.55,
            mint_min_ocr_conf: 0.8,
            min_px: 16.0,
            min_len: 4,
        };
        assert_eq!(assess_quality(0.2, 0.9, 50.0, 6, &g), PlateQuality::Reject); // low det
        assert_eq!(assess_quality(0.9, 0.4, 50.0, 6, &g), PlateQuality::Reject); // low ocr
        assert_eq!(assess_quality(0.9, 0.6, 50.0, 3, &g), PlateQuality::Reject); // too short
        assert_eq!(assess_quality(0.9, 0.6, 50.0, 6, &g), PlateQuality::AttachOnly);
        assert_eq!(assess_quality(0.9, 0.95, 50.0, 6, &g), PlateQuality::Mint);
    }
}
