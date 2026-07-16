//! Char-level refinement of `Replaced` pairs.
//!
//! The structural diff is token-granular, so a one-word edit inside a string
//! atom or a line-diff fallback run marks the whole token or line. Refinement
//! runs a second, char-level diff over each paired line and records the
//! sub-ranges whose characters actually differ in
//! [`DiffChange::refined_spans`](super::DiffChange::refined_spans), so display
//! can narrow the mark.
//!
//! It only writes `refined_spans`. It never alters byte ranges, kinds, pairing,
//! or hunk extents.

use super::{ChangeKind, DiffChange, Side};
use imara_diff::{
    intern::{InternedInput, TokenSource},
    Algorithm, Sink,
};
use std::{collections::HashMap, ops::Range, str::Chars};

/// A line longer than this is refined as a whole rather than char-diffed, since
/// the char diff can be quadratic in the worst case.
const MAX_REFINE_LINE_BYTES: usize = 1024;

/// Changed char regions closer than this many unchanged chars merge into one,
/// so a scattered edit reads as one span rather than confetti.
const MERGE_GAP: usize = 3;

/// Populate [`DiffChange::refined_spans`](super::DiffChange::refined_spans) for
/// every `Replaced` pair, narrowing both sides to the chars that actually
/// differ.
///
/// A pair whose two sides char-diff cleanly gets per-line refined spans. A line
/// that is a full rewrite, unpaired, or too long to char-diff contributes its
/// whole range, and a change whose refined spans come back empty falls back to
/// its whole `byte_range` at display time, so a mark never disappears.
pub fn refine_replaced_pairs(changes: &mut [DiffChange], lhs_text: &str, rhs_text: &str) {
    let mut pairs: HashMap<u32, (Option<usize>, Option<usize>)> = HashMap::new();
    for (i, change) in changes.iter().enumerate() {
        if change.kind != ChangeKind::Replaced {
            continue;
        }
        let Some(pair_id) = change.pair_id else {
            continue;
        };
        let slot = pairs.entry(pair_id).or_insert((None, None));
        match change.side {
            Side::Lhs => slot.0 = Some(i),
            Side::Rhs => slot.1 = Some(i),
        }
    }

    for (lhs_idx, rhs_idx) in pairs.into_values() {
        let (Some(lhs_idx), Some(rhs_idx)) = (lhs_idx, rhs_idx) else {
            continue;
        };
        let lhs_range = changes[lhs_idx].byte_range.clone();
        let rhs_range = changes[rhs_idx].byte_range.clone();
        let (lhs_spans, rhs_spans) = refine_pair(
            &lhs_text[lhs_range.clone()],
            lhs_range.start,
            &rhs_text[rhs_range.clone()],
            rhs_range.start,
        );
        changes[lhs_idx].refined_spans = lhs_spans;
        changes[rhs_idx].refined_spans = rhs_spans;
    }
}

/// Refine one `Replaced` pair. `lhs`/`rhs` are the two sides' changed text;
/// `lhs_base`/`rhs_base` are their byte offsets in the full inputs, so the
/// returned spans are absolute.
fn refine_pair(
    lhs: &str,
    lhs_base: usize,
    rhs: &str,
    rhs_base: usize,
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let lhs_lines = lines_with_offsets(lhs, lhs_base);
    let rhs_lines = lines_with_offsets(rhs, rhs_base);

    let mut lhs_spans = Vec::new();
    let mut rhs_spans = Vec::new();

    let paired = lhs_lines.len().min(rhs_lines.len());
    for i in 0..paired {
        let (l_text, l_start) = lhs_lines[i];
        let (r_text, r_start) = rhs_lines[i];
        if l_text.len() > MAX_REFINE_LINE_BYTES || r_text.len() > MAX_REFINE_LINE_BYTES {
            push_whole_line(&mut lhs_spans, l_text, l_start);
            push_whole_line(&mut rhs_spans, r_text, r_start);
            continue;
        }
        let (l_local, r_local) = char_diff(l_text, r_text);
        extend_absolute(&mut lhs_spans, &l_local, l_start);
        extend_absolute(&mut rhs_spans, &r_local, r_start);
    }
    // A line with no counterpart on the other side is wholly changed.
    for &(text, start) in &lhs_lines[paired..] {
        push_whole_line(&mut lhs_spans, text, start);
    }
    for &(text, start) in &rhs_lines[paired..] {
        push_whole_line(&mut rhs_spans, text, start);
    }
    (lhs_spans, rhs_spans)
}

/// Char-diff two single lines, returning the changed char byte ranges
/// (line-local) on each side, with near-adjacent regions merged.
fn char_diff(lhs: &str, rhs: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    if lhs == rhs {
        return (Vec::new(), Vec::new());
    }
    let input = InternedInput::new(CharTokens(lhs), CharTokens(rhs));
    let (lhs_regions, rhs_regions) = imara_diff::diff(
        Algorithm::Histogram,
        &input,
        CharRegionSink {
            lhs: Vec::new(),
            rhs: Vec::new(),
        },
    );
    (
        char_regions_to_bytes(&merge_regions(lhs_regions), lhs),
        char_regions_to_bytes(&merge_regions(rhs_regions), rhs),
    )
}

