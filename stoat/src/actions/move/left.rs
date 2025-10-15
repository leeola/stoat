//! Move left action implementation and tests.

use crate::Stoat;
use gpui::Context;
use text::Bias;

impl Stoat {
    /// Move cursor left one character.
    ///
    /// Moves left by one character, correctly handling multi-byte UTF-8 characters by clipping
    /// to the nearest character boundary.
    ///
    /// # Related Actions
    ///
    /// - [`move_right`](crate::Stoat::move_right) - Move right one character
    /// - [`move_word_left`](crate::Stoat::move_word_left) - Move left one word
    pub fn move_left(&mut self, cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        if pos.column > 0 {
            let target = text::Point::new(pos.row, pos.column - 1);
            let snapshot = self.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let clipped = snapshot.clip_point(target, Bias::Left);
            self.cursor.move_to(clipped);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_left_one_character(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.move_left(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 4));
        });
    }

    #[gpui::test]
    fn no_op_at_start_of_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hi", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.move_left(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 0));
        });
    }
}
