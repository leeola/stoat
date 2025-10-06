//! Move cursor right command
//!
//! Moves the cursor one character to the right. At the end of a line, wraps to the beginning
//! of the next line. This implements standard text editor right arrow behavior.

use crate::Stoat;
use gpui::App;
use text::Point;
use tracing::trace;

impl Stoat {
    /// Move cursor right by one character.
    ///
    /// Moves the cursor one position to the right in the buffer. At the end of a line,
    /// wraps to the beginning of the next line if available.
    ///
    /// # Behavior
    ///
    /// - If cursor is mid-line: moves right by one column
    /// - If cursor is at line end: moves to start of next line
    /// - If cursor is at end of buffer: no effect
    ///
    /// # Related
    ///
    /// See also [`crate::actions::movement::move_left`] for leftward movement.
    pub fn move_cursor_right(&mut self, cx: &App) {
        let buffer_snapshot = self.buffer_snapshot(cx);
        let current_pos = self.cursor_manager.position();
        let line_len = buffer_snapshot.line_len(current_pos.row);

        if current_pos.column < line_len {
            let new_pos = Point::new(current_pos.row, current_pos.column + 1);
            trace!(from = ?current_pos, to = ?new_pos, "Moving cursor right within line");
            self.cursor_manager.move_to(new_pos);
        } else if current_pos.row < buffer_snapshot.row_count() - 1 {
            // Move to start of next line
            let new_pos = Point::new(current_pos.row + 1, 0);
            trace!(from = ?current_pos, to = ?new_pos, "Moving cursor right to start of next line");
            self.cursor_manager.move_to(new_pos);
            self.ensure_cursor_visible();
        } else {
            trace!(pos = ?current_pos, "At buffer end, cannot move right");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn move_right_within_line() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 2);
        s.command("MoveRight");
        s.assert_cursor_notation("hel|lo");
    }

    #[test]
    fn move_right_at_line_end() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar");
        s.set_cursor(0, 3);
        s.command("MoveRight");
        s.assert_cursor_notation("foo\n|bar");
    }

    #[test]
    fn move_right_at_buffer_end() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 5);
        s.command("MoveRight");
        s.assert_cursor_notation("hello|");
    }
}
