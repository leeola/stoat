use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Undo the last text edit, restoring buffer content and cursor positions.
    pub fn undo(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer().clone();

        let result = buffer.update(cx, |buf, _| buf.undo());
        let Some((tx_id, _)) = result else {
            return;
        };

        // Restore selections from before this transaction
        if let Some((before, _)) = self.selection_history.transaction(tx_id) {
            self.selections.select_anchors(before.clone());
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
    fn undo_restores_text(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            assert_eq!(
                s.active_buffer(cx).read(cx).buffer().read(cx).text(),
                "hello"
            );
            s.undo(cx);
            assert_eq!(s.active_buffer(cx).read(cx).buffer().read(cx).text(), "");
        });
    }

    #[gpui::test]
    fn undo_restores_cursor(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 5));
            s.undo(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 0));
        });
    }

    #[gpui::test]
    fn multi_undo(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            let buf = s.active_buffer(cx).read(cx).buffer().clone();
            s.insert_text("one", cx);
            buf.update(cx, |b, _| {
                b.finalize_last_transaction();
            });
            s.insert_text(" two", cx);
            buf.update(cx, |b, _| {
                b.finalize_last_transaction();
            });
            s.insert_text(" three", cx);
            assert_eq!(
                s.active_buffer(cx).read(cx).buffer().read(cx).text(),
                "one two three"
            );
            s.undo(cx);
            assert_eq!(
                s.active_buffer(cx).read(cx).buffer().read(cx).text(),
                "one two"
            );
            s.undo(cx);
            assert_eq!(s.active_buffer(cx).read(cx).buffer().read(cx).text(), "one");
            s.undo(cx);
            assert_eq!(s.active_buffer(cx).read(cx).buffer().read(cx).text(), "");
        });
    }

    #[gpui::test]
    fn undo_noop_on_empty(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.undo(cx);
            assert_eq!(s.active_buffer(cx).read(cx).buffer().read(cx).text(), "");
        });
    }
}
