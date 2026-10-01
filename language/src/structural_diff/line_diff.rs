//! Line-level diff fallback used by [`super::diff`], and the safety net
//! for inputs that fail to parse or exceed the structural-diff graph cap.
//!
//! Interns each input's lines into token streams and runs imara-diff's
//! histogram algorithm over them, then maps each change region to Lhs/Rhs
//! [`DiffChange`]s. A region that touches both sides is tagged
//! [`super::ChangeKind::Replaced`] (one side replaces the other); a
//! one-sided region is [`super::ChangeKind::Novel`].
//!
//! The histogram pass runs in `O(n + m)` memory over the line count,
//! unlike a subsequence DP table that is quadratic in space, so it stays
//! cheap even on the large or unparseable files this fallback exists for.

use super::{ChangeKind, DiffChange, DiffResult, Side};
use imara_diff::{
    intern::{InternedInput, TokenSource},
    sources, Algorithm, Sink,
};
use std::{iter::Map, ops::Range, slice::Iter};

/// Compute a line-level diff between `lhs` and `rhs`. The returned
/// changes carry rope byte ranges (relative to each input) so they can
/// be threaded directly into [`crate`]'s `DiffMap` scaffolding.
pub fn diff_lines(lhs: &str, rhs: &str) -> DiffResult {
    let lhs_lines = lines_with_offsets(lhs);
    let rhs_lines = lines_with_offsets(rhs);

    let input = InternedInput::new(
        sources::lines_with_terminator(lhs),
        sources::lines_with_terminator(rhs),
    );
    let mut changes = imara_diff::diff(
        Algorithm::Histogram,
        &input,
        ChangeSink {
            lhs_lines: &lhs_lines,
            rhs_lines: &rhs_lines,
            changes: Vec::new(),
            next_pair_id: 0,
        },
    );
    super::refine::refine_replaced_pairs(&mut changes, lhs, rhs);

    DiffResult {
        changes,
        fell_back_to_line_diff: true,
    }
}

/// Pairs the lines of one changed region, matching lines that differ only in
/// whitespace.
///
/// A reindent rewrites every line it moves, so a line diff sees the whole
/// region changed and does not say which old line each new line came from.
/// With their whitespace collapsed, the moved lines match their old selves.
/// The lines between two matches pair by position, and the longer side's extra
/// lines pair with nothing.
///
/// Each pair is `(lhs index, rhs index)`. The pairs hold every line of both
/// sides once, in order on both sides.
pub fn align_lines(lhs: &[&str], rhs: &[&str]) -> Vec<(Option<usize>, Option<usize>)> {
    let lhs_keys: Vec<String> = lhs.iter().map(|line| collapse_whitespace(line)).collect();
    let rhs_keys: Vec<String> = rhs.iter().map(|line| collapse_whitespace(line)).collect();
    let input = InternedInput::new(KeyLines(&lhs_keys), KeyLines(&rhs_keys));

    imara_diff::diff(
        Algorithm::Histogram,
        &input,
        PairSink {
            pairs: Vec::with_capacity(lhs.len().max(rhs.len())),
            lhs_at: 0,
            rhs_at: 0,
            lhs_len: lhs.len(),
        },
    )
}

/// The `(start, end)` byte offsets of one line in the source string.
/// `end` excludes the trailing newline so adjacent lines don't share a
/// boundary byte. imara-diff owns line comparison, so only the offsets
/// are retained here, for mapping change regions back to byte ranges.
struct LineRecord {
    start: usize,
    end: usize,
}

fn lines_with_offsets(text: &str) -> Vec<LineRecord> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            out.push(LineRecord { start, end: idx });
            start = idx + 1;
        }
    }
    if start <= text.len() {
        out.push(LineRecord {
            start,
            end: text.len(),
        });
    }
    out
}

