use crate::{history::SelectionHistoryEntry, stoat::Stoat};
use gpui::Context;

impl Stoat {
    /// Undo the last selection change, restoring cursor positions and multi-cursor state.
    pub fn undo_selection(&mut self, cx: &mut Context<Self>) {
        // Save current state to redo stack before restoring
        let current = SelectionHistoryEntry {
            selections: self.selections.disjoint_anchors_arc(),
            select_next_state: self.select_next_state.clone(),
            select_prev_state: self.select_prev_state.clone(),
        };

        let Some(entry) = self.selection_history.pop_selection_undo() else {
            return;
        };

        self.selection_history.push_selection_redo(current);

        self.selections.select_anchors(entry.selections);
        self.select_next_state = entry.select_next_state;
        self.select_prev_state = entry.select_prev_state;

        // Sync cursor to newest selection
        let buffer_item = self.active_buffer(cx);
        let snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        let newest = self.selections.newest::<text::Point>(&snapshot);
        self.cursor.move_to(newest.head());

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn undo_selection_restores_position(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(text::Point::new(0, 5));
            let snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![text::Selection {
                    id,
                    start: text::Point::new(0, 5),
                    end: text::Point::new(0, 5),
                    reversed: false,
                    goal: text::SelectionGoal::None,
                }],
                &snapshot,
            );
            s.record_selection_change();
            s.move_right(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 6));

            s.undo_selection(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 5));
        });
    }
}
