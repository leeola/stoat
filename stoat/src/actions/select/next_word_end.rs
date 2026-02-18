use crate::{char_classifier::CharClassifier, stoat::Stoat};
use gpui::Context;
use text::Point;

impl Stoat {
    /// Extend all selections to the end of the current/next word.
    ///
    /// Each selection's head moves to the next word boundary while the anchor stays fixed.
    pub fn extend_next_word_end(&mut self, cx: &mut Context<Self>) {
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
                offset = CharClassifier::next_word_end(&buffer_snapshot, offset);
            }
            let new_head = buffer_snapshot.offset_to_point(offset);
            selection.set_head(new_head, text::SelectionGoal::None);
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
    fn extends_selection_to_word_end(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(Point::new(0, 0));
            s.extend_next_word_end(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), Point::new(0, 5));
            assert_eq!(selections[0].tail(), Point::new(0, 0));
        });
    }
}