/// Accumulates imara-diff's change regions into [`DiffChange`]s.
///
/// Each `process_change` hunk maps to the same output the previous
/// subsequence walk produced. A region touching both sides becomes a
/// [`ChangeKind::Replaced`] pair sharing a fresh `pair_id`, and a
/// one-sided region becomes a [`ChangeKind::Novel`] run.
struct ChangeSink<'a> {
    lhs_lines: &'a [LineRecord],
    rhs_lines: &'a [LineRecord],
    changes: Vec<DiffChange>,
    next_pair_id: u32,
}

impl Sink for ChangeSink<'_> {
    type Out = Vec<DiffChange>;

    fn process_change(&mut self, before: Range<u32>, after: Range<u32>) {
        let lhs_start = before.start as usize;
        let lhs_count = (before.end - before.start) as usize;
        let rhs_start = after.start as usize;
        let rhs_count = (after.end - after.start) as usize;

        let kind = if lhs_count > 0 && rhs_count > 0 {
            ChangeKind::Replaced
        } else {
            ChangeKind::Novel
        };
        let pair_id = if kind == ChangeKind::Replaced {
            let id = self.next_pair_id;
            self.next_pair_id += 1;
            Some(id)
        } else {
            None
        };

        if lhs_count > 0 {
            // A pure deletion anchors to the rhs line it sat before, so the
            // renderer can place the removed run.
            let deletion_rhs_anchor = if kind == ChangeKind::Novel {
                Some(after.start)
            } else {
                None
            };
            self.changes.push(DiffChange {
                side: Side::Lhs,
                byte_range: range_for_lines(self.lhs_lines, lhs_start, lhs_count),
                kind,
                move_metadata: None,
                pair_id,
                deletion_rhs_anchor,
                refined_spans: Vec::new(),
                // The line pass runs where no grammar parsed, so nothing in
                // the file carries a token boundary to mark an edit against.
                prose: true,
            });
        }
        if rhs_count > 0 {
            self.changes.push(DiffChange {
                side: Side::Rhs,
                byte_range: range_for_lines(self.rhs_lines, rhs_start, rhs_count),
                kind,
                move_metadata: None,
                pair_id,
                deletion_rhs_anchor: None,
                refined_spans: Vec::new(),
                prose: true,
            });
        }
    }

    fn finish(self) -> Vec<DiffChange> {
        self.changes
    }
}

fn range_for_lines(lines: &[LineRecord], start: usize, count: usize) -> Range<usize> {
    if count == 0 || start >= lines.len() {
        return 0..0;
    }
    let end_line = (start + count - 1).min(lines.len() - 1);
    lines[start].start..lines[end_line].end
}

/// `line` with no leading or trailing whitespace and each inner run of
/// whitespace cut to one space, which is the form [`align_lines`] compares.
fn collapse_whitespace(line: &str) -> String {
    let mut key = String::with_capacity(line.len());
    for word in line.split_whitespace() {
        if !key.is_empty() {
            key.push(' ');
        }
        key.push_str(word);
    }
    key
}

/// A [`TokenSource`] whose tokens are lines already in the form
/// [`collapse_whitespace`] gives them.
struct KeyLines<'a>(&'a [String]);

impl<'a> TokenSource for KeyLines<'a> {
    type Token = &'a str;
    type Tokenizer = Map<Iter<'a, String>, fn(&'a String) -> &'a str>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.iter().map(String::as_str)
    }

    fn estimate_tokens(&self) -> u32 {
        self.0.len() as u32
    }
}

/// Turns imara-diff's change regions into the line pairs [`align_lines`]
/// returns, with a matched pair for each unchanged line between two regions.
struct PairSink {
    pairs: Vec<(Option<usize>, Option<usize>)>,
    lhs_at: usize,
    rhs_at: usize,
    lhs_len: usize,
}

impl PairSink {
    /// Pair the unchanged lines from the cursors up to lhs line `lhs_end`.
    ///
    /// The diff reports only changes, and an unchanged run between two of them
    /// has the same length on both sides, so both cursors step together.
    fn match_until(&mut self, lhs_end: usize) {
        while self.lhs_at < lhs_end {
            self.pairs.push((Some(self.lhs_at), Some(self.rhs_at)));
            self.lhs_at += 1;
            self.rhs_at += 1;
        }
    }
}

