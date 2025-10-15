//! Buffer finder dismiss action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Dismiss buffer finder.
    ///
    /// Clears all buffer finder state including input buffer, buffer lists, and selection index.
    /// Mode and KeyContext transitions are now handled by the
    /// [`SetKeyContext`](crate::actions::SetKeyContext) action bound to Escape.
    ///
    /// # State Cleared
    ///
    /// - `buffer_finder_input` - search input buffer
    /// - `buffer_finder_buffers` - full buffer list from BufferStore
    /// - `buffer_finder_filtered` - filtered buffer list from fuzzy matching
    /// - `buffer_finder_selected` - selection index
    /// - `buffer_finder_previous_mode` - saved mode for restoration
    ///
    /// # Behavior
    ///
    /// - Only operates in buffer_finder mode
    /// - Does not restore previous mode (handled by SetKeyContext action)
    /// - No async tasks to cancel (unlike file_finder)
    ///
    /// # Related
    ///
    /// - [`crate::actions::SetKeyContext`] - handles mode/context transitions
    /// - [`Stoat::open_buffer_finder`] - initializes finder state
    pub fn buffer_finder_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "buffer_finder" {
            return;
        }

        debug!("Dismissing buffer finder");

        // Clear buffer finder state
        self.buffer_finder_input = None;
        self.buffer_finder_buffers.clear();
        self.buffer_finder_filtered.clear();
        self.buffer_finder_selected = 0;
        self.buffer_finder_previous_mode = None;

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn dismisses_buffer_finder(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_buffer_finder(&[], cx);
            assert!(s.buffer_finder_input.is_some());
            s.buffer_finder_dismiss(cx);
            assert!(s.buffer_finder_input.is_none());
            assert!(s.buffer_finder_buffers.is_empty());
        });
    }
}
