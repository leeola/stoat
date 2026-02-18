use crate::{char_classifier::CharClassifier, stoat::Stoat};
use gpui::Context;
use text::Point;

impl Stoat {
    /// Move all cursors to the start of the next WORD (whitespace-delimited).
    ///
    /// Unlike [`move_next_word_start`](Self::move_next_word_start) which respects character
    /// class boundaries, this treats all non-whitespace as one word class.
    pub fn move_next_long_word_start(&mut self, cx: &mut Context<Self>) {
        self.record_selection_change();
        let count = self.take_count();
        let snapshot = {
            let buffer_item = self.active_buffer(cx).read(cx);
            buffer_item.buffer().read(cx).snapshot()
        };

        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<Point>(&snapshot);
            let should_reset = if self.is_mode_anchored() {
                newest_sel.head() != cursor_pos
            } else {
                !newest_sel.is_empty() || newest_sel.head() != cursor_pos
            };

            if should_reset {
                let id = self.selections.next_id();
                self.selections.select(
                    vec![text::Selection {
                        id,
                        start: cursor_pos,
                        end: cursor_pos,
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    }],
                    &snapshot,
                );
            }
        }

        let mut selections = self.selections.all::<Point>(&snapshot);
        for selection in &mut selections {
            for _ in 0..count {
                let cursor_offset = snapshot.point_to_offset(selection.head());
                let target = CharClassifier::next_word_start_big(&snapshot, cursor_offset);
                let target_point = snapshot.offset_to_point(target);

                if self.is_mode_anchored() {
                    selection.set_head(target_point, text::SelectionGoal::None);
                } else {
                    let head = selection.head();
                    selection.start = head;
                    selection.end = target_point;
                    selection.reversed = false;
                }
            }
        }

        self.selections.select(selections.clone(), &snapshot);
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
    fn selects_to_next_word_start_big(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("foo.bar baz", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.move_next_long_word_start(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].tail(), text::Point::new(0, 0));
            assert_eq!(selections[0].head(), text::Point::new(0, 8));
        });
    }

    #[gpui::test]
    fn extends_multiple_selections_independently(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("foo bar\nbaz qux", cx);

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

            s.move_next_long_word_start(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].tail(), text::Point::new(0, 0));
            assert_eq!(selections[0].head(), text::Point::new(0, 4));
            assert_eq!(selections[1].tail(), text::Point::new(1, 0));
            assert_eq!(selections[1].head(), text::Point::new(1, 4));
        });
    }
}
