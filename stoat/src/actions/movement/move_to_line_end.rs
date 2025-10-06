//! Move cursor to line end command
//!
//! Moves the cursor to the end of the current line. This implements standard text
//! editor End key behavior.

use crate::Stoat;
use gpui::App;
use text::Point;
use tracing::debug;

impl Stoat {
    /// Move cursor to the end of the current line.
    ///
    /// Positions the cursor after the last character of the current line, allowing text
    /// to be appended.
    ///
    /// # Behavior
    ///
    /// - Moves cursor to the end of current row
    /// - Position is after the last character (at line_len)
    /// - Resets goal column for vertical movement
    ///
    /// # Related
    ///
    /// See also [`crate::actions::movement::move_to_line_start`] for start-of-line movement.
    pub fn move_cursor_to_line_end(&mut self, cx: &App) {
        let buffer_snapshot = self.buffer_snapshot(cx);
        let current_pos = self.cursor_manager.position();
        let line_len = buffer_snapshot.line_len(current_pos.row);
        let new_pos = Point::new(current_pos.row, line_len);
        debug!(from = ?current_pos, to = ?new_pos, "Moving cursor to line end");
        self.cursor_manager.move_to(new_pos);
        self.ensure_cursor_visible();
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn move_to_line_end_from_middle() {
        let mut s = Stoat::test();
        s.set_text("hello world");
        s.set_cursor(0, 3);
        s.command("MoveToLineEnd");
        s.assert_cursor_notation("hello world|");
    }

    #[test]
    fn move_to_line_end_already_at_end() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 5);
        s.command("MoveToLineEnd");
        s.assert_cursor_notation("hello|");
    }

    #[test]
    fn move_to_line_end_multiline() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar\nbaz");
        s.set_cursor(1, 1);
        s.command("MoveToLineEnd");
        s.assert_cursor_notation("foo\nbar|\nbaz");
    }

    #[test]
    fn move_to_line_end_empty_line() {
        let mut s = Stoat::test();
        s.set_text("foo\n\nbar");
        s.set_cursor(1, 0);
        s.command("MoveToLineEnd");
        s.assert_cursor_notation("foo\n|\nbar");
    }
}
