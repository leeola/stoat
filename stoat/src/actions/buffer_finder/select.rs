//! Buffer finder select action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Select buffer in finder.
    ///
    /// Switches to the selected buffer from [`crate::buffer_store::BufferStore`]. Retrieves
    /// the buffer by ID, updates active buffer tracking, updates activation history, and
    /// resets cursor position. Automatically dismisses the buffer finder after selection.
    ///
    /// # Workflow
    ///
    /// 1. Gets selected buffer entry from filtered list
    /// 2. Retrieves buffer from BufferStore by ID
    /// 3. Updates active_buffer_id to new buffer
    /// 4. Updates current_file_path for status bar (None for unnamed buffers)
    /// 5. Updates BufferStore activation history
    /// 6. Resets cursor to beginning (0,0)
    /// 7. Dismisses buffer finder via [`Stoat::buffer_finder_dismiss`]
    ///
    /// # Behavior
    ///
    /// - Only operates in buffer_finder mode
    /// - Logs error if buffer not found in store
    /// - Always dismisses finder after selection attempt
    /// - Cursor reset is simple (could be improved to save/restore per-buffer cursor)
    ///
    /// # Related
    ///
    /// - [`crate::buffer_store::BufferStore::get_buffer`] - retrieves buffer by ID
    /// - [`crate::buffer_store::BufferStore::activate_buffer`] - updates activation history
    /// - [`Stoat::buffer_finder_dismiss`] - cleanup and mode restoration
    pub fn buffer_finder_select(&mut self, cx: &mut Context<Self>) {
        if self.mode != "buffer_finder" {
            return;
        }

        if self.buffer_finder_selected < self.buffer_finder_filtered.len() {
            let entry = &self.buffer_finder_filtered[self.buffer_finder_selected];
            debug!(buffer = ?entry.display_name, buffer_id = ?entry.buffer_id, "Buffer finder: switching to buffer");

            // Get buffer from BufferStore by ID
            if let Some(buffer_item) = self.buffer_store.read(cx).get_buffer(entry.buffer_id) {
                let buffer_id = buffer_item.read(cx).buffer().read(cx).remote_id();

                // Update active_buffer_id
                self.active_buffer_id = Some(buffer_id);

                // Update current_file_path for status bar (None for unnamed buffers)
                self.current_file_path = entry.path.as_ref().map(|p| self.normalize_file_path(p));

                // Update activation history
                self.buffer_store
                    .update(cx, |store, _cx| store.activate_buffer(buffer_id));

                // Reset cursor to beginning (could be improved to save/restore per-buffer cursor)
                let target_pos = text::Point::new(0, 0);
                self.cursor.move_to(target_pos);

                // Sync selections to cursor position
                let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
                let id = self.selections.next_id();
                self.selections.select(
                    vec![text::Selection {
                        id,
                        start: target_pos,
                        end: target_pos,
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    }],
                    &buffer_snapshot,
                );

                debug!(buffer_id = ?buffer_id, "Switched to buffer");
            } else {
                tracing::error!("Buffer not found in BufferStore: {:?}", entry.buffer_id);
            }
        }

        self.buffer_finder_dismiss(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn selects_buffer(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_buffer_finder(&[], cx);
            s.buffer_finder_select(cx);
            // Dismiss clears state but doesn't change mode (SetKeyContext handles mode transitions)
            assert!(s.buffer_finder_input.is_none());
        });
    }
}
