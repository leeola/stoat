//! Move down action implementation and tests.
//!
//! This module implements the [`move_down`](crate::Stoat::move_down) action, which moves the
//! cursor down one line while preserving the goal column. Works symmetrically with
//! [`move_up`](crate::Stoat::move_up).

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Move cursor down one line.
    ///
    /// Moves the cursor to the next line while maintaining the goal column. If the target
    /// line is shorter than the goal column, the cursor is placed at the end of that line.
    ///
    /// # Behavior
    ///
    /// - Moves to next line (row + 1)
    /// - Preserves goal column from previous horizontal movements
    /// - Clamps column to line length if line is shorter
    /// - Ensures cursor remains visible in viewport
    /// - No-op if already at last line
    ///
    /// # Related Actions
    ///
    /// - [`move_up`](crate::Stoat::move_up) - Move up one line
    /// - [`page_down`](crate::Stoat::page_down) - Move down one page
    pub fn move_down(&mut self, cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        let max_row = self
            .active_buffer(cx)
            .read(cx)
            .buffer()
            .read(cx)
            .max_point()
            .row;

        if pos.row < max_row {
            let target_row = pos.row + 1;
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
    fn moves_down_one_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.move_down(cx);
            assert_eq!(s.cursor.position(), text::Point::new(1, 0));
        });
    }

    #[gpui::test]
    fn no_op_at_last_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2", cx);
            s.move_down(cx);
            let pos = s.cursor.position();
            assert_eq!(pos.row, 1);
        });
    }

    #[gpui::test]
    fn preserves_goal_column(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Very long line\nShort\nVery long line", cx);
            s.set_cursor_position(text::Point::new(0, 10));
            s.move_down(cx);
            // Should clamp to "Short" length (5)
            assert_eq!(s.cursor.position(), text::Point::new(1, 5));
            // Moving down again should return to column 10
            s.move_down(cx);
            assert_eq!(s.cursor.position(), text::Point::new(2, 10));
        });
    }
}
