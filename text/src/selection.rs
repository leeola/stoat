use crate::rope::Rope;
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, ops::Range};

/// The column a vertical motion remembers so `j`/`k` keep the cursor at one
/// column as it crosses lines.
#[derive(Default, Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SelectionGoal {
    #[default]
    None,
    /// A display-cell column, counting each character as the cells it is drawn
    /// in, one for most, two for a wide glyph, and as many as the next tab stop
    /// for a tab.
    ///
    /// Measured along the whole buffer line rather than along a display row, so
    /// a vertical motion lands at the same place whatever soft wrap or folds do
    /// to the rows in between.
    Column(u32),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection<T> {
    pub id: usize,
    pub start: T,
    pub end: T,
    pub reversed: bool,
    pub goal: SelectionGoal,
}

impl<T: Clone> Selection<T> {
    pub fn head(&self) -> T {
        if self.reversed {
            self.start.clone()
        } else {
            self.end.clone()
        }
    }

    pub fn tail(&self) -> T {
        if self.reversed {
            self.end.clone()
        } else {
            self.start.clone()
        }
    }

    pub fn map<F, S>(&self, f: F) -> Selection<S>
    where
        F: Fn(T) -> S,
    {
        Selection {
            id: self.id,
            start: f(self.start.clone()),
            end: f(self.end.clone()),
            reversed: self.reversed,
            goal: self.goal,
        }
    }

    pub fn collapse_to(&mut self, point: T, new_goal: SelectionGoal) {
        self.start = point.clone();
        self.end = point;
        self.goal = new_goal;
        self.reversed = false;
    }
}

impl<T: PartialEq> Selection<T> {
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

impl<T: Copy> Selection<T> {
    pub fn range(&self) -> Range<T> {
        self.start..self.end
    }
}

impl<T: Copy + Ord> Selection<T> {
    pub fn set_head(&mut self, head: T, new_goal: SelectionGoal) {
        if head.cmp(&self.tail()) < Ordering::Equal {
            if !self.reversed {
                self.end = self.start;
                self.reversed = true;
            }
            self.start = head;
        } else {
            if self.reversed {
                self.start = self.end;
                self.reversed = false;
            }
            self.end = head;
        }
        self.goal = new_goal;
    }

    pub fn set_tail(&mut self, tail: T, new_goal: SelectionGoal) {
        if tail.cmp(&self.head()) <= Ordering::Equal {
            if self.reversed {
                self.end = self.start;
                self.reversed = false;
            }
            self.start = tail;
        } else {
            if !self.reversed {
                self.start = self.end;
                self.reversed = true;
            }
            self.end = tail;
        }
        self.goal = new_goal;
    }
}

impl Selection<usize> {
    /// Move the block cursor to `target` under the 1-width-cursor model,
    /// returning a selection whose ends stay at least one grapheme cluster
    /// apart.
    ///
    /// Without `extend` the result is the one-cluster block at `target`,
    /// discarding the old selection. At the rope end, where there is no next
    /// cluster, that block covers the previous cluster instead. With `extend`
    /// the tail is held and the head moves to `target`, and when the head
    /// crosses the tail the tail steps one cluster so the range never
    /// collapses onto the anchor.
    ///
    /// The vertical-movement goal is reset, since this is a horizontal move.
    pub fn put_cursor(&self, rope: &Rope, target: usize, extend: bool) -> Selection<usize> {
        if !extend {
            let point = Selection {
                id: self.id,
                start: target,
                end: target,
                reversed: false,
                goal: SelectionGoal::None,
            };
            return point.min_width_1(rope);
        }

        let anchor = self.tail();
        let head = self.head();
        let anchor = if head >= anchor && target < anchor {
            next_char_boundary(rope, anchor)
        } else if head < anchor && target >= anchor {
            prev_char_boundary(rope, anchor)
        } else {
            anchor
        };

        let (start, end, reversed) = if anchor <= target {
            (anchor, next_char_boundary(rope, target), false)
        } else {
            (target, anchor, true)
        };

        Selection {
            id: self.id,
            start,
            end,
            reversed,
            goal: SelectionGoal::None,
        }
    }

