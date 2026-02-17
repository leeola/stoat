use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Redo the last undone app-state change.
    pub fn redo_state(&mut self, cx: &mut Context<Self>) {
        let current = self.capture_app_state();

        let Some(snapshot) = self.app_state_history.pop_redo() else {
            return;
        };

        self.app_state_history.push_undo(current);

        self.mode = snapshot.mode;
        self.key_context = snapshot.key_context;
        self.selections.select_anchors(snapshot.selections);
        self.select_next_state = snapshot.select_next_state;
        self.select_prev_state = snapshot.select_prev_state;
        self.scroll = snapshot.scroll;

        // Sync cursor to newest selection
        let buffer_item = self.active_buffer(cx);
        let snap = buffer_item.read(cx).buffer().read(cx).snapshot();
        let newest = self.selections.newest::<text::Point>(&snap);
        self.cursor.move_to(newest.head());

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}
