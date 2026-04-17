use std::{ops::Range, sync::Arc};
use stoat_language::{
    structural_diff::{self, ChangeKind as LangChangeKind, DiffChange, Side},
    Language,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReviewSide {
    pub(crate) text: String,
    pub(crate) line_num: u32,
    /// Byte ranges (within `text`) that are Novel or Replaced on this
    /// side. Rendered with the side-specific add/delete highlight.
    pub(crate) change_spans: Vec<Range<usize>>,
    /// Byte ranges (within `text`) that are tagged as part of a move:
    /// byte-for-byte equal to content elsewhere, just relocated.
    /// Rendered with the central [`crate::display_map::syntax_theme::DiffTheme`]
    /// move color (cyan by default), not red/green, so users see at
    /// a glance that the change is a relocation rather than a gain or
    /// loss.
    pub(crate) moved_spans: Vec<Range<usize>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReviewRow {
    Context {
        left: ReviewSide,
        right: ReviewSide,
    },
    Changed {
        left: Option<ReviewSide>,
        right: Option<ReviewSide>,
    },
}

impl ReviewRow {
    fn is_changed(&self) -> bool {
        matches!(self, ReviewRow::Changed { .. })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReviewHunk {
    pub(crate) rows: Vec<ReviewRow>,
}

pub(crate) fn extract_review_hunks(
    language: Option<&Arc<Language>>,
    base_text: &str,
    buffer_text: &str,
    context: u32,
) -> Vec<ReviewHunk> {
    let diff_result = match language {
        Some(lang) => structural_diff::diff_with_language_or_lines(lang, base_text, buffer_text),
        None => structural_diff::diff(base_text, buffer_text),
    };

    let lhs_lines = split_lines(base_text);
    let rhs_lines = split_lines(buffer_text);

    let lhs_changed = mark_changed_lines(&lhs_lines, &diff_result.changes, Side::Lhs);
    let rhs_changed = mark_changed_lines(&rhs_lines, &diff_result.changes, Side::Rhs);

    let lhs_spans = collect_line_spans(&lhs_lines, &diff_result.changes, Side::Lhs);
    let rhs_spans = collect_line_spans(&rhs_lines, &diff_result.changes, Side::Rhs);
    let lhs_moved = collect_moved_spans(&lhs_lines, &diff_result.changes, Side::Lhs);
    let rhs_moved = collect_moved_spans(&rhs_lines, &diff_result.changes, Side::Rhs);

    let all_rows = structural_walk(
        WalkSide {
            lines: &lhs_lines,
            changed: &lhs_changed,
            spans: &lhs_spans,
            moved: &lhs_moved,
        },
        WalkSide {
            lines: &rhs_lines,
            changed: &rhs_changed,
            spans: &rhs_spans,
            moved: &rhs_moved,
        },
    );
    extract_hunks_with_context(&all_rows, context)
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn mark_changed_lines(lines: &[&str], changes: &[DiffChange], side: Side) -> Vec<bool> {
    let mut changed = vec![false; lines.len()];
    if lines.is_empty() {
        return changed;
    }

    let offsets = line_byte_offsets(lines);

    for change in changes {
        if change.side != side || change.byte_range.start >= change.byte_range.end {
            continue;
        }
        let cr = &change.byte_range;
        let first = offsets.partition_point(|&(_, end)| end < cr.start);
        for (i, &(start, end)) in offsets[first..].iter().enumerate() {
            if start >= cr.end {
                break;
            }
            if cr.start < end + 1 && cr.end > start {
                changed[first + i] = true;
            }
        }
    }

    changed
}

fn collect_line_spans(
    lines: &[&str],
    changes: &[DiffChange],
    side: Side,
) -> Vec<Vec<Range<usize>>> {
    collect_spans_by(lines, changes, side, |kind| {
        matches!(kind, LangChangeKind::Novel | LangChangeKind::Replaced)
    })
}

fn collect_moved_spans(
    lines: &[&str],
    changes: &[DiffChange],
    side: Side,
) -> Vec<Vec<Range<usize>>> {
    collect_spans_by(lines, changes, side, |kind| {
        matches!(kind, LangChangeKind::Moved)
    })
}

fn collect_spans_by(
    lines: &[&str],
    changes: &[DiffChange],
    side: Side,
    include: impl Fn(&LangChangeKind) -> bool,
) -> Vec<Vec<Range<usize>>> {
    let mut spans: Vec<Vec<Range<usize>>> = vec![Vec::new(); lines.len()];
    if lines.is_empty() {
        return spans;
    }

    let offsets = line_byte_offsets(lines);

    for change in changes {
        if change.side != side
            || change.byte_range.start >= change.byte_range.end
            || !include(&change.kind)
        {
            continue;
        }
        let cr = &change.byte_range;
        let first = offsets.partition_point(|&(_, end)| end < cr.start);
        for (i, &(line_start, line_end)) in offsets[first..].iter().enumerate() {
            if line_start >= cr.end {
                break;
            }
            let span_start = cr.start.max(line_start) - line_start;
            let span_end = cr.end.min(line_end) - line_start;
            if span_start < span_end {
                spans[first + i].push(span_start..span_end);
            }
        }
    }

    spans
}

fn line_byte_offsets(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut offsets = Vec::with_capacity(lines.len());
    let mut pos = 0usize;
    for &line in lines {
        let end = pos + line.len();
        offsets.push((pos, end));
        pos = end + 1;
    }
    offsets
}

struct WalkSide<'a> {
    lines: &'a [&'a str],
    changed: &'a [bool],
    spans: &'a [Vec<Range<usize>>],
    moved: &'a [Vec<Range<usize>>],
}

/// Walk both files using unchanged lines as alignment anchors. Changed
/// regions are collected and paired into side-by-side rows.
fn structural_walk(lhs: WalkSide<'_>, rhs: WalkSide<'_>) -> Vec<ReviewRow> {
    let mut result = Vec::new();
    let mut li = 0usize;
    let mut ri = 0usize;
    let mut old_line = 1u32;
    let mut new_line = 1u32;

    while li < lhs.lines.len() || ri < rhs.lines.len() {
        let l_ok = li < lhs.lines.len();
        let r_ok = ri < rhs.lines.len();
        let l_unchanged = l_ok && !lhs.changed[li];
        let r_unchanged = r_ok && !rhs.changed[ri];

        if l_unchanged && r_unchanged && lhs.lines[li] == rhs.lines[ri] {
            result.push(ReviewRow::Context {
                left: ReviewSide {
                    text: lhs.lines[li].to_string(),
                    line_num: old_line,
                    change_spans: Vec::new(),
                    moved_spans: Vec::new(),
                },
                right: ReviewSide {
                    text: rhs.lines[ri].to_string(),
                    line_num: new_line,
                    change_spans: Vec::new(),
                    moved_spans: Vec::new(),
                },
            });
            li += 1;
            ri += 1;
            old_line += 1;
            new_line += 1;
            continue;
        }

        // Collect runs of changed lines from both sides, then pair them.
        let mut left_run: Vec<ReviewSide> = Vec::new();
        let mut right_run: Vec<ReviewSide> = Vec::new();

        while li < lhs.lines.len() && lhs.changed[li] {
            left_run.push(ReviewSide {
                text: lhs.lines[li].to_string(),
                line_num: old_line,
                change_spans: lhs.spans[li].clone(),
                moved_spans: lhs.moved[li].clone(),
            });
            li += 1;
            old_line += 1;
        }

        while ri < rhs.lines.len() && rhs.changed[ri] {
            right_run.push(ReviewSide {
                text: rhs.lines[ri].to_string(),
                line_num: new_line,
                change_spans: rhs.spans[ri].clone(),
                moved_spans: rhs.moved[ri].clone(),
            });
            ri += 1;
            new_line += 1;
        }

        if left_run.is_empty() && right_run.is_empty() {
            // Both unchanged but text differs (structural diff paired
            // tokens across non-corresponding lines). Treat as change.
            if l_ok && r_ok {
                result.push(ReviewRow::Changed {
                    left: Some(ReviewSide {
                        text: lhs.lines[li].to_string(),
                        line_num: old_line,
                        change_spans: Vec::new(),
                        moved_spans: Vec::new(),
                    }),
                    right: Some(ReviewSide {
                        text: rhs.lines[ri].to_string(),
                        line_num: new_line,
                        change_spans: Vec::new(),
                        moved_spans: Vec::new(),
                    }),
                });
                li += 1;
                ri += 1;
                old_line += 1;
                new_line += 1;
            } else if l_ok {
                result.push(ReviewRow::Changed {
                    left: Some(ReviewSide {
                        text: lhs.lines[li].to_string(),
                        line_num: old_line,
                        change_spans: Vec::new(),
                        moved_spans: Vec::new(),
                    }),
                    right: None,
                });
                li += 1;
                old_line += 1;
            } else {
                result.push(ReviewRow::Changed {
                    left: None,
                    right: Some(ReviewSide {
                        text: rhs.lines[ri].to_string(),
                        line_num: new_line,
                        change_spans: Vec::new(),
                        moved_spans: Vec::new(),
                    }),
                });
                ri += 1;
                new_line += 1;
            }
            continue;
        }

        // Pair left and right runs side-by-side.
        let max = left_run.len().max(right_run.len());
        for i in 0..max {
            result.push(ReviewRow::Changed {
                left: left_run.get(i).cloned(),
                right: right_run.get(i).cloned(),
            });
        }
    }

    result
}

fn extract_hunks_with_context(all_rows: &[ReviewRow], context: u32) -> Vec<ReviewHunk> {
    let change_indices: Vec<usize> = all_rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.is_changed())
        .map(|(i, _)| i)
        .collect();

    if change_indices.is_empty() {
        return Vec::new();
    }

    let len = all_rows.len();
    let ctx = context as usize;

    let mut regions: Vec<Range<usize>> = Vec::new();
    for &ci in &change_indices {
        let start = ci.saturating_sub(ctx);
        let end = (ci + 1 + ctx).min(len);
        match regions.last_mut() {
            Some(last) if start <= last.end => last.end = last.end.max(end),
            _ => regions.push(start..end),
        }
    }

    regions
        .into_iter()
        .map(|r| ReviewHunk {
            rows: all_rows[r].to_vec(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunks(base: &str, buffer: &str, ctx: u32) -> Vec<ReviewHunk> {
        extract_review_hunks(None, base, buffer, ctx)
    }

    #[test]
    fn no_changes() {
        assert!(hunks("a\nb\nc\n", "a\nb\nc\n", 3).is_empty());
    }

    #[test]
    fn single_addition() {
        let hs = hunks("a\nb\n", "a\nnew\nb\n", 1);
        assert_eq!(hs.len(), 1);
        let rows = &hs[0].rows;
        assert_eq!(rows.len(), 3);
        // Row 0: context "a"
        assert!(matches!(&rows[0], ReviewRow::Context { left, right }
            if left.text == "a" && right.text == "a"
               && left.line_num == 1 && right.line_num == 1));
        // Row 1: added "new" (left=None, right=Some)
        assert!(
            matches!(&rows[1], ReviewRow::Changed { left: None, right: Some(r) }
            if r.text == "new" && r.line_num == 2)
        );
        // Row 2: context "b"
        assert!(matches!(&rows[2], ReviewRow::Context { .. }));
    }

    #[test]
    fn single_deletion() {
        let hs = hunks("a\nold\nb\n", "a\nb\n", 1);
        assert_eq!(hs.len(), 1);
        let rows = &hs[0].rows;
        assert_eq!(rows.len(), 3);
        // Row 0: context "a"
        assert!(matches!(&rows[0], ReviewRow::Context { .. }));
        // Row 1: deleted "old" (left=Some, right=None)
        assert!(
            matches!(&rows[1], ReviewRow::Changed { left: Some(l), right: None }
            if l.text == "old" && l.line_num == 2)
        );
        // Row 2: context "b"
        assert!(matches!(&rows[2], ReviewRow::Context { .. }));
    }

    #[test]
    fn modification_pairs_side_by_side() {
        let hs = hunks("a\nold\nb\n", "a\nnew\nb\n", 1);
        assert_eq!(hs.len(), 1);
        let rows = &hs[0].rows;
        // Row 1: old on left, new on right (same row)
        assert!(
            matches!(&rows[1], ReviewRow::Changed { left: Some(l), right: Some(r) }
            if l.text == "old" && r.text == "new")
        );
    }

    #[test]
    fn two_separate_hunks() {
        let hs = hunks("a\nb\nc\nd\ne\nf\ng\nh\n", "a\nB\nc\nd\ne\nF\ng\nh\n", 1);
        assert_eq!(hs.len(), 2);
        // First hunk: b→B paired
        let row = &hs[0].rows[1];
        assert!(
            matches!(row, ReviewRow::Changed { left: Some(l), right: Some(r) }
            if l.text == "b" && r.text == "B")
        );
    }

    #[test]
    fn adjacent_hunks_merge() {
        let hs = hunks("a\nb\nc\nd\ne\n", "a\nB\nc\nD\ne\n", 1);
        assert_eq!(hs.len(), 1);
    }

    #[test]
    fn empty_base() {
        let hs = hunks("", "a\nb\n", 1);
        assert_eq!(hs.len(), 1);
        assert!(hs[0].rows.iter().all(|r| matches!(
            r,
            ReviewRow::Changed {
                left: None,
                right: Some(_)
            }
        )));
    }

    #[test]
    fn empty_buffer() {
        let hs = hunks("a\nb\n", "", 1);
        assert_eq!(hs.len(), 1);
        assert!(hs[0].rows.iter().all(|r| matches!(
            r,
            ReviewRow::Changed {
                left: Some(_),
                right: None
            }
        )));
    }

    #[test]
    fn line_numbers_track_correctly() {
        let hs = hunks("a\nb\nc\nd\n", "a\nx\ny\nc\nd\n", 1);
        assert_eq!(hs.len(), 1);
        let rows = &hs[0].rows;
        // "a": context
        match &rows[0] {
            ReviewRow::Context { left, right } => {
                assert_eq!(left.line_num, 1);
                assert_eq!(right.line_num, 1);
            },
            _ => panic!("expected context"),
        }
        // "b" deleted, "x" added, "y" added → 2 changed rows
        match &rows[1] {
            ReviewRow::Changed {
                left: Some(l),
                right: Some(r),
            } => {
                assert_eq!(l.line_num, 2); // old "b"
                assert_eq!(r.line_num, 2); // new "x"
            },
            _ => panic!("expected changed with both sides"),
        }
        match &rows[2] {
            ReviewRow::Changed {
                left: None,
                right: Some(r),
            } => {
                assert_eq!(r.line_num, 3); // new "y"
            },
            _ => panic!("expected addition-only"),
        }
        // "c": context
        match &rows[3] {
            ReviewRow::Context { left, right } => {
                assert_eq!(left.line_num, 3);
                assert_eq!(right.line_num, 4);
            },
            _ => panic!("expected context"),
        }
    }

    #[test]
    fn change_spans_present_on_changed_rows() {
        let hs = hunks("let x = 1;\n", "let x = 2;\n", 0);
        assert_eq!(hs.len(), 1);
        match &hs[0].rows[0] {
            ReviewRow::Changed {
                left: Some(l),
                right: Some(r),
            } => {
                assert!(!l.change_spans.is_empty());
                assert!(!r.change_spans.is_empty());
            },
            _ => panic!("expected paired change"),
        }
    }

    #[test]
    fn mark_changed_lines_basic() {
        let text = "aaa\nbbb\nccc\n";
        let lines = split_lines(text);
        let changes = vec![DiffChange {
            side: Side::Lhs,
            byte_range: 4..7,
            kind: structural_diff::ChangeKind::Novel,
            move_metadata: None,
            pair_id: None,
            deletion_rhs_anchor: None,
        }];
        assert_eq!(
            mark_changed_lines(&lines, &changes, Side::Lhs),
            vec![false, true, false]
        );
    }

    #[test]
    fn collect_spans_within_line() {
        let text = "hello world\n";
        let lines = split_lines(text);
        let changes = vec![DiffChange {
            side: Side::Rhs,
            byte_range: 6..11,
            kind: structural_diff::ChangeKind::Replaced,
            move_metadata: None,
            pair_id: None,
            deletion_rhs_anchor: None,
        }];
        let spans = collect_line_spans(&lines, &changes, Side::Rhs);
        assert_eq!(spans, vec![vec![6..11]]);
    }

    use crate::test_harness::TestHarness;

    #[test]
    fn snapshot_review_addition() {
        let mut h = TestHarness::with_size(80, 10);
        h.open_review_from_texts(&[(
            "test.rs",
            "fn a() {}\nfn b() {}\n",
            "fn a() {}\nfn new() {}\nfn b() {}\n",
        )]);
        h.assert_snapshot("review_addition");
    }

    #[test]
    fn snapshot_review_deletion() {
        let mut h = TestHarness::with_size(80, 10);
        h.open_review_from_texts(&[(
            "test.rs",
            "fn a() {}\nfn old() {}\nfn b() {}\n",
            "fn a() {}\nfn b() {}\n",
        )]);
        h.assert_snapshot("review_deletion");
    }

    #[test]
    fn snapshot_review_modification() {
        let mut h = TestHarness::with_size(80, 10);
        h.open_review_from_texts(&[(
            "test.rs",
            "fn main() {\n    let x = 1;\n}\n",
            "fn main() {\n    let x = 2;\n}\n",
        )]);
        h.assert_snapshot("review_modification");
    }

    #[test]
    fn snapshot_review_multi_file() {
        let mut h = TestHarness::with_size(80, 16);
        h.open_review_from_texts(&[
            ("a.rs", "fn a() {}\n", "fn a_renamed() {}\n"),
            ("b.rs", "let x = 1;\n", "let x = 1;\nlet y = 2;\n"),
        ]);
        h.assert_snapshot("review_multi_file");
    }

    #[test]
    fn snapshot_review_move_straight() {
        let mut h = TestHarness::with_size(100, 20);
        let base = "\
fn alpha() {
    let x = 1;
    let y = 2;
    let z = 3;
}

fn beta() {
    let p = 10;
    let q = 20;
    let r = 30;
}
";
        let rhs = "\
fn beta() {
    let p = 10;
    let q = 20;
    let r = 30;
}

fn alpha() {
    let x = 1;
    let y = 2;
    let z = 3;
}
";
        h.open_review_from_texts(&[("swap.rs", base, rhs)]);
        h.assert_snapshot("review_move_straight");
    }

    #[test]
    fn snapshot_review_move_cross_indent() {
        let mut h = TestHarness::with_size(100, 20);
        let base = "\
fn outer() {
    let relocated = compute(arg1, arg2, arg3);
}

fn wrapper() {
    println!(\"hello\");
}
";
        let rhs = "\
fn outer() {}

fn wrapper() {
    println!(\"hello\");
    let relocated = compute(arg1, arg2, arg3);
}
";
        h.open_review_from_texts(&[("nest.rs", base, rhs)]);
        h.assert_snapshot("review_move_cross_indent");
    }

    #[test]
    fn snapshot_review_move_consolidation() {
        let mut h = TestHarness::with_size(100, 24);
        let base = "\
fn first() {
    let temp = heavy_computation(a, b, c);
    save(temp);
}

fn second() {
    let temp = heavy_computation(a, b, c);
    save(temp);
}
";
        let rhs = "\
fn shared() {
    let temp = heavy_computation(a, b, c);
    save(temp);
}

fn first() {
    shared();
}

fn second() {
    shared();
}
";
        h.open_review_from_texts(&[("consolidate.rs", base, rhs)]);
        h.assert_snapshot("review_move_consolidation");
    }
}
