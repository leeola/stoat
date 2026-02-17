use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Delete the text within each selection and enter normal mode.
    pub fn delete_selection(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer().clone();

        let before_selections = self.selections.disjoint_anchors_arc();
        buffer.update(cx, |buf, _| buf.start_transaction());
        let snapshot = buffer.read(cx).snapshot();

        let selections = self.selections.all::<text::Point>(&snapshot);
        let mut edits = Vec::new();
        for selection in &selections {
            if !selection.is_empty() {
                let start = snapshot.point_to_offset(selection.start);
                let end = snapshot.point_to_offset(selection.end);
                edits.push((start..end, ""));
            }
        }

        if !edits.is_empty() {
            buffer.update(cx, |buffer, _| buffer.edit(edits));
            let snapshot = buffer.read(cx).snapshot();
            let updated = self.selections.all::<text::Point>(&snapshot);
            if let Some(first) = updated.first() {
                self.cursor.move_to(first.head());
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

        self.set_mode_by_name("normal", cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn deletes_selected_text(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
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
            s.delete_selection(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, " world");
            assert_eq!(s.mode(), "normal");
        });
    }
}
