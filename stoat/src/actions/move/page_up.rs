//! Page up action implementation and tests.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Move cursor up by one page (approximately one viewport height).
    pub fn page_up(&mut self, cx: &mut Context<Self>) {
        let lines_per_page = self.viewport_lines.unwrap_or(30.0).floor() as u32;

        if lines_per_page == 0 {
            return;
        }

        let current_pos = self.cursor.position();
        let new_row = current_pos.row.saturating_sub(lines_per_page);

        let buffer_snapshot = {
            let buffer_item = self.active_buffer(cx).read(cx);
            buffer_item.buffer().read(cx).snapshot()
        };

        let line_len = buffer_snapshot.line_len(new_row);
        let new_column = self.cursor.goal_column().min(line_len);
        let new_pos = text::Point::new(new_row, new_column);

        self.cursor.move_to_with_goal(new_pos);

        let target_scroll_y = new_row.saturating_sub(3) as f32;
        self.scroll
            .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_up_one_page(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            let mut lines = vec![];
            for i in 0..50 {
                lines.push(format!("Line {}", i));
            }
            s.insert_text(&lines.join("\n"), cx);
            s.set_cursor_position(text::Point::new(40, 0));
            s.page_up(cx);
            assert_eq!(s.cursor.position().row, 10); // 40 - 30
        });
    }
}
