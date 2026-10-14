//! Char-level refinement of prose `Replaced` pairs.
//!
//! The structural diff is token-granular, so a one-word edit inside a string
//! atom or a line-diff fallback run marks the whole token or line. Refinement
//! pairs the lines of each prose pair, runs a second, char-level diff over each
//! line pair that differs, and records the sub-ranges whose characters actually
//! differ in [`DiffChange::refined_spans`](super::DiffChange::refined_spans), so
//! display narrows the mark.
//!
//! The lines pair by [`line_diff::align_lines`], not by position. A line
//! inserted inside a long string, comment, or YAML block scalar shifts every
//! line after it, so position pairing compares each of those lines with its old
//! neighbor and marks them all. Aligned, they meet their old selves and mark
//! nothing. The refine returns those equal lines, so a consumer shows them as
//! context.
//!
//! Only prose pairs are refined. A code atom carries its own token boundary, so
//! marking part of one reads as a typo rather than as an edit. An honest
//! `remove` -> `keep` rename narrowed to `remov` says the `e` survived, which
//! is not what happened. Prose has no such boundary, which is the whole reason
//! the narrowing is worth doing there.
//!
//! It only writes `refined_spans`. It never alters byte ranges, kinds, pairing,
//! or hunk extents.

use super::{line_diff, ChangeKind, DiffChange, MatchedPair, Side};
use imara_diff::{Algorithm, Diff, InternedInput, TokenSource};
use std::{collections::HashMap, ops::Range, str::Chars};

/// A line longer than this is refined as a whole rather than char-diffed, since
/// the char diff can be quadratic in the worst case.
const MAX_REFINE_LINE_BYTES: usize = 1024;

/// A pair with more differing line pairs than this marks each of them whole.
///
/// Every char-diffed line costs an interning pass and a diff of its own, so a
/// block rewritten wholesale runs one per line only to arrive at spans that
/// cover its lines anyway. Past the cap, the equal lines still mark nothing.
///
/// The count is what tells the two cases apart. Refinement shows a reader the
/// word that changed. That question fits an edit a person makes by hand, and
/// not a rewrite where the answer is every line.
const MAX_CHAR_DIFFED_LINES: usize = 32;

/// Changed char regions closer than this many unchanged chars merge into one,
/// so a scattered edit reads as one span rather than confetti.
const MERGE_GAP: usize = 3;

/// The spans one refined pair yields, with the lhs side first.
type PairSpans = (Vec<Range<usize>>, Vec<Range<usize>>);

/// One line of a side's changed text.
struct PairLine<'a> {
    /// The line's text, without its newline.
    text: &'a str,
    /// The absolute byte offset of the line's first char.
    start: usize,
    /// Whether a newline inside the changed text ends the line.
    terminated: bool,
}

impl PairLine<'_> {
    fn text_range(&self) -> Range<usize> {
        self.start..self.start + self.text.len()
    }

    /// The line's range with its newline, when the changed text holds one.
    ///
    /// A line that marks whole marks its newline too. So a blank line still
    /// marks its row, and the whole lines of a run touch.
    fn whole(&self) -> Range<usize> {
        self.start..self.start + self.text.len() + usize::from(self.terminated)
    }
}