    /// Widen an empty selection to cover one grapheme cluster, leaving any
    /// non-empty selection untouched, so the block cursor always has a cell.
    ///
    /// An empty selection widens its head forward over the next cluster, or
    /// backward over the previous one at the rope end where there is no next
    /// cluster. The vertical-movement goal is preserved.
    pub fn min_width_1(&self, rope: &Rope) -> Selection<usize> {
        if !self.is_empty() {
            return self.clone();
        }

        let offset = self.start;
        let forward = next_char_boundary(rope, offset);
        let (start, end) = if forward > offset {
            (offset, forward)
        } else {
            (prev_char_boundary(rope, offset), offset)
        };

        Selection {
            id: self.id,
            start,
            end,
            reversed: false,
            goal: self.goal,
        }
    }
}

/// Returns the offset of the block-cursor cell for a selection spanning
/// `anchor` to `head`.
///
/// Under Helix's 1-width cursor convention a forward selection (`head >
/// anchor`) draws its block cursor one grapheme cluster back from the head, on
/// the last selected cell rather than the boundary past it. Collapsed and
/// reversed selections place the cursor on the head, so `head` is returned
/// unchanged.
///
/// Stepping by cluster rather than by scalar is what keeps the cursor off the
/// inside of a decomposed accent or an emoji sequence, which no consumer of
/// this convention could render as a cell.
pub fn cursor_offset(rope: &Rope, anchor: usize, head: usize) -> usize {
    if head > anchor {
        rope.prev_grapheme_boundary(head)
    } else {
        head
    }
}

/// [`cursor_offset`] for a whole set of `(anchor, head)` pairs.
///
/// Answers pair for pair, in the order given. Only the forward selections need
/// a cluster stepped back, and those steps are taken in one walk of the rope
/// rather than a descent from the root apiece, which is what makes painting a
/// few hundred cursors cost one traversal instead of hundreds.
pub fn cursor_offsets(rope: &Rope, selections: &[(usize, usize)]) -> Vec<usize> {
    let mut cursors: Vec<usize> = selections.iter().map(|&(_, head)| head).collect();

    let stepping: Vec<usize> = selections
        .iter()
        .enumerate()
        .filter(|&(_, &(anchor, head))| head > anchor)
        .map(|(i, _)| i)
        .collect();
    if stepping.is_empty() {
        return cursors;
    }

    let heads: Vec<usize> = stepping.iter().map(|&i| cursors[i]).collect();
    for (&i, boundary) in stepping
        .iter()
        .zip(rope.prev_grapheme_boundaries_batch(&heads))
    {
        cursors[i] = boundary;
    }
    cursors
}

/// Offset one grapheme cluster past `offset`, or `offset` itself at the rope
/// end.
///
/// Forward mirror of the back-step in [`cursor_offset`]. A forward selection
/// whose block cursor should sit on the cluster at `offset` stores its head
/// here, one cell past it, so [`cursor_offset`] recovers that cluster.
pub fn next_char_boundary(rope: &Rope, offset: usize) -> usize {
    rope.next_grapheme_boundary(offset)
}

/// Offset one grapheme cluster before `offset`, or `offset` itself at the rope
/// start.
///
/// Backward mirror of [`next_char_boundary`], stepping by a whole cluster the
/// way [`cursor_offset`] does when a forward selection's block cursor sits one
/// cell back from the head.
pub fn prev_char_boundary(rope: &Rope, offset: usize) -> usize {
    rope.prev_grapheme_boundary(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A father-mother-daughter ZWJ sequence. Three 4-byte emoji joined by two
    /// 3-byte zero-width joiners, so 18 bytes rendering as one cell.
    const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";

    fn sel(start: i32, end: i32, reversed: bool) -> Selection<i32> {
        Selection {
            id: 7,
            start,
            end,
            reversed,
            goal: SelectionGoal::None,
        }
    }

    fn usel(start: usize, end: usize, reversed: bool) -> Selection<usize> {
        Selection {
            id: 7,
            start,
            end,
            reversed,
            goal: SelectionGoal::None,
        }
    }

    #[test]
    fn head_of_forward_selection_returns_end() {
        assert_eq!(sel(1, 5, false).head(), 5);
    }

    #[test]
    fn head_of_reversed_selection_returns_start() {
        assert_eq!(sel(1, 5, true).head(), 1);
    }

    #[test]
    fn tail_is_opposite_of_head() {
        let s = sel(1, 5, false);
        assert_eq!(s.tail(), 1);
        let s = sel(1, 5, true);
        assert_eq!(s.tail(), 5);
    }

    #[test]
    fn is_empty_when_start_equals_end() {
        assert!(sel(3, 3, false).is_empty());
        assert!(!sel(3, 4, false).is_empty());
    }

    #[test]
    fn set_head_flips_reversed_when_crossing_tail() {
        let mut s = sel(5, 10, false);
        s.set_head(2, SelectionGoal::Column(2));
        assert_eq!(
            s,
            Selection {
                id: 7,
                start: 2,
                end: 5,
                reversed: true,
                goal: SelectionGoal::Column(2),
            }
        );
    }

    #[test]
    fn set_tail_flips_reversed_when_crossing_head() {
        let mut s = sel(5, 10, false);
        s.set_tail(15, SelectionGoal::None);
        assert_eq!(
            s,
            Selection {
                id: 7,
                start: 10,
                end: 15,
                reversed: true,
                goal: SelectionGoal::None,
            }
        );
    }

    #[test]
    fn collapse_to_resets_reversed_and_sets_goal() {
        let mut s = sel(1, 5, true);
        s.collapse_to(3, SelectionGoal::Column(9));
        assert_eq!(
            s,
            Selection {
                id: 7,
                start: 3,
                end: 3,
                reversed: false,
                goal: SelectionGoal::Column(9),
            }
        );
    }

    #[test]
    fn map_preserves_id_and_goal() {
        let s = Selection {
            id: 42,
            start: 1,
            end: 5,
            reversed: true,
            goal: SelectionGoal::Column(11),
        };
        let mapped: Selection<String> = s.map(|x| x.to_string());
        assert_eq!(
            mapped,
            Selection {
                id: 42,
                start: "1".into(),
                end: "5".into(),
                reversed: true,
                goal: SelectionGoal::Column(11),
            }
        );
    }

    #[test]
    fn range_returns_start_to_end() {
        assert_eq!(sel(2, 7, false).range(), 2..7);
        assert_eq!(sel(2, 7, true).range(), 2..7);
    }

    #[test]
    fn cursor_offset_is_one_char_back_when_forward_else_head() {
        assert_eq!(cursor_offset(&Rope::from("abcd"), 0, 4), 3);
        assert_eq!(cursor_offset(&Rope::from("café"), 0, 5), 3);
        assert_eq!(cursor_offset(&Rope::from("abcd"), 3, 3), 3);
        assert_eq!(cursor_offset(&Rope::from("abcd"), 5, 1), 1);
    }

    #[test]
    fn cursor_offset_lands_on_a_cluster_start_not_inside_it() {
        let rope = Rope::from(format!("a{FAMILY}b").as_str());
        assert_eq!(
            cursor_offset(&rope, 0, 19),
            1,
            "a forward selection ending past the family lands on its first byte",
        );
        assert_eq!(
            cursor_offset(&rope, 0, 20),
            19,
            "the cell after the family is the plain b",
        );
    }

    #[test]
    fn cursor_offsets_answers_pair_for_pair_like_the_scalar() {
        // Mixed on purpose. Only the forward pairs step back, so the batch has
        // to put its answers on those and leave the collapsed and reversed ones
        // holding their heads, all while the two sets interleave.
        let rope = Rope::from(format!("a{FAMILY}b cafe\u{301} xyz").as_str());
        let pairs = [(0, 19), (19, 19), (25, 20), (0, 20), (28, 25), (21, 27)];

        assert_eq!(
            cursor_offsets(&rope, &pairs),
            pairs
                .iter()
                .map(|&(anchor, head)| cursor_offset(&rope, anchor, head))
                .collect::<Vec<_>>(),
            "the batch matches the scalar on every pair",
        );
        assert_eq!(
            cursor_offsets(&rope, &[]),
            Vec::<usize>::new(),
            "no selections is no work",
        );
    }

    #[test]
    fn boundaries_round_trip_over_a_decomposed_accent() {
        let rope = Rope::from("e\u{301}x");
        assert_eq!(
            next_char_boundary(&rope, 0),
            3,
            "e and its mark step as one"
        );
        assert_eq!(prev_char_boundary(&rope, 3), 0);
        assert_eq!(next_char_boundary(&rope, 3), 4);
        assert_eq!(prev_char_boundary(&rope, 4), 3);
    }

    #[test]
    fn min_width_1_covers_a_whole_cluster() {
        let rope = Rope::from(FAMILY);
        assert_eq!(
            usel(0, 0, false).min_width_1(&rope),
            usel(0, 18, false),
            "the block cursor covers the whole sequence, not its first scalar",
        );
        assert_eq!(
            usel(18, 18, false).min_width_1(&rope),
            usel(0, 18, false),
            "at the rope end it widens backward over the same cluster",
        );
    }

    #[test]
    fn put_cursor_without_extend_is_one_char_block_at_target() {
        let r = Rope::from("abcdef");
        assert_eq!(
            usel(0, 0, false).put_cursor(&r, 2, false),
            usel(2, 3, false)
        );
        assert_eq!(
            usel(1, 4, false).put_cursor(&r, 0, false),
            usel(0, 1, false)
        );
    }

    #[test]
    fn put_cursor_without_extend_covers_prev_char_at_eof() {
        assert_eq!(
            usel(1, 1, false).put_cursor(&Rope::from("abcd"), 4, false),
            usel(3, 4, false)
        );
        assert_eq!(
            usel(0, 0, false).put_cursor(&Rope::from("café"), 3, false),
            usel(3, 5, false)
        );
    }

    #[test]
    fn put_cursor_extend_moves_head_and_widens_forward() {
        let r = Rope::from("abcdefgh");
        assert_eq!(usel(0, 0, false).put_cursor(&r, 0, true), usel(0, 1, false));
        assert_eq!(usel(0, 0, false).put_cursor(&r, 2, true), usel(0, 3, false));
        assert_eq!(usel(2, 8, false).put_cursor(&r, 4, true), usel(2, 5, false));
    }

    #[test]
    fn put_cursor_extend_shifts_anchor_when_head_crosses() {
        let r = Rope::from("abcdefgh");
        assert_eq!(usel(5, 5, false).put_cursor(&r, 2, true), usel(2, 6, true));
        assert_eq!(usel(3, 6, false).put_cursor(&r, 0, true), usel(0, 4, true));
        assert_eq!(usel(3, 6, true).put_cursor(&r, 6, true), usel(5, 7, false));
    }

    #[test]
    fn min_width_1_widens_empty_forward() {
        let r = Rope::from("abcd");
        assert_eq!(usel(2, 2, false).min_width_1(&r), usel(2, 3, false));
        assert_eq!(usel(0, 0, false).min_width_1(&r), usel(0, 1, false));
    }

    #[test]
    fn min_width_1_widens_backward_at_eof() {
        assert_eq!(
            usel(4, 4, false).min_width_1(&Rope::from("abcd")),
            usel(3, 4, false)
        );
        assert_eq!(
            usel(5, 5, false).min_width_1(&Rope::from("café")),
            usel(3, 5, false)
        );
    }

    #[test]
    fn min_width_1_leaves_non_empty_unchanged() {
        let r = Rope::from("abcd");
        assert_eq!(usel(1, 3, false).min_width_1(&r), usel(1, 3, false));
        assert_eq!(usel(1, 3, true).min_width_1(&r), usel(1, 3, true));
    }
}
