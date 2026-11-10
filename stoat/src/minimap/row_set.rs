//! The rows of a minimap store that hold their summaries.

use std::ops::Range;

/// A set of rows, held as sorted, disjoint ranges that never touch.
///
/// A strip builds the rows around its view and the rows a scroll brings near
/// it, so the built rows of a large file are a few runs rather than one prefix.
/// One range per run keeps each question about them a short walk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct RowSet(Vec<Range<u32>>);

impl RowSet {
    /// Whether any row of `rows` is in the set.
    pub(super) fn intersects(&self, rows: Range<u32>) -> bool {
        if rows.is_empty() {
            return false;
        }
        let at = self.0.partition_point(|run| run.end <= rows.start);
        self.0.get(at).is_some_and(|run| run.start < rows.end)
    }

    /// Whether every row of `rows` is in the set.
    pub(super) fn covers(&self, rows: Range<u32>) -> bool {
        if rows.is_empty() {
            return true;
        }
        let at = self.0.partition_point(|run| run.end <= rows.start);
        self.0
            .get(at)
            .is_some_and(|run| run.start <= rows.start && rows.end <= run.end)
    }

    /// Add `rows`, merged with every run it overlaps or touches.
    pub(super) fn insert(&mut self, rows: Range<u32>) {
        if rows.is_empty() {
            return;
        }
        let from = self.0.partition_point(|run| run.end < rows.start);
        let to = self.0.partition_point(|run| run.start <= rows.end);
        let merged = match (self.0[from..to].first(), self.0[from..to].last()) {
            (Some(first), Some(last)) => first.start.min(rows.start)..last.end.max(rows.end),
            _ => rows,
        };
        self.0.splice(from..to, [merged]);
    }

    /// Follow an edit that replaced `removed` rows from `start` with
    /// `inserted` rows, which join the set when `built` is set.
    ///
    /// The replaced rows leave the set, and the rows after them move by the
    /// line delta, as the store's rows move.
    pub(super) fn splice(&mut self, start: u32, removed: u32, inserted: u32, built: bool) {
        let end = start.saturating_add(removed);
        let delta = i64::from(inserted) - i64::from(removed);
        let shift = |row: u32| (i64::from(row) + delta).max(0) as u32;

        let runs = std::mem::take(&mut self.0);
        for run in runs {
            if run.start < start {
                self.insert(run.start..run.end.min(start));
            }
            if run.end > end {
                self.insert(shift(run.start.max(end))..shift(run.end));
            }
        }
        if built {
            self.insert(start..start.saturating_add(inserted));
        }
    }

    /// The set holding `rows` alone.
    pub(super) fn of(rows: Range<u32>) -> RowSet {
        let mut set = RowSet::default();
        set.insert(rows);
        set
    }

    /// The rows of this set outside `other`, ascending.
    pub(super) fn difference(&self, other: &RowSet) -> Vec<Range<u32>> {
        self.0
            .iter()
            .flat_map(|run| other.gaps(run.clone()))
            .collect()
    }

    /// The rows of `within` outside the set, ascending.
    pub(super) fn gaps(&self, within: Range<u32>) -> Vec<Range<u32>> {
        let mut gaps = Vec::new();
        let mut at = within.start;
        let first = self.0.partition_point(|run| run.end <= at);
        for run in &self.0[first..] {
            if run.start >= within.end {
                break;
            }
            if run.start > at {
                gaps.push(at..run.start);
            }
            at = run.end;
        }
        if at < within.end {
            gaps.push(at..within.end);
        }
        gaps
    }

    /// The first rows of the set within `from..end`, a run of at most `max`
    /// of them, or `None` when the set holds none there.
    pub(super) fn next_run(&self, from: u32, end: u32, max: u32) -> Option<Range<u32>> {
        let at = self.0.partition_point(|run| run.end <= from);
        let run = self.0.get(at)?;
        let start = run.start.max(from);
        if start >= end {
            return None;
        }
        Some(start..run.end.min(end).min(start.saturating_add(max)))
    }
}

