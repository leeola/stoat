use crate::{char_classifier::CharClassifier, stoat::Stoat};
use gpui::Context;

impl Stoat {
    /// Delete word after cursor.
    ///
    /// Deletes from cursor to end of current/next word group. Cursor stays in place.
    /// If no next word group, does nothing. Triggers reparse for syntax highlighting.
    pub fn delete_word_right(&mut self, cx: &mut Context<Self>) {
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
            let end_offset = CharClassifier::next_word_end(&buffer_snapshot, pos_offset);

            if end_offset > pos_offset {
                edits.push((pos_offset..end_offset, ""));
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
    fn deletes_next_word(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.delete_word_right(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, " world");
        });
    }

    #[gpui::test]
    fn cursor_stays_in_place(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(text::Point::new(0, 6));
            let pos_before = s.cursor.position();
            s.delete_word_right(cx);
            assert_eq!(s.cursor.position(), pos_before);
        });
    }

    #[gpui::test]
    fn no_op_at_end(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            s.delete_word_right(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hello");
        });
    }
}
