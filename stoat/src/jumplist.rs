//! Per-editor jumplist tracking byte offsets the user marked
//! with `SaveSelection`. `JumpBackward` and `JumpForward` walk
//! through the recorded positions; recording a new position
//! truncates anything ahead of the current cursor.

#[derive(Debug, Default, Clone)]
pub(crate) struct JumpList {
    positions: Vec<usize>,
    cursor: usize,
}

impl JumpList {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn save(&mut self, pos: usize) {
        self.positions.truncate(self.cursor);
        self.positions.push(pos);
        self.cursor = self.positions.len();
    }

    pub(crate) fn backward(&mut self) -> Option<usize> {
        if self.cursor == 0 {
            return None;
        }
        self.cursor -= 1;
        Some(self.positions[self.cursor])
    }

    pub(crate) fn forward(&mut self) -> Option<usize> {
        if self.cursor + 1 >= self.positions.len() {
            return None;
        }
        self.cursor += 1;
        Some(self.positions[self.cursor])
    }
}

#[cfg(test)]
mod tests {
    use super::JumpList;

    #[test]
    fn save_pushes_and_resets_cursor_past_end() {
        let mut j = JumpList::new();
        j.save(10);
        j.save(20);
        assert_eq!(j.positions, vec![10, 20]);
        assert_eq!(j.cursor, 2);
    }

    #[test]
    fn backward_walks_through_history() {
        let mut j = JumpList::new();
        j.save(5);
        j.save(10);
        j.save(15);
        assert_eq!(j.backward(), Some(15));
        assert_eq!(j.backward(), Some(10));
        assert_eq!(j.backward(), Some(5));
        assert_eq!(j.backward(), None);
    }

    #[test]
    fn forward_walks_back_after_backward() {
        let mut j = JumpList::new();
        j.save(5);
        j.save(10);
        j.save(15);
        assert_eq!(j.backward(), Some(15));
        assert_eq!(j.backward(), Some(10));
        assert_eq!(j.forward(), Some(15));
        assert_eq!(j.forward(), None);
    }

    #[test]
    fn save_truncates_forward_history() {
        let mut j = JumpList::new();
        j.save(5);
        j.save(10);
        j.save(15);
        assert_eq!(j.backward(), Some(15));
        assert_eq!(j.backward(), Some(10));
        // cursor is now at index 1 (pointing at 10). save() truncates
        // [..1] (keeping just [5]), then pushes 99.
        j.save(99);
        assert_eq!(j.positions, vec![5, 99]);
        assert_eq!(j.forward(), None);
    }

    #[test]
    fn empty_jumplist_navigation_is_noop() {
        let mut j = JumpList::new();
        assert_eq!(j.backward(), None);
        assert_eq!(j.forward(), None);
    }
}