impl Sink for PairSink {
    type Out = Vec<(Option<usize>, Option<usize>)>;

    fn process_change(&mut self, before: Range<u32>, after: Range<u32>) {
        self.match_until(before.start as usize);

        let lhs = before.start as usize..before.end as usize;
        let rhs = after.start as usize..after.end as usize;
        for i in 0..lhs.len().max(rhs.len()) {
            self.pairs.push((
                (i < lhs.len()).then_some(lhs.start + i),
                (i < rhs.len()).then_some(rhs.start + i),
            ));
        }
        self.lhs_at = lhs.end;
        self.rhs_at = rhs.end;
    }

    fn finish(mut self) -> Self::Out {
        let lhs_len = self.lhs_len;
        self.match_until(lhs_len);
        self.pairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes(lhs: &str, rhs: &str) -> Vec<DiffChange> {
        diff_lines(lhs, rhs).changes
    }

    /// The line pass runs where nothing parsed, so the whole file is text with
    /// no token boundary anywhere in it, whatever its extension says.
    #[test]
    fn every_line_change_reports_prose() {
        let changes = changes("fn alpha() {}\nkeep\n", "fn beta() {}\nkeep\nadded\n");
        assert!(!changes.is_empty(), "the fixture produces changes");
        assert!(
            changes.iter().all(|c| c.prose),
            "every change the line pass emits reports prose"
        );
    }

    #[test]
    fn identical_inputs_produce_no_changes() {
        let lhs = "alpha\nbeta\ngamma\n";
        let rhs = "alpha\nbeta\ngamma\n";
        assert!(changes(lhs, rhs).is_empty());
    }

    #[test]
    fn pure_addition_marks_rhs_novel() {
        let lhs = "alpha\nbeta\n";
        let rhs = "alpha\nbeta\ngamma\n";
        let result = changes(lhs, rhs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].side, Side::Rhs);
        assert_eq!(result[0].kind, ChangeKind::Novel);
        // Range covers exactly "gamma".
        assert_eq!(&rhs[result[0].byte_range.clone()], "gamma");
    }

