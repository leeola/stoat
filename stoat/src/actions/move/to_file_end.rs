//! Move to file end action implementation and tests.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Move cursor to the end of the file.
    pub fn move_to_file_end(&mut self, cx: &mut Context<Self>) {
        let buffer_snapshot = {
            let buffer_item = self.active_buffer(cx).read(cx);
            buffer_item.buffer().read(cx).snapshot()
        };

        let last_row = buffer_snapshot.row_count().saturating_sub(1);
        let last_line_len = buffer_snapshot.line_len(last_row);
        let new_pos = text::Point::new(last_row, last_line_len);

        self.cursor.move_to(new_pos);
        self.ensure_cursor_visible();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_last_position(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.move_to_file_end(cx);
            assert_eq!(s.cursor.position(), text::Point::new(2, 6));
        });
    }
}
