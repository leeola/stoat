//! Move cursor to file end command
//!
//! Moves the cursor to the end of the buffer (last line, end of line).
//! This implements common "go to bottom" behavior (often bound to G or Ctrl+End).

use crate::Stoat;
use gpui::App;
use text::Point;
use tracing::debug;

impl Stoat {
    /// Move cursor to the end of the file.
    ///
    /// Positions the cursor at the very end of the buffer, after the last character of
    /// the last line.
    ///
    /// # Behavior
    ///
    /// - Moves cursor to last row, end of line
    /// - Position is after the last character
    /// - Resets goal column for vertical movement
    ///
    /// # Related
    ///
    /// See also [`crate::actions::movement::move_to_file_start`] for start-of-file movement.
    pub fn move_cursor_to_file_end(&mut self, cx: &App) {
        let current_pos = self.cursor_manager.position();
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let last_row = buffer_snapshot.row_count().saturating_sub(1);
        let last_line_len = buffer_snapshot.line_len(last_row);
        let new_pos = Point::new(last_row, last_line_len);
        debug!(from = ?current_pos, to = ?new_pos, "Moving cursor to file end");
        self.cursor_manager.move_to(new_pos);
        self.ensure_cursor_visible();
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn move_to_file_end_from_middle() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar\nbaz");
        s.set_cursor(1, 1);
        s.input("G");
        s.assert_cursor_notation("foo\nbar\nbaz|");
    }

    #[test]
    fn move_to_file_end_from_start() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar\nbaz");
        s.set_cursor(0, 0);
        s.input("G");
        s.assert_cursor_notation("foo\nbar\nbaz|");
    }

    #[test]
    fn move_to_file_end_already_at_end() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar");
        s.set_cursor(1, 3);
        s.input("G");
        s.assert_cursor_notation("foo\nbar|");
    }

    #[test]
    fn move_to_file_end_single_line() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 2);
        s.input("G");
        s.assert_cursor_notation("hello|");
    }
}
