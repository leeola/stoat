//! Move cursor down command
//!
//! Moves the cursor down by one line while maintaining the goal column position.
//! This implements standard text editor down arrow behavior with column memory.

use crate::Stoat;
use gpui::App;
use text::Point;
use tracing::trace;

impl Stoat {
    /// Move cursor down by one line.
    ///
    /// Moves the cursor down by one line, attempting to maintain the horizontal (column)
    /// position. Uses a "goal column" to remember the desired column across short lines.
    ///
    /// # Behavior
    ///
    /// - Moves down one row
    /// - Maintains goal column when possible
    /// - Clamps to line length if line is shorter than goal column
    /// - No effect if already at last line
    ///
    /// # Goal Column
    ///
    /// The cursor manager tracks a goal column that persists across vertical movements,
    /// allowing the cursor to return to its original column when moving through shorter lines.
    ///
    /// # Related
    ///
    /// See also [`crate::actions::movement::move_up`] for upward movement.
    pub fn move_cursor_down(&mut self, cx: &App) {
        let buffer_snapshot = self.buffer_snapshot(cx);
        let max_row = buffer_snapshot.row_count() - 1;
        let current_pos = self.cursor_manager.position();

        if current_pos.row < max_row {
            let new_row = current_pos.row + 1;
            let line_len = buffer_snapshot.line_len(new_row);
            let new_column = self.cursor_manager.goal_column().min(line_len);
            let new_pos = Point::new(new_row, new_column);
            trace!(from = ?current_pos, to = ?new_pos, goal_column = self.cursor_manager.goal_column(), "Moving cursor down");
            self.cursor_manager.move_to_with_goal(new_pos);
            self.ensure_cursor_visible();
        } else {
            trace!(pos = ?current_pos, "At last line, cannot move down");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn move_down_basic() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar");
        s.set_cursor(0, 1);
        s.input("j");
        s.assert_cursor_notation("foo\nb|ar");
    }

    #[test]
    fn move_down_at_last_line() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar");
        s.set_cursor(1, 1);
        s.input("j");
        s.assert_cursor_notation("foo\nb|ar");
    }

    #[test]
    fn move_down_maintains_goal_column() {
        let mut s = Stoat::test();
        s.set_text("hello\nx\nworld");
        s.set_cursor(0, 3);
        s.input("j");
        s.assert_cursor_notation("hello\nx|\nworld");
        s.input("j");
        s.assert_cursor_notation("hello\nx\nwor|ld");
    }

    #[test]
    fn move_down_to_shorter_line() {
        let mut s = Stoat::test();
        s.set_text("abcdef\nab");
        s.set_cursor(0, 5);
        s.input("j");
        s.assert_cursor_notation("abcdef\nab|");
    }
}
