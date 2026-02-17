use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Indent lines at cursor or within selection by prepending a tab.
    pub fn indent(&mut self, cx: &mut Context<Self>) {
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

        // Collect unique rows from all selections
        let selections = self.selections.all::<text::Point>(&snapshot);
        let mut rows: Vec<u32> = Vec::new();
        for selection in &selections {
            let start_row = selection.start.row;
            let end_row = selection.end.row;
            for row in start_row..=end_row {
                if !rows.contains(&row) {
                    rows.push(row);
                }
            }
        }
        rows.sort();

        let mut edits: Vec<(std::ops::Range<usize>, &str)> = Vec::new();
        for row in &rows {
            let offset = snapshot.point_to_offset(text::Point::new(*row, 0));
            edits.push((offset..offset, "\t"));
        }

        if !edits.is_empty() {
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
    fn indents_current_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.indent(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "\thello");
        });
    }
}
