//! Move word right action implementation and tests.

use crate::Stoat;
use gpui::Context;
use text::ToOffset;

impl Stoat {
    /// Move cursor right by one word (symbol).
    pub fn move_word_right(&mut self, cx: &mut Context<Self>) {
        let (buffer_snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer_snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (buffer_snapshot, token_snapshot)
        };

        let cursor_pos = self.cursor.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut found_symbol_end: Option<usize> = None;

        while let Some(token) = token_cursor.item() {
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            if token_end <= cursor_offset {
                token_cursor.next();
                continue;
            }

            if token.kind.is_symbol() {
                found_symbol_end = Some(token_end);
                break;
            }

            token_cursor.next();
        }

        if let Some(offset) = found_symbol_end {
            let new_pos = buffer_snapshot.offset_to_point(offset);
            self.cursor.move_to(new_pos);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_next_word(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.move_word_right(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 5));
        });
    }
}
