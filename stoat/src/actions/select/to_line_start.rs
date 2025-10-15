//! Select to line start action implementation and tests.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Extend selection to start of line.
    pub fn select_to_line_start(&mut self, cx: &mut Context<Self>) {
        let selection = self.cursor.selection().clone();
        let cursor_pos = selection.cursor_position();
        let new_pos = text::Point::new(cursor_pos.row, 0);

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
    fn extends_to_line_start(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.select_to_line_start(cx);
            let sel = s.cursor.selection();
            assert!(!sel.is_empty());
        });
    }
}
