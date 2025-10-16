//! Select next symbol action implementation and tests.

use crate::Stoat;
use gpui::Context;
use std::ops::Range;
use text::ToOffset;

impl Stoat {
    /// Select the next symbol from the current cursor position.
    pub fn select_next_symbol(&mut self, cx: &mut Context<Self>) {
        let (buffer_snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer_snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (buffer_snapshot, token_snapshot)
        };

        let current_selection = self.cursor.selection();
        if !current_selection.is_empty() && current_selection.reversed {
            let start = current_selection.start;
            let end = current_selection.end;
            let selection = crate::cursor::Selection::new(start, end);
            self.cursor.set_selection(selection);
            cx.notify();
            return;
        }

        let cursor_pos = self.cursor.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut found_symbol: Option<Range<usize>> = None;

        while let Some(token) = token_cursor.item() {
            let token_start = token.range.start.to_offset(&buffer_snapshot);
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // Skip tokens that start at or before cursor (vim `w` behavior)
            if token_start <= cursor_offset {
                token_cursor.next();
                continue;
            }

            // Found first symbol after cursor
            if token.kind.is_symbol() {
                found_symbol = Some(token_start..token_end);
                break;
            }

            token_cursor.next();
        }

        if let Some(ref range) = found_symbol {
            let selection_start = buffer_snapshot.offset_to_point(range.start);
            let selection_end = buffer_snapshot.offset_to_point(range.end);
            let selection = crate::cursor::Selection::new(selection_start, selection_end);
            self.cursor.set_selection(selection);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn selects_next_word(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.select_next_symbol(cx);
            let sel = s.cursor.selection();
            // Vim `w` behavior: skip "hello", select "world"
            assert_eq!(sel.start, text::Point::new(0, 6));
            assert_eq!(sel.end, text::Point::new(0, 11));
        });
    }
}
