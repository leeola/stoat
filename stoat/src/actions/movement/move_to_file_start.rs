//! Move cursor to file start command
//!
//! Moves the cursor to the beginning of the buffer (first line, first column).
//! This implements common "go to top" behavior (often bound to gg or Ctrl+Home).

use crate::Stoat;
use text::Point;
use tracing::debug;

impl Stoat {
    /// Move cursor to the start of the file.
    ///
    /// Positions the cursor at the very beginning of the buffer (row 0, column 0),
    /// regardless of current position.
    ///
    /// # Behavior
    ///
    /// - Moves cursor to (0, 0)
    /// - Resets goal column for vertical movement
    /// - Works from any position in the buffer
    ///
    /// # Related
    ///
    /// See also [`crate::actions::movement::move_to_file_end`] for end-of-file movement.
    pub fn move_cursor_to_file_start(&mut self) {
        let current_pos = self.cursor_manager.position();
        let new_pos = Point::new(0, 0);
        debug!(from = ?current_pos, to = ?new_pos, "Moving cursor to file start");
        self.cursor_manager.move_to(new_pos);
        self.ensure_cursor_visible();
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn move_to_file_start_from_middle() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar\nbaz");
        s.set_cursor(1, 2);
        s.input("g g");
        s.assert_cursor_notation("|foo\nbar\nbaz");
    }

    #[test]
    fn move_to_file_start_from_end() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar\nbaz");
        s.set_cursor(2, 3);
        s.input("g g");
        s.assert_cursor_notation("|foo\nbar\nbaz");
    }

    #[test]
    fn move_to_file_start_already_at_start() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar");
        s.set_cursor(0, 0);
        s.input("g g");
        s.assert_cursor_notation("|foo\nbar");
    }
}
