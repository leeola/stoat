//! Move cursor left command
//!
//! Moves the cursor one character to the left. At the beginning of a line, wraps to the end
//! of the previous line. This implements standard text editor left arrow behavior.

use crate::Stoat;
use gpui::App;
use text::Point;
use tracing::trace;

impl Stoat {
    /// Move cursor left by one character.
    ///
    /// Moves the cursor one position to the left in the buffer. At the beginning of a line,
    /// wraps to the end of the previous line if available.
    ///
    /// # Behavior
    ///
    /// - If cursor is mid-line: moves left by one column
    /// - If cursor is at line start: moves to end of previous line
    /// - If cursor is at start of buffer: no effect
    ///
    /// # Related
    ///
    /// See also [`crate::actions::movement::move_right`] for rightward movement.
    pub fn move_cursor_left(&mut self, cx: &App) {
        let current_pos = self.cursor_manager.position();

        if current_pos.column > 0 {
            let new_pos = Point::new(current_pos.row, current_pos.column - 1);
            trace!(from = ?current_pos, to = ?new_pos, "Moving cursor left within line");
            self.cursor_manager.move_to(new_pos);
        } else if current_pos.row > 0 {
            // Move to end of previous line
            let buffer_snapshot = self.buffer.read(cx).snapshot();
            let prev_row = current_pos.row - 1;
            let line_len = buffer_snapshot.line_len(prev_row);
            let new_pos = Point::new(prev_row, line_len);
            trace!(from = ?current_pos, to = ?new_pos, "Moving cursor left to end of previous line");
            self.cursor_manager.move_to(new_pos);
        } else {
            trace!(pos = ?current_pos, "At buffer start, cannot move left");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn move_left_within_line() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 3);
        s.input("h");
        s.assert_cursor_notation("he|llo");
    }

    #[test]
    fn move_left_at_line_start() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar");
        s.set_cursor(1, 0);
        s.input("h");
        s.assert_cursor_notation("foo|\nbar");
    }

    #[test]
    fn move_left_at_buffer_start() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 0);
        s.input("h");
        s.assert_cursor_notation("|hello");
    }
}
