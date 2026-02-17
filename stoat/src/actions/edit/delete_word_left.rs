use crate::{char_classifier::CharClassifier, stoat::Stoat};
use gpui::Context;

impl Stoat {
    /// Delete word before cursor.
    ///
    /// Deletes from start of current/previous word group to cursor. Cursor moves to
    /// deletion start. If no previous word group, does nothing. Triggers reparse for
    /// syntax highlighting.
    pub fn delete_word_left(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer().clone();

        let before_selections = self.selections.disjoint_anchors_arc();
        buffer.update(cx, |buf, _| {
            buf.start_transaction();
        });
        let buffer_snapshot = buffer.read(cx).snapshot();

        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<text::Point>(&buffer_snapshot);
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

        let selections = self.selections.all::<text::Point>(&buffer_snapshot);
        let mut edits = Vec::new();

        for selection in &selections {
            let pos_offset = buffer_snapshot.point_to_offset(selection.head());
            let start_offset = CharClassifier::previous_word_start(&buffer_snapshot, pos_offset);

            if start_offset < pos_offset {
                edits.push((start_offset..pos_offset, ""));
            }
        }

        if !edits.is_empty() {
            buffer.update(cx, |buffer, _| {
                buffer.edit(edits);
            });

            let snapshot = buffer.read(cx).snapshot();
            let updated_selections = self.selections.all::<text::Point>(&snapshot);

            if let Some(last) = updated_selections.last() {
                self.cursor.move_to(last.head());
            }

            let tx = buffer.update(cx, |buf, _| buf.end_transaction());
            if let Some((tx_id, _)) = tx {
                self.selection_history
                    .insert_transaction(tx_id, before_selections);
                self.selection_history
                    .set_after_selections(tx_id, self.selections.disjoint_anchors_arc());
            }

            buffer_item.update(cx, |item, cx| {
                let _ = item.reparse(cx);
            });

            cx.emit(crate::stoat::StoatEvent::Changed);
        } else {
            buffer.update(cx, |buf, _| {
                buf.end_transaction();
            });
        }

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn deletes_previous_word(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.delete_word_left(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hello ");
        });
    }

    #[gpui::test]
    fn deletes_to_word_start_when_mid_word(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            s.set_cursor_position(text::Point::new(0, 3));
            s.delete_word_left(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "lo");
        });
    }

    #[gpui::test]
    fn no_op_at_start(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.delete_word_left(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hello");
        });
    }
}
