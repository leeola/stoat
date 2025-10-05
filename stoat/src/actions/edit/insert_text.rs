//! Insert text command
//!
//! Inserts text at the current cursor position. This is the primary text input mechanism
//! for the editor, handling everything from single character input to paste operations.

use crate::Stoat;
use gpui::App;
use tracing::trace;

impl Stoat {
    /// Insert text at the current cursor position.
    ///
    /// Inserts the given text string at the cursor position and advances the cursor
    /// past the inserted text. The buffer is re-parsed after insertion to update
    /// syntax highlighting.
    ///
    /// # Behavior
    ///
    /// - Inserts text at cursor offset
    /// - Re-parses buffer to update token map (main buffer only)
    /// - Advances cursor by length of inserted text
    /// - Handles single characters, strings, and pasted content
    /// - In file_finder mode, inserts into input buffer and re-filters files
    ///
    /// # Mode-Aware Routing
    ///
    /// - **file_finder mode**: Inserts into input buffer, triggers file filtering
    /// - **Other modes**: Inserts into main buffer, updates syntax highlighting
    ///
    /// # Implementation Details
    ///
    /// This method:
    /// 1. Checks current mode and selects appropriate buffer
    /// 2. Converts cursor position to byte offset
    /// 3. Updates the buffer with the new text
    /// 4. Re-parses main buffer or re-filters files depending on mode
    /// 5. Moves cursor forward by the inserted text length
    ///
    /// # Context
    ///
    /// This is dispatched by the input system when:
    /// - User types a character in insert mode or file_finder mode
    /// - Text is pasted from clipboard
    /// - IME systems complete multi-character input
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::actions::edit::delete_left`] for backspace
    /// - [`crate::actions::edit::delete_right`] for delete
    pub fn insert_text(&mut self, text: &str, cx: &mut App) {
        // Route to file finder input buffer if in file_finder mode
        if self.mode() == "file_finder" {
            if let Some(input_buffer) = &self.file_finder_input {
                // Insert into input buffer at end
                let snapshot = input_buffer.read(cx).snapshot();
                let end_offset = snapshot.len();

                input_buffer.update(cx, |buffer, _cx| {
                    buffer.edit([(end_offset..end_offset, text)]);
                });

                // Re-filter files based on new query
                let query = self.file_finder_query(cx);
                self.filter_files(&query);

                trace!(query = ?query, filtered_count = self.file_finder_filtered.len(), "File finder: filtered");
            }
            return;
        }

        // Main buffer insertion for all other modes
        let buffer_snapshot = self.buffer_snapshot(cx);
        let cursor_pos = self.cursor_manager.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        trace!(pos = ?cursor_pos, text = ?text, len = text.len(), "Inserting text");

        // Update buffer and reparse through active item
        let active_item = self.active_buffer_item(cx);
        active_item.update(cx, |item, cx| {
            // Edit buffer
            item.buffer().update(cx, |buffer, _| {
                buffer.edit([(cursor_offset..cursor_offset, text)]);
            });

            // Reparse to update syntax highlighting
            if let Err(e) = item.reparse(cx) {
                tracing::error!("Failed to parse after insert: {}", e);
            }
        });

        // Move cursor forward by the inserted text length
        let buffer_snapshot = self.buffer_snapshot(cx);
        let new_cursor_position = buffer_snapshot.offset_to_point(cursor_offset + text.len());
        self.cursor_manager.move_to(new_cursor_position);
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn insert_single_char() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 2);
        s.set_mode("insert");
        s.input("x");
        s.assert_cursor_notation("hex|llo");
    }

    #[test]
    fn insert_multiple_chars() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 5);
        s.set_mode("insert");
        s.input("space w o r l d");
        s.assert_cursor_notation("hello world|");
    }

    #[test]
    fn insert_at_start() {
        let mut s = Stoat::test();
        s.set_text("world");
        s.set_cursor(0, 0);
        s.set_mode("insert");
        s.input("h e l l o space");
        s.assert_cursor_notation("hello |world");
    }

    #[test]
    fn insert_at_end() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 5);
        s.set_mode("insert");
        s.input("!");
        s.assert_cursor_notation("hello!|");
    }

    #[test]
    fn insert_newline() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 5);
        s.set_mode("insert");
        s.input("\n");
        s.assert_cursor_notation("hello\n|");
    }
}
