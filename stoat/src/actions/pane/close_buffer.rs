use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};
use tracing::debug;

impl PaneGroupView {
    pub(crate) fn handle_close_buffer(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor().cloned() else {
            return;
        };

        let stoat = editor.read(cx).stoat.clone();
        let closed = stoat.update(cx, |s, cx| {
            let Some(active_id) = s.active_buffer_id else {
                return false;
            };

            // Find the next buffer to switch to from activation history (MRU order)
            let prev_id = s
                .buffer_store
                .read(cx)
                .buffer_ids_by_activation()
                .iter()
                .rev()
                .find(|&&id| id != active_id)
                .copied();

            let Some(next_id) = prev_id else {
                debug!("Cannot close last buffer");
                return false;
            };

            if let Err(e) = s.switch_to_buffer(next_id, cx) {
                tracing::error!("Failed to switch buffer after close: {e}");
                return false;
            }

            // Remove from open_buffers (strong refs)
            s.open_buffers
                .retain(|b| b.read(cx).buffer().read(cx).remote_id() != active_id);

            // Close in buffer store
            s.buffer_store.update(cx, |store, _cx| {
                store.close_buffer(active_id);
            });

            debug!(?active_id, ?next_id, "Buffer closed");
            true
        });

        if closed {
            cx.notify();
        }
    }
}
