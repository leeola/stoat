//! Move cursor to line start command
//!
//! Moves the cursor to the beginning of the current line. This implements standard text
//! editor Home key behavior.

use crate::Stoat;
use text::Point;
use tracing::debug;

impl Stoat {
    /// Move cursor to the start of the current line.
    ///
    /// Positions the cursor at column 0 of the current line, regardless of the current
    /// cursor position.
    ///
    /// # Behavior
    ///
    /// - Moves cursor to column 0 of current row
    /// - Resets goal column for vertical movement
    ///
    /// # Related
    ///
    /// See also [`crate::actions::movement::move_to_line_end`] for end-of-line movement.
    pub fn move_cursor_to_line_start(&mut self) {
        let current_pos = self.cursor_manager.position();
        let new_pos = Point::new(current_pos.row, 0);
        debug!(from = ?current_pos, to = ?new_pos, "Moving cursor to line start");
        self.cursor_manager.move_to(new_pos);
        self.ensure_cursor_visible();
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn move_to_line_start_from_middle() {
        let mut s = Stoat::test();
        s.set_text("hello world");
        s.set_cursor(0, 6);
        s.input("0");
        s.assert_cursor_notation("|hello world");
    }

    #[test]
    fn move_to_line_start_already_at_start() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 0);
        s.input("0");
        s.assert_cursor_notation("|hello");
    }

    #[test]
    fn move_to_line_start_multiline() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar\nbaz");
        s.set_cursor(1, 2);
        s.input("0");
        s.assert_cursor_notation("foo\n|bar\nbaz");
    }
}