/// Populate [`DiffChange::refined_spans`](super::DiffChange::refined_spans) for
/// every prose `Replaced` pair, narrowing both sides to the chars that actually
/// differ, and return the line pairs the refine found equal.
///
/// A pair is prose only when both of its sides are, since one code atom brings
/// a token boundary back to the run. Every other pair keeps `None`, so it marks
/// whole.
///
/// Both sides of a prose pair get `Some`. An equal line pair contributes
/// nothing, and a differing pair contributes its changed chars. A line with no
/// counterpart contributes its whole range. So does each line of a differing
/// pair past `MAX_CHAR_DIFFED_LINES` differing pairs, or longer than
/// `MAX_REFINE_LINE_BYTES`. A side whose chars all survive gets `Some` and
/// empty, such as the old side of an insertion.
///
/// The returned pairs are the equal lines in lhs order, with a blank line as two
/// empty ranges at its start. A line counts only when nothing about it changed.
/// So a file's last line that gained or lost its newline stays out, and so does
/// every line of a pair whose lines all match, since what changed there lies
/// outside the text the refine compares.
pub fn refine_replaced_pairs(
    changes: &mut [DiffChange],
    lhs_text: &str,
    rhs_text: &str,
) -> Vec<MatchedPair> {
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

    let mut unchanged = Vec::new();
    for (lhs_idx, rhs_idx) in pairs.into_values() {
        let (Some(lhs_idx), Some(rhs_idx)) = (lhs_idx, rhs_idx) else {
            continue;
        };
        if !(changes[lhs_idx].prose && changes[rhs_idx].prose) {
            continue;
        }
        let lhs_range = changes[lhs_idx].byte_range.clone();
        let rhs_range = changes[rhs_idx].byte_range.clone();
        let (lhs_spans, rhs_spans) = refine_pair(
            &lhs_text[lhs_range.clone()],
            lhs_range.start,
            &rhs_text[rhs_range.clone()],
            rhs_range.start,
            &mut unchanged,
        );
        changes[lhs_idx].refined_spans = Some(lhs_spans);
        changes[rhs_idx].refined_spans = Some(rhs_spans);
    }

    // The refine compares a line's text alone. A line that runs to the end of a
    // file with no final newline still differs from the same text that a
    // newline ends.
    unchanged.retain(|pair| (pair.lhs.end == lhs_text.len()) == (pair.rhs.end == rhs_text.len()));
    unchanged.sort_by_key(|pair| pair.lhs.start);
    unchanged
}

/// Refine one `Replaced` pair. `lhs`/`rhs` are the two sides' changed text;
/// `lhs_base`/`rhs_base` are their byte offsets in the full inputs, so the
/// returned spans are absolute.
///
/// Pushes the pair's equal line pairs onto `unchanged`, unless every line of the
/// pair matches.
fn refine_pair(
    lhs: &str,
    lhs_base: usize,
    rhs: &str,
    rhs_base: usize,
    unchanged: &mut Vec<MatchedPair>,
) -> PairSpans {
    let lhs_lines = lines_with_offsets(lhs, lhs_base);
    let rhs_lines = lines_with_offsets(rhs, rhs_base);
    let aligned = {
        let lhs_texts: Vec<&str> = lhs_lines.iter().map(|line| line.text).collect();
        let rhs_texts: Vec<&str> = rhs_lines.iter().map(|line| line.text).collect();
        line_diff::align_lines(&lhs_texts, &rhs_texts)
    };

    let differing = aligned
        .iter()
        .filter(|pair| match **pair {
            (Some(l), Some(r)) => lhs_lines[l].text != rhs_lines[r].text,
            _ => false,
        })
        .count();
    let one_sided = aligned
        .iter()
        .any(|pair| pair.0.is_none() || pair.1.is_none());
    let some_line_changed = differing > 0 || one_sided;
    let char_diffs = differing <= MAX_CHAR_DIFFED_LINES;

    let mut lhs_spans = Vec::new();
    let mut rhs_spans = Vec::new();
    let mut offsets = Vec::new();
    for pair in aligned {
        match pair {
            (Some(l), Some(r)) => {
                let (l, r) = (&lhs_lines[l], &rhs_lines[r]);
                if l.text == r.text {
                    if some_line_changed {
                        unchanged.push(MatchedPair {
                            lhs: l.text_range(),
                            rhs: r.text_range(),
                        });
                    }
                    continue;
                }
                let short =
                    l.text.len() <= MAX_REFINE_LINE_BYTES && r.text.len() <= MAX_REFINE_LINE_BYTES;
                if char_diffs && short {
                    let (l_local, r_local) = char_diff(l.text, r.text, &mut offsets);
                    extend_absolute(&mut lhs_spans, &l_local, l.start);
                    extend_absolute(&mut rhs_spans, &r_local, r.start);
                } else {
                    push_whole(&mut lhs_spans, l.whole());
                    push_whole(&mut rhs_spans, r.whole());
                }
            },
            (Some(l), None) => push_whole(&mut lhs_spans, lhs_lines[l].whole()),
            (None, Some(r)) => push_whole(&mut rhs_spans, rhs_lines[r].whole()),
            (None, None) => {},
        }
    }
    (lhs_spans, rhs_spans)
}

