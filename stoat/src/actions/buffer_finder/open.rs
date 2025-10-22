//! Open buffer finder action implementation and tests.

use crate::stoat::Stoat;
use gpui::{AppContext, Context};
use std::num::NonZeroU64;
use text::{Buffer, BufferId};
use tracing::debug;

impl Stoat {
    /// Open buffer finder modal.
    ///
    /// Initializes the buffer finder with all currently open buffers from
    /// [`crate::buffer_store::BufferStore`]. Creates an input buffer for fuzzy search and
    /// displays buffer metadata including active, visible, and dirty status flags.
    ///
    /// # Arguments
    ///
    /// * `visible_buffer_ids` - Buffer IDs visible in any pane (for visibility flag)
    /// * `cx` - Context for creating entities and reading state
    ///
    /// # Workflow
    ///
    /// 1. Saves current mode for restoration when dismissed
    /// 2. Creates input buffer (BufferId 3) for search queries
    /// 3. Queries BufferStore for buffer list with status flags
    /// 4. Initializes filtered list with all buffers
    /// 5. Sets selection to first buffer
    ///
    /// # Related
    ///
    /// - [`Stoat::buffer_finder_next`] - navigate to next buffer
    /// - [`Stoat::buffer_finder_prev`] - navigate to previous buffer
    /// - [`Stoat::buffer_finder_select`] - select and switch to buffer
    /// - [`Stoat::buffer_finder_dismiss`] - close finder
    /// - [`Stoat::filter_buffers`] - filter buffers by query
    /// - [`crate::buffer_store::BufferStore::buffer_list`] - gets buffer list with flags
    pub fn open_buffer_finder(&mut self, visible_buffer_ids: &[BufferId], cx: &mut Context<Self>) {
        debug!("Opening buffer finder");

        // Save current mode and context
        // TODO: Context restoration should be configurable via keymap once we have
        // concrete use cases to guide the design of keymap-based abstractions
        self.buffer_finder_previous_mode = Some(self.mode.clone());
        self.buffer_finder_previous_key_context = Some(self.key_context);
        self.key_context = crate::stoat::KeyContext::BufferFinder;
        self.mode = "buffer_finder".to_string();

        // Create input buffer
        let buffer_id = BufferId::from(NonZeroU64::new(3).unwrap());
        let input_buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
        self.buffer_finder_input = Some(input_buffer);

        // Get all open buffers from BufferStore with status flags
        let buffers =
            self.buffer_store
                .read(cx)
                .buffer_list(self.active_buffer_id, visible_buffer_ids, cx);

        debug!(
            buffer_count = buffers.len(),
            "Loaded buffers from BufferStore"
        );

        self.buffer_finder_buffers = buffers.clone();
        self.buffer_finder_filtered = buffers;
        self.buffer_finder_selected = 0;

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn opens_buffer_finder(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.mode = "normal".to_string();
            s.open_buffer_finder(&[], cx);
            assert_eq!(s.mode(), "buffer_finder");
            assert!(s.buffer_finder_input.is_some());
        });
    }
}
