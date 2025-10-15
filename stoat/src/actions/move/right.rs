//! Move right action implementation and tests.

use crate::Stoat;
use gpui::Context;
use text::Bias;

impl Stoat {
    /// Move cursor right one character.
    pub fn move_right(&mut self, cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        let line_len = self
            .active_buffer(cx)
            .read(cx)
            .buffer()
            .read(cx)
            .line_len(pos.row);

        if pos.column < line_len {
            let target = text::Point::new(pos.row, pos.column + 1);
            let snapshot = self.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let clipped = snapshot.clip_point(target, Bias::Right);
            self.cursor.move_to(clipped);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_right_one_character(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.move_right(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 1));
        });
    }

    #[gpui::test]
    fn no_op_at_end_of_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hi", cx);
            s.move_right(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 2));
        });
    }
}