/// Merge char-index regions separated by fewer than [`MERGE_GAP`] unchanged
/// chars, so a word split into a few near edits reads as one span.
fn merge_regions(mut regions: Vec<Range<usize>>) -> Vec<Range<usize>> {
    regions.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for r in regions {
        match merged.last_mut() {
            Some(last) if r.start.saturating_sub(last.end) < MERGE_GAP => {
                last.end = last.end.max(r.end);
            },
            _ => merged.push(r),
        }
    }
    merged
}

/// Map char-index regions to line-local byte ranges. `line.char_indices()`
/// yields each char's byte offset. The appended `line.len()` closes the last
/// region.
fn char_regions_to_bytes(regions: &[Range<usize>], line: &str) -> Vec<Range<usize>> {
    let mut offsets: Vec<usize> = line.char_indices().map(|(i, _)| i).collect();
    offsets.push(line.len());
    regions
        .iter()
        .map(|r| offsets[r.start]..offsets[r.end])
        .collect()
}

fn extend_absolute(spans: &mut Vec<Range<usize>>, local: &[Range<usize>], base: usize) {
    for r in local {
        spans.push(base + r.start..base + r.end);
    }
}

fn push_whole_line(spans: &mut Vec<Range<usize>>, text: &str, start: usize) {
    if !text.is_empty() {
        spans.push(start..start + text.len());
    }
}

/// Split `text` into lines (newline excluded), each paired with its absolute
/// byte start (`base` + line-local offset).
fn lines_with_offsets(text: &str, base: usize) -> Vec<(&str, usize)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            out.push((&text[start..idx], base + start));
            start = idx + 1;
        }
    }
    out.push((&text[start..], base + start));
    out
}

/// A [`TokenSource`] whose tokens are the chars of a single line.
struct CharTokens<'a>(&'a str);

impl<'a> TokenSource for CharTokens<'a> {
    type Token = char;
    type Tokenizer = Chars<'a>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.chars()
    }

    fn estimate_tokens(&self) -> u32 {
        self.0.len() as u32
    }
}

/// Collects imara-diff's changed char-index ranges, `before` on the lhs and
/// `after` on the rhs.
struct CharRegionSink {
    lhs: Vec<Range<usize>>,
    rhs: Vec<Range<usize>>,
}

impl Sink for CharRegionSink {
    type Out = (Vec<Range<usize>>, Vec<Range<usize>>);

    fn process_change(&mut self, before: Range<u32>, after: Range<u32>) {
        if before.end > before.start {
            self.lhs.push(before.start as usize..before.end as usize);
        }
        if after.end > after.start {
            self.rhs.push(after.start as usize..after.end as usize);
        }
    }

    fn finish(self) -> Self::Out {
        (self.lhs, self.rhs)
    }
}

#[cfg(test)]
mod tests {
    use super::refine_pair;

    #[test]
    fn refines_an_inserted_word_to_just_that_word() {
        let lhs = "foo(\"hello world\")";
        let rhs = "foo(\"hello brave world\")";
        let (_, rhs_spans) = refine_pair(lhs, 0, rhs, 0);
        assert_eq!(rhs_spans.len(), 1, "one contiguous inserted region");
        assert_eq!(&rhs[rhs_spans[0].clone()], "brave ");
    }

    #[test]
    fn refines_a_replaced_word_on_both_sides() {
        // "alpha" and "OMEGA" share no chars, so the whole word narrows on each
        // side (a shared prefix/suffix would trim further, as other tests show).
        let lhs = "let x = alpha;";
        let rhs = "let x = OMEGA;";
        let (lhs_spans, rhs_spans) = refine_pair(lhs, 0, rhs, 0);
        assert_eq!(&lhs[lhs_spans[0].clone()], "alpha");
        assert_eq!(&rhs[rhs_spans[0].clone()], "OMEGA");
    }

    #[test]
    fn fully_rewritten_line_keeps_the_whole_line() {
        let (lhs, rhs) = ("alpha", "ZZZZZ");
        let (lhs_spans, rhs_spans) = refine_pair(lhs, 0, rhs, 0);
        assert_eq!(lhs_spans.len(), 1);
        assert_eq!(&lhs[lhs_spans[0].clone()], "alpha");
        assert_eq!(rhs_spans.len(), 1);
        assert_eq!(&rhs[rhs_spans[0].clone()], "ZZZZZ");
    }

    #[test]
    fn refines_a_multiline_pair_per_zipped_line() {
        let lhs = "old1\nold2";
        let rhs = "new1\nnew2";
        let (_, rhs_spans) = refine_pair(lhs, 0, rhs, 0);
        // "old" -> "new" on line 1 (0..3) and line 2 (5..8, after "new1\n").
        assert_eq!(rhs_spans, [0..3, 5..8]);
    }

    #[test]
    fn absolute_offsets_add_the_base() {
        let rhs = "hello brave world";
        let (_, rhs_spans) = refine_pair("hello world", 100, rhs, 200);
        assert_eq!(rhs_spans.len(), 1);
        assert_eq!(rhs_spans[0].start, 200 + 6);
    }
}
