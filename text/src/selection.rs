use crate::rope::Rope;
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, ops::Range};

#[derive(Default, Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SelectionGoal {
    #[default]
    None,
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
    /// returning a selection whose ends stay at least one character apart.
    ///
    /// Without `extend` the result is the one-character block at `target`,
    /// discarding the old selection. At the rope end, where there is no next
    /// character, that block covers the previous character instead. With
    /// `extend` the tail is held and the head moves to `target`, and when the
    /// head crosses the tail the tail steps one character so the range never
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

    /// Widen an empty selection to cover one character, leaving any non-empty
    /// selection untouched, so the block cursor always has a cell.
    ///
    /// An empty selection widens its head forward over the next character, or
    /// backward over the previous one at the rope end where there is no next
    /// character. The vertical-movement goal is preserved.
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
/// anchor`) draws its block cursor one character back from the head, on the
/// last selected cell rather than the boundary past it. Collapsed and reversed
/// selections place the cursor on the head, so `head` is returned unchanged.
pub fn cursor_offset(rope: &Rope, anchor: usize, head: usize) -> usize {
    if head > anchor {
        rope.reversed_chars_at(head)
            .next()
            .map(|ch| head - ch.len_utf8())
            .unwrap_or(head)
    } else {
        head
    }
}

/// Offset one character past `offset`, or `offset` itself at the rope end.
///
/// Forward mirror of the back-step in [`cursor_offset`]. A forward selection
/// whose block cursor should sit on the character at `offset` stores its head
/// here, one cell past it, so [`cursor_offset`] recovers that character.
pub fn next_char_boundary(rope: &Rope, offset: usize) -> usize {
    rope.chars_at(offset)
        .next()
        .map(|ch| offset + ch.len_utf8())
        .unwrap_or(offset)
}

/// Offset one character before `offset`, or `offset` itself at the rope start.
///
/// Backward mirror of [`next_char_boundary`], stepping by a whole character
/// the way [`cursor_offset`] does when a forward selection's block cursor sits
/// one cell back from the head.
pub fn prev_char_boundary(rope: &Rope, offset: usize) -> usize {
    rope.reversed_chars_at(offset)
        .next()
        .map(|ch| offset - ch.len_utf8())
        .unwrap_or(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

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
