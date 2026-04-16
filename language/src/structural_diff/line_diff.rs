//! Line-level diff fallback used by [`super::diff`] until the
//! tree-based path is in place, and as the safety net for inputs that
//! exceed the structural-diff graph cap.
//!
//! Implements the longest-common-subsequence approach: hash each line of
//! both inputs, build an `LCS` table, then walk the table backwards to
//! emit Lhs/Rhs novel runs. Adjacent Lhs+Rhs novel pairs are tagged
//! [`super::ChangeKind::Replaced`] (one side replaces the other);
//! standalone runs are [`super::ChangeKind::Novel`].
//!
//! Complexity: O(L * R) time and space, where L and R are line counts.
//! For ~10kloc files this is ~100 MB; the tree-based path will avoid
//! this for the common case. The fallback is a hard ceiling, not a hot
//! path.

use super::{ChangeKind, DiffChange, DiffResult, Side};

/// Compute a line-level diff between `lhs` and `rhs`. The returned
/// changes carry rope byte ranges (relative to each input) so they can
/// be threaded directly into [`crate`]'s `DiffMap` scaffolding.
pub fn diff_lines(lhs: &str, rhs: &str) -> DiffResult {
    let lhs_lines = lines_with_offsets(lhs);
    let rhs_lines = lines_with_offsets(rhs);

    let lcs = build_lcs_table(&lhs_lines, &rhs_lines);
    let walk = walk_lcs(&lhs_lines, &rhs_lines, &lcs);

    let changes = collapse_runs_into_changes(&lhs_lines, &rhs_lines, walk);

    DiffResult {
        changes,
        fell_back_to_line_diff: true,
    }
}

/// One line plus its (start, end) byte offset in the source string.
/// `end` excludes the trailing newline so adjacent lines don't share a
/// boundary byte.
struct LineRecord<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

fn lines_with_offsets(text: &str) -> Vec<LineRecord<'_>> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            out.push(LineRecord {
                text: &text[start..idx],
                start,
                end: idx,
            });
            start = idx + 1;
        }
    }
    if start <= text.len() {
        out.push(LineRecord {
            text: &text[start..],
            start,
            end: text.len(),
        });
    }
    out
}

/// Standard O(n*m) LCS table over line equality.
fn build_lcs_table(lhs: &[LineRecord<'_>], rhs: &[LineRecord<'_>]) -> Vec<Vec<u32>> {
    let l = lhs.len();
    let r = rhs.len();
    let mut table = vec![vec![0u32; r + 1]; l + 1];
    for i in 0..l {
        for j in 0..r {
            table[i + 1][j + 1] = if lhs[i].text == rhs[j].text {
                table[i][j] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    table
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WalkOp {
    Equal,
    DelLhs,
    InsRhs,
}

/// Walk the LCS table backwards to produce a per-position op stream.
/// `ops` ends up in source order: index 0 is the first decision made
/// (lhs[0]/rhs[0]).
fn walk_lcs(lhs: &[LineRecord<'_>], rhs: &[LineRecord<'_>], table: &[Vec<u32>]) -> Vec<WalkOp> {
    let mut i = lhs.len();
    let mut j = rhs.len();
    let mut ops = Vec::new();
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && lhs[i - 1].text == rhs[j - 1].text {
            ops.push(WalkOp::Equal);
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || table[i][j - 1] >= table[i - 1][j]) {
            ops.push(WalkOp::InsRhs);
            j -= 1;
        } else {
            ops.push(WalkOp::DelLhs);
            i -= 1;
        }
    }
    ops.reverse();
    ops
}

/// Walk the op list and collapse runs of consecutive Lhs/Rhs ops into
/// [`DiffChange`]s. Adjacent Lhs+Rhs runs (in either order, with no
/// intervening Equal) become [`ChangeKind::Replaced`]; isolated runs
/// become [`ChangeKind::Novel`].
fn collapse_runs_into_changes(
    lhs: &[LineRecord<'_>],
    rhs: &[LineRecord<'_>],
    ops: Vec<WalkOp>,
) -> Vec<DiffChange> {
    let mut changes = Vec::new();
    let mut lhs_idx = 0usize;
    let mut rhs_idx = 0usize;
    let mut i = 0usize;

    while i < ops.len() {
        match ops[i] {
            WalkOp::Equal => {
                lhs_idx += 1;
                rhs_idx += 1;
                i += 1;
            },
            _ => {
                let run_start = i;
                let lhs_start = lhs_idx;
                let rhs_start = rhs_idx;
                let mut lhs_count = 0usize;
                let mut rhs_count = 0usize;
                while i < ops.len() && ops[i] != WalkOp::Equal {
                    match ops[i] {
                        WalkOp::DelLhs => {
                            lhs_count += 1;
                            lhs_idx += 1;
                        },
                        WalkOp::InsRhs => {
                            rhs_count += 1;
                            rhs_idx += 1;
                        },
                        WalkOp::Equal => unreachable!(),
                    }
                    i += 1;
                }
                let kind = if lhs_count > 0 && rhs_count > 0 {
                    ChangeKind::Replaced
                } else {
                    ChangeKind::Novel
                };
                if lhs_count > 0 {
                    changes.push(DiffChange {
                        side: Side::Lhs,
                        byte_range: range_for_lines(lhs, lhs_start, lhs_count),
                        kind,
                        move_metadata: None,
                    });
                }
                if rhs_count > 0 {
                    changes.push(DiffChange {
                        side: Side::Rhs,
                        byte_range: range_for_lines(rhs, rhs_start, rhs_count),
                        kind,
                        move_metadata: None,
                    });
                }
                let _ = run_start;
            },
        }
    }
    changes
}

fn range_for_lines(lines: &[LineRecord<'_>], start: usize, count: usize) -> std::ops::Range<usize> {
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
}
