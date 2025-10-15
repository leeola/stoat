//! Select to line end action implementation and tests.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Extend selection to end of line.
    pub fn select_to_line_end(&mut self, cx: &mut Context<Self>) {
        let selection = self.cursor.selection().clone();
        let cursor_pos = selection.cursor_position();

        let line_len = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer = buffer_item.buffer().read(cx);
            buffer.line_len(cursor_pos.row)
        };

        let new_pos = text::Point::new(cursor_pos.row, line_len);

        let new_selection = if selection.is_empty() {
            crate::cursor::Selection::new(cursor_pos, new_pos)
        } else {
            let anchor = selection.anchor_position();
            crate::cursor::Selection::new(anchor, new_pos)
        };

        self.cursor.set_selection(new_selection);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn extends_to_line_end(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.select_to_line_end(cx);
            let sel = s.cursor.selection();
            assert!(!sel.is_empty());
        });
    }
}
