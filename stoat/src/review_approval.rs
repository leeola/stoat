//! The rows a range review marks approved, kept per file.
//!
//! A range review diffs against a merge base that no commit sits over, so its
//! staging keys have no index or commit to write into. The review keeps the
//! rows the reader approved instead, and a diff map built under the review
//! reads them as its staged state.

use std::ops::Range;

/// The buffer rows a review approved in one file, as sorted, disjoint ranges.
///
/// Ranges that touch merge, so a run approved over two presses reads as one.
/// A zero-width hunk, a deletion anchored at row `a`, has no row of its own and
/// is stored as `a..a + 1`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ApprovedRows(Vec<Range<u32>>);

impl ApprovedRows {
    /// Add `rows` to the approved set.
    pub(crate) fn approve(&mut self, rows: Range<u32>) {
        if rows.is_empty() {
            return;
        }
        let mut merged = rows;
        self.0.retain(|range| {
            let apart = range.end < merged.start || merged.end < range.start;
            if !apart {
                merged.start = merged.start.min(range.start);
                merged.end = merged.end.max(range.end);
            }
            apart
        });
        let at = self.0.partition_point(|range| range.start < merged.start);
        self.0.insert(at, merged);
    }

    /// Take `rows` out of the approved set.
    ///
    /// A range that holds `rows` in its middle splits in two.
    pub(crate) fn unapprove(&mut self, rows: Range<u32>) {
        if rows.is_empty() {
            return;
        }
        let mut kept = Vec::with_capacity(self.0.len() + 1);
        for range in self.0.drain(..) {
            if range.end <= rows.start || rows.end <= range.start {
                kept.push(range);
                continue;
            }
            if range.start < rows.start {
                kept.push(range.start..rows.start);
            }
            if rows.end < range.end {
                kept.push(rows.end..range.end);
            }
        }
        self.0 = kept;
    }

    /// Whether every row of `rows` is approved.
    pub(crate) fn covers(&self, rows: &Range<u32>) -> bool {
        self.0
            .iter()
            .any(|range| range.start <= rows.start && rows.end <= range.end)
    }

    /// The approved ranges, sorted and disjoint.
    pub(crate) fn ranges(&self) -> &[Range<u32>] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::ApprovedRows;
    use std::ops::Range;

    fn rows(ranges: &[Range<u32>]) -> ApprovedRows {
        let mut rows = ApprovedRows::default();
        for range in ranges {
            rows.approve(range.clone());
        }
        rows
    }

    /// The merge stops where the next range sits apart.
    #[test]
    fn approving_the_gap_merges_its_neighbors() {
        let mut approved = rows(&[0..2, 4..6, 8..9]);
        approved.approve(2..4);
        assert_eq!(approved.ranges(), [0..6, 8..9]);
    }

    #[test]
    fn unapproving_the_middle_splits_the_range() {
        let mut approved = rows(&[0..6, 8..9]);
        approved.unapprove(2..4);
        assert_eq!(approved.ranges(), [0..2, 4..6, 8..9]);
    }

    #[test]
    fn covers_needs_every_row() {
        let approved = rows(&[0..2, 4..6]);
        assert_eq!(
            [0..2, 1..3, 4..5, 2..4].map(|range| approved.covers(&range)),
            [true, false, true, false],
        );
    }
}
