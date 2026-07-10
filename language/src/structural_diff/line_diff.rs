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
use imara_diff::{intern::InternedInput, sources, Algorithm, Sink};
use std::ops::Range;

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
    let changes = imara_diff::diff(
        Algorithm::Histogram,
        &input,
        ChangeSink {
            lhs_lines: &lhs_lines,
            rhs_lines: &rhs_lines,
            changes: Vec::new(),
            next_pair_id: 0,
        },
    );

    DiffResult {
        changes,
        fell_back_to_line_diff: true,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn changes(lhs: &str, rhs: &str) -> Vec<DiffChange> {
        diff_lines(lhs, rhs).changes
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
