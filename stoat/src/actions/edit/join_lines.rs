use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Join current line with the next line(s), respecting count prefix.
    pub fn join_lines(&mut self, cx: &mut Context<Self>) {
        let count = self.take_count();
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer().clone();

        let before_selections = self.selections.disjoint_anchors_arc();
        buffer.update(cx, |buf, _| buf.start_transaction());

        for _ in 0..count {
            let snapshot = buffer.read(cx).snapshot();
            let cursor_pos = self.cursor.position();
            let max_row = snapshot.max_point().row;

            if cursor_pos.row >= max_row {
                break;
            }

            let line_len = snapshot.line_len(cursor_pos.row);
            let join_start = snapshot.point_to_offset(text::Point::new(cursor_pos.row, line_len));

            // Find end of leading whitespace on next line
            let next_line_start = snapshot.point_to_offset(text::Point::new(cursor_pos.row + 1, 0));
            let next_line_len = snapshot.line_len(cursor_pos.row + 1);
            let next_line_text: String = snapshot
                .text_for_range(
                    next_line_start
                        ..snapshot
                            .point_to_offset(text::Point::new(cursor_pos.row + 1, next_line_len)),
                )
                .collect();
            let ws_len = next_line_text.len() - next_line_text.trim_start().len();
            let join_end = next_line_start + ws_len;

            buffer.update(cx, |buffer, _| {
                buffer.edit([(join_start..join_end, " ")]);
            });

            // Update cursor to join point
            let new_pos = text::Point::new(cursor_pos.row, line_len);
            self.cursor.move_to(new_pos);
            let snapshot = buffer.read(cx).snapshot();
            let id = self.selections.next_id();
            self.selections.select(
                vec![text::Selection {
                    id,
                    start: new_pos,
                    end: new_pos,
                    reversed: false,
                    goal: text::SelectionGoal::None,
                }],
                &snapshot,
            );
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
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn joins_two_lines(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello\nworld", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.join_lines(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hello world");
        });
    }

    #[gpui::test]
    fn strips_leading_whitespace(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello\n    world", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.join_lines(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hello world");
        });
    }

    #[gpui::test]
    fn no_op_on_last_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.join_lines(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hello");
        });
    }
}
