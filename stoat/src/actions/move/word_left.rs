//! Move word left action implementation and tests.

use crate::Stoat;
use gpui::Context;
use text::ToOffset;

impl Stoat {
    /// Move cursor left by one word (symbol).
    pub fn move_word_left(&mut self, cx: &mut Context<Self>) {
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

        let mut prev_symbol_start: Option<usize> = None;

        while let Some(token) = token_cursor.item() {
            let token_start = token.range.start.to_offset(&buffer_snapshot);
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            if token_start >= cursor_offset {
                break;
            }

            if token.kind.is_symbol() {
                if token_start < cursor_offset && cursor_offset <= token_end {
                    prev_symbol_start = Some(token_start);
                    break;
                }

                if token_end < cursor_offset {
                    prev_symbol_start = Some(token_start);
                }
            }

            token_cursor.next();
        }

        if let Some(offset) = prev_symbol_start {
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
    fn moves_to_previous_word(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.move_word_left(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 6));
        });
    }
}
