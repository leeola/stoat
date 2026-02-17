use crate::{history::SelectionHistoryEntry, stoat::Stoat};
use gpui::Context;

impl Stoat {
    /// Redo the last undone selection change.
    pub fn redo_selection(&mut self, cx: &mut Context<Self>) {
        let current = SelectionHistoryEntry {
            selections: self.selections.disjoint_anchors_arc(),
            select_next_state: self.select_next_state.clone(),
            select_prev_state: self.select_prev_state.clone(),
        };

        let Some(entry) = self.selection_history.pop_selection_redo() else {
            return;
        };

        self.selection_history.push_selection_undo(current);

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
