//! Move cursor up by one page command
//!
//! Moves the cursor up by approximately one viewport height and initiates an animated
//! scroll to keep the cursor visible. This implements standard PageUp key behavior.

use crate::Stoat;
use gpui::App;
use text::Point;
use tracing::debug;

impl Stoat {
    /// Move cursor up by one page (approximately one viewport height).
    ///
    /// Moves the cursor up by the visible line count and animates the viewport to follow.
    /// The page size is determined by the current viewport dimensions.
    ///
    /// # Behavior
    ///
    /// - Moves up by `viewport_lines` rows (defaults to 30 if not set)
    /// - Maintains goal column across the movement
    /// - Clamps to line length if target line is shorter
    /// - Initiates animated scroll to keep cursor visible
    /// - No effect if already at first line
    ///
    /// # Scroll Animation
    ///
    /// The viewport animates smoothly to position the cursor approximately 3 lines from
    /// the top, providing context while avoiding the very top edge.
    ///
    /// # Related
    ///
    /// See also [`crate::actions::movement::page_down`] for downward page movement.
    pub fn move_cursor_page_up(&mut self, cx: &App) {
        let lines_per_page = self.viewport_lines.unwrap_or(30.0).floor() as u32;

        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let current_pos = self.cursor_manager.position();

        if lines_per_page > 0 {
            let new_row = current_pos.row.saturating_sub(lines_per_page);
            let line_len = buffer_snapshot.line_len(new_row);
            let new_column = self.cursor_manager.goal_column().min(line_len);
            let new_pos = Point::new(new_row, new_column);
            let target_scroll_y = new_row.saturating_sub(3) as f32;
            debug!(from = ?current_pos, to = ?new_pos, lines_per_page, scroll_y = target_scroll_y, "Page up");
            self.cursor_manager.move_to_with_goal(new_pos);

            // Start animated scroll to keep the cursor visible
            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn page_up_basic() {
        let mut s = Stoat::test();
        // Create 40 lines of text
        let lines: Vec<String> = (0..40).map(|i| format!("line {}", i)).collect();
        s.set_text(&lines.join("\n"));

        // Set viewport to 10 lines
        s.resize_lines(10.0);

        // Start at line 20
        s.set_cursor(20, 0);
        s.input("pageup"); // PageUp key

        // Should move up by 10 lines
        let (row, col) = s.cursor();
        assert_eq!(row, 10);
        assert_eq!(col, 0);
    }

    #[test]
    fn page_up_near_start() {
        let mut s = Stoat::test();
        s.set_text("line 0\nline 1\nline 2\nline 3\nline 4");
        s.resize_lines(10.0);
        s.set_cursor(2, 0);
        s.input("pageup"); // PageUp key

        // Should clamp to start
        s.assert_cursor_notation("|line 0\nline 1\nline 2\nline 3\nline 4");
    }

    #[test]
    fn page_up_at_start() {
        let mut s = Stoat::test();
        s.set_text("line 0\nline 1\nline 2");
        s.resize_lines(10.0);
        s.set_cursor(0, 0);
        s.input("pageup"); // PageUp key

        // Should stay at start
        s.assert_cursor_notation("|line 0\nline 1\nline 2");
    }
}
