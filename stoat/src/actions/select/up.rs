//! Select up action implementation and tests.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Extend selection up by one line.
    pub fn select_up(&mut self, cx: &mut Context<Self>) {
        let selection = self.cursor.selection().clone();
        let cursor_pos = selection.cursor_position();

        if cursor_pos.row > 0 {
            let target_row = cursor_pos.row - 1;
            let line_len = {
                let buffer_item = self.active_buffer(cx).read(cx);
                let buffer = buffer_item.buffer().read(cx);
                buffer.line_len(target_row)
            };

            let target_column = self.cursor.goal_column().min(line_len);
            let new_pos = text::Point::new(target_row, target_column);

            let new_selection = if selection.is_empty() {
                crate::cursor::Selection::new(cursor_pos, new_pos)
            } else {
                let anchor = selection.anchor_position();
                crate::cursor::Selection::new(anchor, new_pos)
            };

            self.cursor.set_selection(new_selection);
        }

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn extends_selection_up(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line1\nLine2", cx);
            s.set_cursor_position(text::Point::new(1, 0));
            s.select_up(cx);
            let sel = s.cursor.selection();
            assert!(!sel.is_empty());
        });
    }
}
