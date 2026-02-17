use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Convert selected text to lowercase. In normal mode with collapsed cursor, operates on char
    /// under cursor.
    pub fn lowercase(&mut self, cx: &mut Context<Self>) {
        self.case_transform(cx, |s| s.to_lowercase());
    }

    pub(crate) fn case_transform(
        &mut self,
        cx: &mut Context<Self>,
        transform: impl Fn(&str) -> String,
    ) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer().clone();

        let before_selections = self.selections.disjoint_anchors_arc();
        buffer.update(cx, |buf, _| buf.start_transaction());
        let snapshot = buffer.read(cx).snapshot();

        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<text::Point>(&snapshot);
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
                    &snapshot,
                );
            }
        }

        let selections = self.selections.all::<text::Point>(&snapshot);
        let mut owned_edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();

        for selection in &selections {
            let (start_offset, end_offset) = if selection.is_empty() {
                let pos = selection.head();
                let line_len = snapshot.line_len(pos.row);
                if pos.column >= line_len {
                    continue;
                }
                (
                    snapshot.point_to_offset(pos),
                    snapshot.point_to_offset(text::Point::new(pos.row, pos.column + 1)),
                )
            } else {
                (
                    snapshot.point_to_offset(selection.start),
                    snapshot.point_to_offset(selection.end),
                )
            };

            let text: String = snapshot.text_for_range(start_offset..end_offset).collect();
            let transformed = transform(&text);
            if transformed != text {
                owned_edits.push((start_offset..end_offset, transformed));
            }
        }

        if !owned_edits.is_empty() {
            let edits: Vec<(std::ops::Range<usize>, &str)> = owned_edits
                .iter()
                .map(|(range, s)| (range.clone(), s.as_str()))
                .collect();
            buffer.update(cx, |buffer, _| buffer.edit(edits));
            let snapshot = buffer.read(cx).snapshot();
            let updated = self.selections.all::<text::Point>(&snapshot);
            if let Some(last) = updated.last() {
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
            self.send_did_change_notification(cx);
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
    fn lowercases_selection(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("HELLO", cx);
            let snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![text::Selection {
                    id,
                    start: text::Point::new(0, 0),
                    end: text::Point::new(0, 5),
                    reversed: false,
                    goal: text::SelectionGoal::None,
                }],
                &snapshot,
            );
            s.lowercase(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hello");
        });
    }

    #[gpui::test]
    fn lowercases_char_under_cursor(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("HELLO", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.lowercase(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hELLO");
        });
    }
}
