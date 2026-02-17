use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Redo the last undone text edit, restoring buffer content and cursor positions.
    pub fn redo(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer().clone();

        let result = buffer.update(cx, |buf, _| buf.redo());
        let Some((tx_id, _)) = result else {
            return;
        };

        // Restore selections from after this transaction
        if let Some((_, Some(after))) = self.selection_history.transaction(tx_id) {
            self.selections.select_anchors(after.clone());
        }

        // Sync cursor to newest selection
        let snapshot = buffer.read(cx).snapshot();
        let newest = self.selections.newest::<text::Point>(&snapshot);
        self.cursor.move_to(newest.head());

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
    fn redo_restores_text(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            s.undo(cx);
            assert_eq!(s.active_buffer(cx).read(cx).buffer().read(cx).text(), "");
            s.redo(cx);
            assert_eq!(
                s.active_buffer(cx).read(cx).buffer().read(cx).text(),
                "hello"
            );
        });
    }

    #[gpui::test]
    fn undo_redo_round_trip(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            let buf = s.active_buffer(cx).read(cx).buffer().clone();
            s.insert_text("abc", cx);
            buf.update(cx, |b, _| {
                b.finalize_last_transaction();
            });
            s.insert_text("def", cx);
            s.undo(cx);
            s.undo(cx);
            s.redo(cx);
            assert_eq!(s.active_buffer(cx).read(cx).buffer().read(cx).text(), "abc");
            s.redo(cx);
            assert_eq!(
                s.active_buffer(cx).read(cx).buffer().read(cx).text(),
                "abcdef"
            );
        });
    }

    #[gpui::test]
    fn redo_noop_on_empty(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.redo(cx);
            assert_eq!(s.active_buffer(cx).read(cx).buffer().read(cx).text(), "");
        });
    }
}
