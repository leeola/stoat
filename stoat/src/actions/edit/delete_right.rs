//! Delete right command
//!
//! Deletes the character to the right of the cursor (delete key). At the end of a line,
//! merges with the next line.

use crate::Stoat;
use gpui::App;
use text::Point;

impl Stoat {
    /// Delete character to the right of cursor (delete).
    ///
    /// Removes the character immediately after the cursor. At the end of a line,
    /// removes the newline and merges with the next line.
    ///
    /// # Behavior
    ///
    /// - If mid-line: deletes next character
    /// - If at line end: merges with next line (removes newline)
    /// - If at buffer end: no effect
    /// - Cursor stays at current position
    ///
    /// # Implementation
    ///
    /// Uses the [`delete_range`] helper to handle the actual deletion and buffer re-parsing.
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::actions::edit::delete_left`] for backward delete
    /// - [`crate::actions::edit::delete_range`] for the underlying deletion mechanism
    pub fn delete_right(&mut self, cx: &mut App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let current_pos = self.cursor_manager.position();
        let line_len = buffer_snapshot.line_len(current_pos.row);

        if current_pos.column < line_len {
            // Delete character on same line
            let start = current_pos;
            let end = Point::new(current_pos.row, current_pos.column + 1);
            self.delete_range(start..end, cx);
        } else if current_pos.row < buffer_snapshot.row_count() - 1 {
            // Merge with next line
            let start = current_pos;
            let end = Point::new(current_pos.row + 1, 0);
            self.delete_range(start..end, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn delete_right_within_line() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 2);
        s.input("X");
        s.assert_cursor_notation("he|lo");
    }

    #[test]
    fn delete_right_at_line_end() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar");
        s.set_cursor(0, 3);
        s.input("X");
        s.assert_cursor_notation("foo|bar");
    }

    #[test]
    fn delete_right_at_buffer_end() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 5);
        s.input("X");
        s.assert_cursor_notation("hello|");
    }

    #[test]
    fn delete_right_multiple() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 0);
        s.input("XXX");
        s.assert_cursor_notation("|lo");
    }
}
