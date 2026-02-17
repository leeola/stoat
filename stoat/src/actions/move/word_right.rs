use crate::{char_classifier::CharClassifier, stoat::Stoat};
use gpui::Context;
use text::Point;

impl Stoat {
    /// Move all cursors right by one word.
    ///
    /// Each cursor moves independently to the end of the current/next word group and collapses
    /// any existing selections. Uses character classification for word boundary detection,
    /// working on both code and plain text files.
    pub fn move_word_right(&mut self, cx: &mut Context<Self>) {
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
            let mut cursor_offset = buffer_snapshot.point_to_offset(selection.head());
            for _ in 0..count {
                cursor_offset = CharClassifier::next_word_end(&buffer_snapshot, cursor_offset);
            }
            let new_pos = buffer_snapshot.offset_to_point(cursor_offset);
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
    fn moves_to_next_word(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.move_word_right(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(0, 5));
        });
    }

    #[gpui::test]
    fn moves_multiple_cursors_independently(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world\nfoo bar", cx);

            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(0, 0),
                        end: text::Point::new(0, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 0),
                        end: text::Point::new(1, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            s.move_word_right(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].head(), text::Point::new(0, 5));
            assert_eq!(selections[1].head(), text::Point::new(1, 3));
        });
    }

    #[gpui::test]
    fn moves_across_punctuation(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello, world", cx);
            s.set_cursor_position(text::Point::new(0, 0));

            s.move_word_right(cx);
            assert_eq!(s.active_selections(cx)[0].head(), text::Point::new(0, 5));

            s.move_word_right(cx);
            assert_eq!(s.active_selections(cx)[0].head(), text::Point::new(0, 6));

            s.move_word_right(cx);
            assert_eq!(s.active_selections(cx)[0].head(), text::Point::new(0, 12));
        });
    }
}
