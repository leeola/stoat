//! Move to file start action implementation and tests.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Move cursor to the beginning of the file.
    pub fn move_to_file_start(&mut self, cx: &mut Context<Self>) {
        self.cursor.move_to(text::Point::new(0, 0));
        self.ensure_cursor_visible();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_row_zero_column_zero(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.move_to_file_start(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 0));
        });
    }
}
