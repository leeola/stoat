use crate::{char_classifier::CharClassifier, stoat::Stoat};
use gpui::Context;
use text::Point;

impl Stoat {
    /// Move all cursors to end of current/next WORD (whitespace-delimited).
    ///
    /// Unlike [`move_word_end`](Self::move_word_end), punctuation is treated the same as
    /// word characters. Only whitespace separates WORDs.
    pub fn move_word_end_big(&mut self, cx: &mut Context<Self>) {
        self.record_selection_change();
        let count = self.take_count();
        let buffer_snapshot = {
            let buffer_item = self.active_buffer(cx).read(cx);
            buffer_item.buffer().read(cx).snapshot()
        };

        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<Point>(&buffer_snapshot);
            if newest_sel.head() != cursor_pos {
                let id = self.selections.next_id();
                self.selections.select(
                    vec![text::Selection {
                        id,
                        start: cursor_pos,
                        end: cursor_pos,
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    }],
                    &buffer_snapshot,
                );
            }
        }

        let mut selections = self.selections.all::<Point>(&buffer_snapshot);
        for selection in &mut selections {
            let mut offset = buffer_snapshot.point_to_offset(selection.head());
            for _ in 0..count {
                offset = CharClassifier::next_word_end_big(&buffer_snapshot, offset);
            }
            let new_pos = buffer_snapshot.offset_to_point(offset);
            selection.start = new_pos;
            selection.end = new_pos;
            selection.reversed = false;
            selection.goal = text::SelectionGoal::None;
        }

        self.selections.select(selections.clone(), &buffer_snapshot);
        if let Some(last) = selections.last() {
            self.cursor.move_to(last.head());
        }

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_past_punctuation(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello.world foo", cx);
            s.set_cursor_position(Point::new(0, 0));
            s.move_word_end_big(cx);
            // Should skip punctuation and go to end of "hello.world"
            assert_eq!(s.active_selections(cx)[0].head(), Point::new(0, 11));
        });
    }

    #[gpui::test]
    fn stops_at_whitespace(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(Point::new(0, 0));
            s.move_word_end_big(cx);
            assert_eq!(s.active_selections(cx)[0].head(), Point::new(0, 5));
        });
    }
}
