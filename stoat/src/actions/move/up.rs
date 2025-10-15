//! Move up action implementation and tests.
//!
//! This module implements the [`move_up`](crate::Stoat::move_up) action, which moves the cursor
//! up one line while preserving the goal column. The goal column is maintained across vertical
//! movements, allowing navigation through lines of varying lengths while staying in the desired
//! horizontal position.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Move cursor up one line.
    ///
    /// Moves the cursor to the previous line while maintaining the goal column. If the target
    /// line is shorter than the goal column, the cursor is placed at the end of that line.
    ///
    /// # Behavior
    ///
    /// - Moves to previous line (row - 1)
    /// - Preserves goal column from previous horizontal movements
    /// - Clamps column to line length if line is shorter
    /// - Ensures cursor remains visible in viewport
    /// - No-op if already at first line
    ///
    /// # Related Actions
    ///
    /// - [`move_down`](crate::Stoat::move_down) - Move down one line
    /// - [`page_up`](crate::Stoat::page_up) - Move up one page
    pub fn move_up(&mut self, cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        if pos.row > 0 {
            let target_row = pos.row - 1;
            let line_len = self
                .active_buffer(cx)
                .read(cx)
                .buffer()
                .read(cx)
                .line_len(target_row);
            let target_column = self.cursor.goal_column().min(line_len);
            self.cursor
                .move_to_with_goal(text::Point::new(target_row, target_column));
            self.ensure_cursor_visible();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_up_one_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.set_cursor_position(text::Point::new(2, 0));
            s.move_up(cx);
            assert_eq!(s.cursor.position(), text::Point::new(1, 0));
        });
    }

    #[gpui::test]
    fn no_op_at_first_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1", cx);
            s.set_cursor_position(text::Point::new(0, 3));
            s.move_up(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 3));
        });
    }

    #[gpui::test]
    fn preserves_goal_column(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Short\nVery long line\nShort", cx);
            s.set_cursor_position(text::Point::new(1, 10));
            s.move_up(cx);
            // Should clamp to "Short" length (5)
            assert_eq!(s.cursor.position(), text::Point::new(0, 5));
            // But moving down should return to column 10
            s.move_down(cx);
            assert_eq!(s.cursor.position(), text::Point::new(1, 10));
        });
    }
}
