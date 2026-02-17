use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Open a new line below the current line and enter insert mode.
    pub fn open_line_below(&mut self, cx: &mut Context<Self>) {
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
        let mut edits = Vec::new();
        for selection in &selections {
            let pos = selection.head();
            let line_len = snapshot.line_len(pos.row);
            let line_end = snapshot.point_to_offset(text::Point::new(pos.row, line_len));
            edits.push((line_end..line_end, "\n"));
        }

        buffer.update(cx, |buffer, _| buffer.edit(edits));

        let snapshot = buffer.read(cx).snapshot();
        let mut new_selections = Vec::new();
        let id_start = self.selections.next_id();
        for (i, selection) in selections.iter().enumerate() {
            let new_pos = text::Point::new(selection.head().row + 1 + i as u32, 0);
            let clipped = snapshot.clip_point(new_pos, text::Bias::Left);
            new_selections.push(text::Selection {
                id: id_start + i,
                start: clipped,
                end: clipped,
                reversed: false,
                goal: text::SelectionGoal::None,
            });
        }

        self.selections.select(new_selections.clone(), &snapshot);
        if let Some(last) = new_selections.last() {
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

        self.set_mode_by_name("insert", cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn opens_line_below(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello\nworld", cx);
            s.set_cursor_position(text::Point::new(0, 3));
            s.set_mode_by_name("normal", cx);
            s.open_line_below(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hello\n\nworld");
            assert_eq!(s.cursor.position(), text::Point::new(1, 0));
            assert_eq!(s.mode(), "insert");
        });
    }
}
