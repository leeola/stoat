use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Set replace_pending flag; the next printable key will replace the char under each cursor.
    pub fn replace_char(&mut self, _cx: &mut Context<Self>) {
        self.replace_pending = true;
    }

    /// Replace the character under each cursor with the given character.
    pub fn replace_char_with(&mut self, ch: &str, cx: &mut Context<Self>) {
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
            if pos.column < line_len {
                let start = snapshot.point_to_offset(pos);
                let end = snapshot.point_to_offset(text::Point::new(pos.row, pos.column + 1));
                edits.push((start..end, ch));
            }
        }

        if !edits.is_empty() {
            // Save original positions (replace should not move cursor)
            let original_positions: Vec<_> = selections.iter().map(|s| s.head()).collect();
            buffer.update(cx, |buffer, _| buffer.edit(edits));
            let snapshot = buffer.read(cx).snapshot();
            let id_start = self.selections.next_id();
            let new_selections: Vec<_> = original_positions
                .iter()
                .enumerate()
                .map(|(i, &pos)| text::Selection {
                    id: id_start + i,
                    start: pos,
                    end: pos,
                    reversed: false,
                    goal: text::SelectionGoal::None,
                })
                .collect();
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
    fn replaces_char_under_cursor(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.replace_char_with("X", cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Xello");
            assert_eq!(s.cursor.position(), text::Point::new(0, 0));
        });
    }

    #[gpui::test]
    fn sets_replace_pending_flag(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            assert!(!s.replace_pending);
            s.replace_char(cx);
            assert!(s.replace_pending);
        });
    }
}
