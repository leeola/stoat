use std::{cmp::Ordering, ops::Range};

#[derive(Default, Copy, Clone, Debug, PartialEq, Eq)]
pub enum SelectionGoal {
    #[default]
    None,
    Column(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
}
