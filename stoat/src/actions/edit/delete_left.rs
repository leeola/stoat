//! Delete left command
//!
//! Deletes the character to the left of the cursor (backspace). At the beginning of a line,
//! merges with the previous line.

use crate::Stoat;
use gpui::App;
use text::Point;

impl Stoat {
    /// Delete character to the left of cursor (backspace).
    ///
    /// Removes the character immediately before the cursor. At the beginning of a line,
    /// removes the newline and merges with the previous line.
    ///
    /// # Behavior
    ///
    /// - If mid-line: deletes previous character
    /// - If at line start: merges with previous line (removes newline)
    /// - If at buffer start: no effect
    /// - Cursor moves to deletion point
    ///
    /// # Implementation
    ///
    /// Uses the [`delete_range`] helper to handle the actual deletion and buffer re-parsing.
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::actions::edit::delete_right`] for forward delete
    /// - [`crate::actions::edit::delete_range`] for the underlying deletion mechanism
    pub fn delete_left(&mut self, cx: &mut App) {
        let current_pos = self.cursor_manager.position();
        if current_pos.column > 0 {
            // Delete character on same line
            let start = Point::new(current_pos.row, current_pos.column - 1);
            let end = current_pos;
            self.delete_range(start..end, cx);
            self.cursor_manager.move_to(start);
        } else if current_pos.row > 0 {
            // Merge with previous line
            let buffer_snapshot = self.buffer.read(cx).snapshot();
            let prev_row = current_pos.row - 1;
            let prev_line_len = buffer_snapshot.line_len(prev_row);
            let start = Point::new(prev_row, prev_line_len);
            let end = Point::new(current_pos.row, 0);

            self.delete_range(start..end, cx);
            self.cursor_manager.move_to(start);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn delete_left_within_line() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 3);
        s.input("x");
        s.assert_cursor_notation("he|lo");
    }

    #[test]
    fn delete_left_at_line_start() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar");
        s.set_cursor(1, 0);
        s.input("x");
        s.assert_cursor_notation("foo|bar");
    }

    #[test]
    fn delete_left_at_buffer_start() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 0);
        s.input("x");
        s.assert_cursor_notation("|hello");
    }

    #[test]
    fn delete_left_multiple() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 5);
        s.input("xxx");
        s.assert_cursor_notation("he|");
    }
}
