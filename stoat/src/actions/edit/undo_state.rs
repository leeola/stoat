use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Undo the last app-state change (mode, context, scroll, selections).
    pub fn undo_state(&mut self, cx: &mut Context<Self>) {
        let current = self.capture_app_state();

        let Some(snapshot) = self.app_state_history.pop_undo() else {
            return;
        };

        self.app_state_history.push_redo(current);

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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn undo_state_restores_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            assert_eq!(s.mode(), "normal");
            s.record_app_state();
            s.enter_insert_mode(cx);
            assert_eq!(s.mode(), "insert");
            s.undo_state(cx);
            assert_eq!(s.mode(), "normal");
        });
    }
}