#[cfg(test)]
mod tests {
    use super::RowSet;
    use std::ops::Range;

    /// The set holding the `(start, end)` runs in `runs`.
    fn set(runs: &[(u32, u32)]) -> RowSet {
        let mut set = RowSet::default();
        for &(start, end) in runs {
            set.insert(start..end);
        }
        set
    }

    /// The `(start, end)` of each range.
    fn pairs(ranges: impl IntoIterator<Item = Range<u32>>) -> Vec<(u32, u32)> {
        ranges.into_iter().map(|run| (run.start, run.end)).collect()
    }

    #[test]
    fn an_insert_merges_the_runs_it_touches() {
        assert_eq!(
            [
                pairs(set(&[(0, 4), (8, 12), (4, 8)]).0),
                pairs(set(&[(0, 4), (10, 12), (5, 6)]).0),
                pairs(set(&[(2, 6), (0, 10)]).0),
                pairs(set(&[(0, 2), (0, 0)]).0),
            ],
            [
                vec![(0, 12)],
                vec![(0, 4), (5, 6), (10, 12)],
                vec![(0, 10)],
                vec![(0, 2)],
            ],
        );
    }

    #[test]
    fn a_splice_drops_the_replaced_rows_and_moves_the_rest() {
        let splice = |start, removed, inserted, built| {
            let mut rows = set(&[(0, 10), (20, 30)]);
            rows.splice(start, removed, inserted, built);
            pairs(rows.0)
        };

        assert_eq!(
            [
                splice(5, 2, 0, false),
                splice(5, 2, 4, false),
                splice(5, 2, 4, true),
                splice(12, 3, 1, false),
                splice(8, 15, 0, false),
                splice(10, 10, 0, false),
            ],
            [
                vec![(0, 8), (18, 28)],
                vec![(0, 5), (9, 12), (22, 32)],
                vec![(0, 12), (22, 32)],
                vec![(0, 10), (18, 28)],
                vec![(0, 15)],
                vec![(0, 20)],
            ],
        );
    }

    #[test]
    fn gaps_are_the_rows_outside_the_set() {
        let rows = set(&[(5, 10), (20, 30)]);

        assert_eq!(
            [
                pairs(rows.gaps(0..40)),
                pairs(rows.gaps(6..25)),
                pairs(rows.gaps(22..28)),
                pairs(rows.gaps(12..15)),
            ],
            [
                vec![(0, 5), (10, 20), (30, 40)],
                vec![(10, 20)],
                vec![],
                vec![(12, 15)],
            ],
        );
    }

    #[test]
    fn next_run_takes_set_rows_in_order_and_in_bounded_steps() {
        let rows = set(&[(5, 10), (20, 30)]);

        assert_eq!(
            pairs(
                [
                    rows.next_run(0, 40, 100),
                    rows.next_run(7, 40, 100),
                    rows.next_run(10, 40, 4),
                ]
                .into_iter()
                .flatten()
            ),
            [(5, 10), (7, 10), (20, 24)],
        );
        assert_eq!(
            [rows.next_run(10, 15, 100), rows.next_run(30, 40, 100)],
            [None, None],
            "no set rows lie in those bounds",
        );
    }

    #[test]
    fn intersects_and_covers_ask_about_any_and_every_row() {
        let rows = set(&[(5, 10), (20, 30)]);

        assert_eq!(
            [
                (rows.intersects(0..6), rows.covers(0..6)),
                (rows.intersects(6..9), rows.covers(6..9)),
                (rows.intersects(10..20), rows.covers(10..20)),
                (rows.intersects(3..3), rows.covers(3..3)),
            ],
            [(true, false), (true, true), (false, false), (false, true)],
        );
    }

    #[test]
    fn a_difference_is_the_rows_outside_the_other_set() {
        assert_eq!(
            [
                pairs(set(&[(0, 10), (20, 30)]).difference(&set(&[(5, 25)]))),
                pairs(set(&[(3, 4), (8, 9)]).difference(&set(&[(0, 4)]))),
            ],
            [vec![(0, 5), (25, 30)], vec![(8, 9)]],
        );
    }
}
