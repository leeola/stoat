//! Delete to end of line command
//!
//! Deletes text from the cursor position to the end of the current line, preserving
//! the newline. This is commonly bound to `D` or `C-k` in vim-like editors.

use crate::Stoat;
use gpui::App;
use text::Point;
use tracing::debug;

impl Stoat {
    /// Delete from cursor to end of line.
    ///
    /// Removes all text from the cursor position to the end of the current line,
    /// preserving the newline character. The cursor stays at its current position.
    ///
    /// # Behavior
    ///
    /// - Deletes from cursor to end of line (exclusive of newline)
    /// - Cursor stays at current position
    /// - If cursor is already at end of line: no effect
    /// - Empty lines remain empty
    ///
    /// # Implementation
    ///
    /// Uses the [`delete_range`] helper to handle the actual deletion and buffer re-parsing.
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::actions::edit::delete_line`] for full line deletion
    /// - [`crate::actions::edit::delete_range`] for the underlying deletion mechanism
    pub fn delete_to_end_of_line(&mut self, cx: &mut App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let current_pos = self.cursor_manager.position();
        let line_len = buffer_snapshot.line_len(current_pos.row);
        let end = Point::new(current_pos.row, line_len);

        if current_pos.column < line_len {
            debug!(from = ?current_pos, to = ?end, "Delete to end of line");
            self.delete_range(current_pos..end, cx);
        } else {
            debug!(pos = ?current_pos, "Already at end of line, nothing to delete");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn delete_to_end_from_middle() {
        let mut s = Stoat::test();
        s.set_text("hello world");
        s.set_cursor(0, 6);
        s.input("D");
        s.assert_cursor_notation("hello |");
    }

    #[test]
    fn delete_to_end_from_start() {
        let mut s = Stoat::test();
        s.set_text("hello world");
        s.set_cursor(0, 0);
        s.input("D");
        s.assert_cursor_notation("|");
    }

    #[test]
    fn delete_to_end_at_end() {
        let mut s = Stoat::test();
        s.set_text("hello world");
        s.set_cursor(0, 11);
        s.input("D");
        s.assert_cursor_notation("hello world|");
    }

    #[test]
    fn delete_to_end_multiline() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar\nbaz");
        s.set_cursor(1, 1);
        s.input("D");
        s.assert_cursor_notation("foo\nb|\nbaz");
    }

    #[test]
    fn delete_to_end_preserves_newline() {
        let mut s = Stoat::test();
        s.set_text("hello\nworld");
        s.set_cursor(0, 2);
        s.input("D");
        s.assert_cursor_notation("he|\nworld");
    }
}
