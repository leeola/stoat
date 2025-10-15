//! Move to line start action implementation and tests.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Move cursor to start of line (column 0).
    pub fn move_to_line_start(&mut self, _cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        self.cursor.move_to(text::Point::new(pos.row, 0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_column_zero(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello World", cx);
            s.move_to_line_start(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 0));
        });
    }
}
