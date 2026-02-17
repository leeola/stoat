use crate::{char_classifier::CharClassifier, stoat::Stoat};
use gpui::Context;
use text::Point;

impl Stoat {
    /// Extend all selections to the previous word.
    ///
    /// Each selection extends independently by finding the previous word from its head position.
    /// In anchored mode, extends the selection. In non-anchored mode, selects just the word.
    pub fn select_prev_symbol(&mut self, cx: &mut Context<Self>) {
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
            if !selection.is_empty() && !selection.reversed {
                let start = selection.start;
                let end = selection.end;
                selection.start = end;
                selection.end = start;
                selection.reversed = true;
                continue;
            }

            let cursor_offset = snapshot.point_to_offset(selection.head());

            if let Some(range) = CharClassifier::prev_word_range(&snapshot, cursor_offset) {
                if self.is_mode_anchored() {
                    let selection_start = snapshot.offset_to_point(range.start);
                    selection.set_head(selection_start, text::SelectionGoal::None);
                } else {
                    let selection_start = snapshot.offset_to_point(range.start);
                    let selection_end = snapshot.offset_to_point(range.end);
                    selection.start = selection_start;
                    selection.end = selection_end;
                    selection.reversed = true;
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
    fn selects_previous_word(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.select_prev_symbol(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(0, 6));
            assert_eq!(selections[0].tail(), text::Point::new(0, 11));
        });
    }

    #[gpui::test]
    fn extends_multiple_selections_independently(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world\nfoo bar", cx);

            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(0, 11),
                        end: text::Point::new(0, 11),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 7),
                        end: text::Point::new(1, 7),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            s.select_prev_symbol(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].head(), text::Point::new(0, 6));
            assert_eq!(selections[0].tail(), text::Point::new(0, 11));
            assert_eq!(selections[1].head(), text::Point::new(1, 4));
            assert_eq!(selections[1].tail(), text::Point::new(1, 7));
        });
    }
}