/// Char-diff two single lines, returning the changed char byte ranges
/// (line-local) on each side, with near-adjacent regions merged.
///
/// `offsets` is scratch the char-to-byte mapping builds in, held by the caller
/// so a pair's lines share one buffer rather than each taking two of their own.
fn char_diff(
    lhs: &str,
    rhs: &str,
    offsets: &mut Vec<usize>,
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    if lhs == rhs {
        return (Vec::new(), Vec::new());
    }
    // A char carries no indentation, so no slider heuristic places a region.
    let input = InternedInput::new(CharTokens(lhs), CharTokens(rhs));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let (mut lhs_regions, mut rhs_regions) = (Vec::new(), Vec::new());
    for hunk in diff.hunks() {
        if !hunk.before.is_empty() {
            lhs_regions.push(hunk.before.start as usize..hunk.before.end as usize);
        }
        if !hunk.after.is_empty() {
            rhs_regions.push(hunk.after.start as usize..hunk.after.end as usize);
        }
    }
    (
        char_regions_to_bytes(&merge_regions(lhs_regions), lhs, offsets),
        char_regions_to_bytes(&merge_regions(rhs_regions), rhs, offsets),
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
///
/// `offsets` is refilled here rather than allocated, so the two calls a line
/// makes and every line of a pair share one buffer.
fn char_regions_to_bytes(
    regions: &[Range<usize>],
    line: &str,
    offsets: &mut Vec<usize>,
) -> Vec<Range<usize>> {
    offsets.clear();
    offsets.extend(line.char_indices().map(|(i, _)| i));
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

/// Push the range of a line that marks whole, joined to the span before it when
/// the two touch.
///
/// A whole line carries its newline, so the next whole line starts where it
/// ends, and a run of them costs one span rather than one per line.
fn push_whole(spans: &mut Vec<Range<usize>>, range: Range<usize>) {
    match spans.last_mut() {
        Some(last) if last.end == range.start => last.end = range.end,
        _ => spans.push(range),
    }
}

/// Split `text` into its lines, each with its absolute byte start, which is
/// `base` plus its offset in `text`.
///
/// A trailing newline ends the last line and opens no empty one. The bytes
/// after it start the next line of the file, which the change does not hold.
fn lines_with_offsets(text: &str, base: usize) -> Vec<PairLine<'_>> {
    let mut start = base;
    text.split_inclusive('\n')
        .map(|piece| {
            let text = piece.strip_suffix('\n');
            let line = PairLine {
                text: text.unwrap_or(piece),
                start,
                terminated: text.is_some(),
            };
            start += piece.len();
            line
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::{refine_pair, MatchedPair, PairSpans, MAX_CHAR_DIFFED_LINES};
    use std::ops::Range;

    /// Refine one pair at offset zero, returning its spans and its unchanged
    /// lines.
    fn refine(lhs: &str, rhs: &str) -> (PairSpans, Vec<MatchedPair>) {
        let mut unchanged = Vec::new();
        let spans = refine_pair(lhs, 0, rhs, 0, &mut unchanged);
        (spans, unchanged)
    }

    /// The text each span covers.
    fn texts<'a>(text: &'a str, spans: &[Range<usize>]) -> Vec<&'a str> {
        spans.iter().map(|span| &text[span.clone()]).collect()
    }

    /// The 0-based line `range` starts on, with the text it covers.
    fn located<'a>(text: &'a str, range: &Range<usize>) -> (usize, &'a str) {
        (
            text[..range.start].matches('\n').count(),
            &text[range.clone()],
        )
    }

    /// Up to the cap, each differing line narrows to the word that changed. One
    /// differing line past it, each marks whole. Both sides of the cap, since
    /// one tested only far from its edge is satisfied by the wrong comparison.
    #[test]
    fn a_rewrite_past_the_char_diff_cap_marks_its_lines_whole() {
        let block = |lines: usize, word: &str| -> String {
            (0..lines).map(|i| format!("call{i}({word})\n")).collect()
        };

        let (lhs, rhs) = (
            block(MAX_CHAR_DIFFED_LINES, "alpha"),
            block(MAX_CHAR_DIFFED_LINES, "OMEGA"),
        );
        let ((_, rhs_spans), _) = refine(&lhs, &rhs);
        assert_eq!(
            texts(&rhs, &rhs_spans),
            vec!["OMEGA"; MAX_CHAR_DIFFED_LINES],
            "at the cap, each line narrows to the word that changed",
        );

        let lines = MAX_CHAR_DIFFED_LINES + 1;
        let (lhs, rhs) = (block(lines, "alpha"), block(lines, "OMEGA"));
        let ((lhs_spans, rhs_spans), unchanged) = refine(&lhs, &rhs);
        assert_eq!(
            (texts(&lhs, &lhs_spans), texts(&rhs, &rhs_spans), unchanged),
            (vec![lhs.as_str()], vec![rhs.as_str()], Vec::new()),
            "past the cap, the lines mark whole and join into one span per side",
        );
    }

    /// Lines inserted in the middle of a tall pair mark alone. The lines after
    /// them meet their old selves, so they mark nothing and pair as unchanged.
    #[test]
    fn an_insertion_inside_a_tall_prose_pair_marks_the_inserted_lines() {
        let old: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
        let new: Vec<String> = {
            let mut new = old.clone();
            new.splice(20..20, (0..3).map(|i| format!("added {i}")));
            new
        };
        let (lhs, rhs) = (old.join("\n"), new.join("\n"));
        let ((lhs_spans, rhs_spans), unchanged) = refine(&lhs, &rhs);

        assert_eq!(
            (lhs_spans, texts(&rhs, &rhs_spans)),
            (Vec::new(), vec!["added 0\nadded 1\nadded 2\n"]),
            "the inserted lines mark as one run, and the old side marks nothing",
        );
        assert_eq!(
            unchanged
                .iter()
                .map(|pair| (located(&lhs, &pair.lhs), located(&rhs, &pair.rhs)))
                .collect::<Vec<_>>(),
            (0..40)
                .map(|i| {
                    let moved = if i < 20 { i } else { i + 3 };
                    ((i, old[i].as_str()), (moved, old[i].as_str()))
                })
                .collect::<Vec<_>>(),
            "each old line pairs with itself, past the insertion too",
        );
    }

    /// A reindented line pairs with its old self, not with the line inserted
    /// above it, so it marks only the indent it gained.
    #[test]
    fn a_reindented_line_pairs_with_its_old_self() {
        let (lhs, rhs) = ("first\n  keep\nlast", "first\nadded\n    keep\nlast");
        let ((lhs_spans, rhs_spans), unchanged) = refine(lhs, rhs);
        assert_eq!(
            (
                texts(lhs, &lhs_spans),
                texts(rhs, &rhs_spans),
                unchanged
                    .iter()
                    .map(|pair| (located(lhs, &pair.lhs), located(rhs, &pair.rhs)))
                    .collect::<Vec<_>>(),
            ),
            (
                vec![],
                vec!["added\n", "  "],
                vec![((0, "first"), (0, "first")), ((2, "last"), (3, "last"))],
            ),
            "the new line marks whole, and the moved line marks only its two new spaces",
        );
    }

    /// A blank line has no char of its own. With no counterpart it marks its
    /// newline, so its row still reads as changed. Beside a blank counterpart it
    /// pairs as two empty ranges.
    #[test]
    fn a_blank_line_marks_its_newline_or_pairs_empty() {
        let pair = |lhs: Range<usize>, rhs: Range<usize>| MatchedPair { lhs, rhs };

        let lhs = "a\n\nb";
        let ((lhs_spans, rhs_spans), unchanged) = refine(lhs, "a\nb");
        assert_eq!(
            (
                lhs_spans
                    .iter()
                    .map(|span| located(lhs, span))
                    .collect::<Vec<_>>(),
                rhs_spans,
                unchanged,
            ),
            (
                vec![(1, "\n")],
                Vec::new(),
                vec![pair(0..1, 0..1), pair(3..4, 2..3)]
            ),
            "the removed blank line marks its newline",
        );

        let (lhs, rhs) = ("x\n\ny", "X\n\ny");
        let ((lhs_spans, rhs_spans), unchanged) = refine(lhs, rhs);
        assert_eq!(
            (texts(lhs, &lhs_spans), texts(rhs, &rhs_spans), unchanged),
            (
                vec!["x"],
                vec!["X"],
                vec![pair(2..2, 2..2), pair(3..4, 3..4)]
            ),
            "a blank line beside a blank counterpart pairs as two empty ranges",
        );
    }

    #[test]
    fn refines_an_inserted_word_to_just_that_word() {
        let lhs = "foo(\"hello world\")";
        let rhs = "foo(\"hello brave world\")";
        let ((_, rhs_spans), _) = refine(lhs, rhs);
        assert_eq!(rhs_spans.len(), 1, "one contiguous inserted region");
        assert_eq!(&rhs[rhs_spans[0].clone()], "brave ");
    }

    #[test]
    fn refines_a_replaced_word_on_both_sides() {
        // "alpha" and "OMEGA" share no chars, so the whole word narrows on each
        // side (a shared prefix/suffix would trim further, as other tests show).
        let lhs = "let x = alpha;";
        let rhs = "let x = OMEGA;";
        let ((lhs_spans, rhs_spans), _) = refine(lhs, rhs);
        assert_eq!(&lhs[lhs_spans[0].clone()], "alpha");
        assert_eq!(&rhs[rhs_spans[0].clone()], "OMEGA");
    }

    #[test]
    fn fully_rewritten_line_keeps_the_whole_line() {
        let (lhs, rhs) = ("alpha", "ZZZZZ");
        let ((lhs_spans, rhs_spans), _) = refine(lhs, rhs);
        assert_eq!(lhs_spans.len(), 1);
        assert_eq!(&lhs[lhs_spans[0].clone()], "alpha");
        assert_eq!(rhs_spans.len(), 1);
        assert_eq!(&rhs[rhs_spans[0].clone()], "ZZZZZ");
    }

    #[test]
    fn refines_each_differing_line_of_a_multiline_pair() {
        let lhs = "old1\nold2";
        let rhs = "new1\nnew2";
        let ((_, rhs_spans), _) = refine(lhs, rhs);
        // "old" -> "new" on line 1 (0..3) and line 2 (5..8, after "new1\n").
        assert_eq!(rhs_spans, [0..3, 5..8]);
    }

    #[test]
    fn absolute_offsets_add_the_base() {
        let rhs = "same\nhello brave world";
        let mut unchanged = Vec::new();
        let (_, rhs_spans) = refine_pair("same\nhello world", 100, rhs, 200, &mut unchanged);
        assert_eq!(rhs_spans.len(), 1);
        assert_eq!(rhs_spans[0].start, 200 + 5 + 6);
        assert_eq!(
            unchanged,
            [MatchedPair {
                lhs: 100..104,
                rhs: 200..204
            }],
            "an unchanged line is absolute too",
        );
    }

    #[test]
    fn a_pair_whose_lines_match_refines_to_empty_spans() {
        assert_eq!(
            refine("   -p viewership_http", "   -p viewership_http"),
            ((Vec::new(), Vec::new()), Vec::new()),
            concat!(
                "lines that match differ only in their terminators, so nothing inside them ",
                "marks, and none of them reads as unchanged",
            ),
        );
    }
}
