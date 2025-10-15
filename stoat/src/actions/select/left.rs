//! Select left action implementation and tests.

use crate::Stoat;
use gpui::Context;
use text::Bias;

impl Stoat {
    /// Extend selection left by one character.
    pub fn select_left(&mut self, cx: &mut Context<Self>) {
        let selection = self.cursor.selection().clone();
        let cursor_pos = selection.cursor_position();

        if cursor_pos.column > 0 {
            let target = text::Point::new(cursor_pos.row, cursor_pos.column - 1);
            let snapshot = self.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let new_pos = snapshot.clip_point(target, Bias::Left);

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
    fn extends_selection_left(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.select_left(cx);
            let sel = s.cursor.selection();
            assert!(!sel.is_empty());
        });
    }
}
