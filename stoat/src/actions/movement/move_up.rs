//! Move cursor up command
//!
//! Moves the cursor up by one line while maintaining the goal column position.
//! This implements standard text editor up arrow behavior with column memory.

use crate::Stoat;
use gpui::App;
use text::Point;
use tracing::trace;

impl Stoat {
    /// Move cursor up by one line.
    ///
    /// Moves the cursor up by one line, attempting to maintain the horizontal (column)
    /// position. Uses a "goal column" to remember the desired column across short lines.
    ///
    /// # Behavior
    ///
    /// - Moves up one row
    /// - Maintains goal column when possible
    /// - Clamps to line length if line is shorter than goal column
    /// - No effect if already at first line
    ///
    /// # Goal Column
    ///
    /// The cursor manager tracks a goal column that persists across vertical movements,
    /// allowing the cursor to return to its original column when moving through shorter lines.
    ///
    /// # Related
    ///
    /// See also [`crate::actions::movement::move_down`] for downward movement.
    pub fn move_cursor_up(&mut self, cx: &App) {
        let current_pos = self.cursor_manager.position();
        if current_pos.row > 0 {
            let buffer_snapshot = self.buffer.read(cx).snapshot();
            let new_row = current_pos.row - 1;
            let line_len = buffer_snapshot.line_len(new_row);
            let new_column = self.cursor_manager.goal_column().min(line_len);
            let new_pos = Point::new(new_row, new_column);
            trace!(from = ?current_pos, to = ?new_pos, goal_column = self.cursor_manager.goal_column(), "Moving cursor up");
            self.cursor_manager.move_to_with_goal(new_pos);
        } else {
            trace!(pos = ?current_pos, "At first line, cannot move up");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn move_up_basic() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar");
        s.set_cursor(1, 1);
        s.input("k");
        s.assert_cursor_notation("f|oo\nbar");
    }

    #[test]
    fn move_up_at_first_line() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar");
        s.set_cursor(0, 1);
        s.input("k");
        s.assert_cursor_notation("f|oo\nbar");
    }

    #[test]
    fn move_up_maintains_goal_column() {
        let mut s = Stoat::test();
        s.set_text("hello\nx\nworld");
        s.set_cursor(2, 3);
        s.input("k");
        s.assert_cursor_notation("hello\nx|\nworld");
        s.input("k");
        s.assert_cursor_notation("hel|lo\nx\nworld");
    }

    #[test]
    fn move_up_to_shorter_line() {
        let mut s = Stoat::test();
        s.set_text("ab\nabcdef");
        s.set_cursor(1, 5);
        s.input("k");
        s.assert_cursor_notation("ab|\nabcdef");
    }
}
