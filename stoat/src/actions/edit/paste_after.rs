use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Paste clipboard contents after the cursor position.
    pub fn paste_after(&mut self, cx: &mut Context<Self>) {
        let clipboard_text = match cx.read_from_clipboard() {
            Some(item) => match item.text() {
                Some(t) => t,
                None => return,
            },
            None => return,
        };

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
            let insert_col = (pos.column + 1).min(line_len);
            let insert_offset = snapshot.point_to_offset(text::Point::new(pos.row, insert_col));
            edits.push((insert_offset..insert_offset, clipboard_text.as_str()));
        }

        let text_len = clipboard_text.len();
        buffer.update(cx, |buffer, _| buffer.edit(edits));

        let snapshot = buffer.read(cx).snapshot();
        let mut new_selections = Vec::new();
        let id_start = self.selections.next_id();
        for (i, selection) in selections.iter().enumerate() {
            let pos = selection.head();
            let insert_col = (pos.column + 1).min(snapshot.line_len(pos.row));
            let base_offset = snapshot.point_to_offset(text::Point::new(pos.row, insert_col));
            let new_offset = base_offset + i * text_len + text_len - 1;
            let new_offset = new_offset.min(snapshot.len());
            let new_pos = snapshot.offset_to_point(new_offset);
            new_selections.push(text::Selection {
                id: id_start + i,
                start: new_pos,
                end: new_pos,
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
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn paste_after_is_noop_without_clipboard(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            s.set_cursor_position(text::Point::new(0, 2));
            s.paste_after(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hello");
        });
    }
}
