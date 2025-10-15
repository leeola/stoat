//! Move to line end action implementation and tests.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Move cursor to end of line.
    pub fn move_to_line_end(&mut self, cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        let line_len = self
            .active_buffer(cx)
            .read(cx)
            .buffer()
            .read(cx)
            .line_len(pos.row);
        self.cursor.move_to(text::Point::new(pos.row, line_len));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_end_of_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.move_to_line_end(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 5));
        });
    }
}
