use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Outdent lines at cursor or within selection by removing one leading tab or spaces.
    pub fn outdent(&mut self, cx: &mut Context<Self>) {
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
        let mut rows: Vec<u32> = Vec::new();
        for selection in &selections {
            for row in selection.start.row..=selection.end.row {
                if !rows.contains(&row) {
                    rows.push(row);
                }
            }
        }
        rows.sort();
        rows.reverse();

        let mut edits: Vec<(std::ops::Range<usize>, &str)> = Vec::new();
        for row in &rows {
            let line_start = snapshot.point_to_offset(text::Point::new(*row, 0));
            let line_len = snapshot.line_len(*row);
            let line_text: String = snapshot
                .text_for_range(
                    line_start..snapshot.point_to_offset(text::Point::new(*row, line_len)),
                )
                .collect();

            let remove_len = if line_text.starts_with('\t') {
                1
            } else {
                let spaces = line_text.chars().take_while(|c| *c == ' ').count();
                spaces.min(4)
            };

            if remove_len > 0 {
                edits.push((line_start..line_start + remove_len, ""));
            }
        }

        if !edits.is_empty() {
            edits.reverse();
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
    fn removes_leading_tab(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("\thello", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.outdent(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hello");
        });
    }

    #[gpui::test]
    fn removes_leading_spaces(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("    hello", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.outdent(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hello");
        });
    }
}
