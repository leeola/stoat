use crate::{
    review,
    review::{ReviewRow, ReviewSide},
};
use std::sync::Arc;
use stoat_language::Language;

/// One row of the three-column merge view, aligning the common ancestor with
/// the two sides of a conflict.
///
/// A row either pairs a single ancestor line with the ours and theirs lines
/// that descend from it (`base` set), or carries a one-sided insertion with no
/// ancestor line (`base` None). `conflict` is set when both sides changed the
/// ancestor line the row covers.
// Consumed by render_conflict, the merge-view rendering sibling.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MergeRow {
    pub base: Option<ReviewSide>,
    pub ours: Option<ReviewSide>,
    pub theirs: Option<ReviewSide>,
    pub conflict: bool,
}

/// Align `ancestor` against `ours` and `theirs` into three-column merge rows.
///
/// Each side is diffed against the ancestor, then the two aligned walks are
/// merged on their shared ancestor lines. Rows for the same ancestor line pair
/// into one [`MergeRow`], and side-only insertions interleave after their
/// preceding ancestor line with ours before theirs. A row is a conflict when
/// both sides changed the ancestor line it covers.
// Consumed by render_conflict, the merge-view rendering sibling.
#[allow(dead_code)]
pub(crate) fn build_merge_rows(
    ancestor: &str,
    ours: &str,
    theirs: &str,
    language: Option<&Arc<Language>>,
) -> Vec<MergeRow> {
    let ours_rows = review::aligned_rows(ancestor, ours, language);
    let theirs_rows = review::aligned_rows(ancestor, theirs, language);

    let mut rows = Vec::new();
    let mut i = 0;
    let mut j = 0;

    loop {
        while let Some((None, right, _)) = ours_rows.get(i).map(row_parts) {
            rows.push(MergeRow {
                base: None,
                ours: right.cloned(),
                theirs: None,
                conflict: false,
            });
            i += 1;
        }
        while let Some((None, right, _)) = theirs_rows.get(j).map(row_parts) {
            rows.push(MergeRow {
                base: None,
                ours: None,
                theirs: right.cloned(),
                conflict: false,
            });
            j += 1;
        }

        let (Some(ours_row), Some(theirs_row)) = (ours_rows.get(i), theirs_rows.get(j)) else {
            break;
        };
        let (base, ours_side, ours_changed) = row_parts(ours_row);
        let (_, theirs_side, theirs_changed) = row_parts(theirs_row);
        rows.push(MergeRow {
            base: base.cloned(),
            ours: ours_side.cloned(),
            theirs: theirs_side.cloned(),
            conflict: ours_changed && theirs_changed,
        });
        i += 1;
        j += 1;
    }

    rows
}

/// Split a review row into its (base side, other side, is-changed) parts. `left`
/// is the base (ancestor) side and `right` the other side; a `Context` row is
/// unchanged on both, a `Changed` row is changed with either side possibly
/// absent (a deletion or an insertion).
fn row_parts(row: &ReviewRow) -> (Option<&ReviewSide>, Option<&ReviewSide>, bool) {
    match row {
        ReviewRow::Context { left, right } => (Some(left), Some(right), false),
        ReviewRow::Changed { left, right } => (left.as_ref(), right.as_ref(), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflicts(rows: &[MergeRow]) -> Vec<bool> {
        rows.iter().map(|r| r.conflict).collect()
    }

    fn text(side: &Option<ReviewSide>) -> Option<&str> {
        side.as_ref().map(|s| s.text.as_str())
    }

    #[test]
    fn disjoint_edits_never_conflict() {
        let rows = build_merge_rows("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nb\nc\nD\n", None);
        assert_eq!(conflicts(&rows), [false, false, false, false]);
        assert_eq!(text(&rows[1].ours), Some("B"));
        assert_eq!(text(&rows[1].theirs), Some("b"));
        assert_eq!(text(&rows[3].theirs), Some("D"));
    }

    #[test]
    fn same_line_divergent_edit_conflicts() {
        let rows = build_merge_rows(
            "1 shared header\n2 middle line\n3 shared footer\n",
            "1 shared header\n2 ours edit\n3 shared footer\n",
            "1 shared header\n2 theirs edit\n3 shared footer\n",
            None,
        );
        assert_eq!(conflicts(&rows), [false, true, false]);
        assert_eq!(text(&rows[1].base), Some("2 middle line"));
        assert_eq!(text(&rows[1].ours), Some("2 ours edit"));
        assert_eq!(text(&rows[1].theirs), Some("2 theirs edit"));
    }

    #[test]
    fn one_sided_insertion_interleaves() {
        let rows = build_merge_rows("a\nb\n", "a\nINSERT\nb\n", "a\nb\n", None);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            text(&rows[1].base),
            None,
            "an insertion has no ancestor line"
        );
        assert_eq!(text(&rows[1].ours), Some("INSERT"));
        assert_eq!(text(&rows[1].theirs), None);
        assert!(!rows[1].conflict);
    }

    #[test]
    fn pure_context_pairs_three_ways() {
        let rows = build_merge_rows("x\ny\nz\n", "x\ny\nz\n", "x\ny\nz\n", None);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| {
            r.base.is_some() && r.ours.is_some() && r.theirs.is_some() && !r.conflict
        }));
    }
}
