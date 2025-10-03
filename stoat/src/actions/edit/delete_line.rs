//! Delete line command
//!
//! Deletes the entire current line including the newline character. This is commonly
//! bound to `dd` in vim-like editors.

use crate::Stoat;
use gpui::App;
use text::Point;
use tracing::debug;

impl Stoat {
    /// Delete the current line.
    ///
    /// Removes the entire line where the cursor is positioned, including the trailing
    /// newline (except for the last line). The cursor moves to the beginning of the line.
    ///
    /// # Behavior
    ///
    /// - For non-last lines: deletes from line start to next line start (includes newline)
    /// - For last line: deletes from line start to line end (no newline to delete)
    /// - Cursor moves to beginning of the line (or next line if not last)
    /// - Empty buffer remains empty
    ///
    /// # Implementation
    ///
    /// Uses the [`delete_range`] helper to handle the actual deletion and buffer re-parsing.
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::actions::edit::delete_to_end_of_line`] for partial line deletion
    /// - [`crate::actions::edit::delete_range`] for the underlying deletion mechanism
    pub fn delete_line(&mut self, cx: &mut App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let current_pos = self.cursor_manager.position();
        let line_start = Point::new(current_pos.row, 0);

        // Include newline if not last line
        let line_end = if current_pos.row < buffer_snapshot.row_count() - 1 {
            Point::new(current_pos.row + 1, 0)
        } else {
            // Last line - delete to end of line
            let line_len = buffer_snapshot.line_len(current_pos.row);
            Point::new(current_pos.row, line_len)
        };

        debug!(row = current_pos.row, from = ?line_start, to = ?line_end, "Deleting line");
        self.delete_range(line_start..line_end, cx);
        self.cursor_manager.move_to(line_start);
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn delete_line_middle() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar\nbaz");
        s.set_cursor(1, 1);
        s.input("dd");
        s.assert_cursor_notation("foo\n|baz");
    }

    #[test]
    fn delete_line_first() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar\nbaz");
        s.set_cursor(0, 2);
        s.input("dd");
        s.assert_cursor_notation("|bar\nbaz");
    }

    #[test]
    fn delete_line_last() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar\nbaz");
        s.set_cursor(2, 2);
        s.input("dd");
        s.assert_cursor_notation("foo\nbar\n|");
    }

    #[test]
    fn delete_line_single_line() {
        let mut s = Stoat::test();
        s.set_text("hello world");
        s.set_cursor(0, 5);
        s.input("dd");
        s.assert_cursor_notation("|");
    }

    #[test]
    fn delete_line_empty() {
        let mut s = Stoat::test();
        s.set_text("foo\n\nbar");
        s.set_cursor(1, 0);
        s.input("dd");
        s.assert_cursor_notation("foo\n|bar");
    }
}