    #[test]
    fn pure_deletion_marks_lhs_novel() {
        let lhs = "alpha\nbeta\ngamma\n";
        let rhs = "alpha\ngamma\n";
        let result = changes(lhs, rhs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].side, Side::Lhs);
        assert_eq!(result[0].kind, ChangeKind::Novel);
        assert_eq!(&lhs[result[0].byte_range.clone()], "beta");
    }

    #[test]
    fn replacement_pairs_lhs_and_rhs() {
        let lhs = "alpha\nbeta\ngamma\n";
        let rhs = "alpha\nBETA\ngamma\n";
        let result = changes(lhs, rhs);
        assert_eq!(result.len(), 2);
        let lhs_change = result.iter().find(|c| c.side == Side::Lhs).unwrap();
        let rhs_change = result.iter().find(|c| c.side == Side::Rhs).unwrap();
        assert_eq!(lhs_change.kind, ChangeKind::Replaced);
        assert_eq!(rhs_change.kind, ChangeKind::Replaced);
        assert_eq!(&lhs[lhs_change.byte_range.clone()], "beta");
        assert_eq!(&rhs[rhs_change.byte_range.clone()], "BETA");
    }

    #[test]
    fn empty_inputs() {
        assert!(changes("", "").is_empty());
        let only_rhs = changes("", "hello\nworld\n");
        assert_eq!(only_rhs.len(), 1);
        assert_eq!(only_rhs[0].side, Side::Rhs);
        assert_eq!(only_rhs[0].kind, ChangeKind::Novel);

        let only_lhs = changes("hello\nworld\n", "");
        assert_eq!(only_lhs.len(), 1);
        assert_eq!(only_lhs[0].side, Side::Lhs);
        assert_eq!(only_lhs[0].kind, ChangeKind::Novel);
    }

    #[test]
    fn fell_back_flag_is_set() {
        let r = diff_lines("a\n", "b\n");
        assert!(r.fell_back_to_line_diff);
    }

    #[test]
    fn replaced_changes_share_pair_id() {
        let result = changes("alpha\nbeta\ngamma\n", "alpha\nBETA\ngamma\n");
        assert_eq!(result.len(), 2);
        let lhs_change = result.iter().find(|c| c.side == Side::Lhs).unwrap();
        let rhs_change = result.iter().find(|c| c.side == Side::Rhs).unwrap();
        assert_eq!(lhs_change.kind, ChangeKind::Replaced);
        assert_eq!(rhs_change.kind, ChangeKind::Replaced);
        assert!(lhs_change.pair_id.is_some());
        assert_eq!(lhs_change.pair_id, rhs_change.pair_id);
    }

    #[test]
    fn novel_changes_have_no_pair_id() {
        let result = changes("", "hello\n");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, ChangeKind::Novel);
        assert!(result[0].pair_id.is_none());
    }

    #[test]
    fn deletion_carries_rhs_anchor() {
        let result = changes("ctx1\nremoved\nctx2\n", "ctx1\nctx2\n");
        let lhs = result.iter().find(|c| c.side == Side::Lhs).unwrap();
        assert_eq!(lhs.kind, ChangeKind::Novel);
        assert_eq!(lhs.deletion_rhs_anchor, Some(1));
    }

    #[test]
    fn align_lines_pairs_a_reindented_line_with_its_old_self() {
        let lhs = ["    let a = 1;\n", "    let b = 2;\n"];
        let rhs = [
            "    if x {\n",
            "        let a = 1;\n",
            "        let b = 2;\n",
            "    }\n",
        ];
        assert_eq!(
            align_lines(&lhs, &rhs),
            [
                (None, Some(0)),
                (Some(0), Some(1)),
                (Some(1), Some(2)),
                (None, Some(3)),
            ],
        );
    }

    /// Inner spacing collapses too, and the lines between two matches pair by
    /// position, the longer side's extra line with nothing.
    #[test]
    fn align_lines_pairs_the_lines_between_matches_by_position() {
        let lhs = ["f(a,  b)", "x", "end"];
        let rhs = ["f(a, b)", "y", "z", "  end"];
        assert_eq!(
            align_lines(&lhs, &rhs),
            [
                (Some(0), Some(0)),
                (Some(1), Some(1)),
                (None, Some(2)),
                (Some(2), Some(3)),
            ],
        );
    }

    #[test]
    fn replacement_has_no_deletion_anchor() {
        let result = changes("alpha\nold\n", "alpha\nnew\n");
        let lhs = result.iter().find(|c| c.side == Side::Lhs).unwrap();
        assert_eq!(lhs.deletion_rhs_anchor, None);
    }

    #[test]
    fn pair_ids_are_distinct_per_run() {
        let result = changes("a\nA\nb\nB\nc\n", "a\nAA\nb\nBB\nc\n");
        let pair_ids: Vec<u32> = result.iter().filter_map(|c| c.pair_id).collect();
        assert_eq!(pair_ids.len(), 4);
        let mut lhs_ids: Vec<u32> = result
            .iter()
            .filter(|c| c.side == Side::Lhs)
            .filter_map(|c| c.pair_id)
            .collect();
        let mut rhs_ids: Vec<u32> = result
            .iter()
            .filter(|c| c.side == Side::Rhs)
            .filter_map(|c| c.pair_id)
            .collect();
        lhs_ids.sort_unstable();
        rhs_ids.sort_unstable();
        assert_eq!(lhs_ids, rhs_ids);
        let unique: std::collections::HashSet<_> = lhs_ids.iter().copied().collect();
        assert_eq!(unique.len(), lhs_ids.len(), "pair ids must be unique");
    }
}
