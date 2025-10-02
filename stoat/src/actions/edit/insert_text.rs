//! Insert text command
//!
//! Inserts text at the current cursor position. This is the primary text input mechanism
//! for the editor, handling everything from single character input to paste operations.

use crate::Stoat;
use gpui::App;

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
    /// - Re-parses buffer to update token map
    /// - Advances cursor by length of inserted text
    /// - Handles single characters, strings, and pasted content
    ///
    /// # Implementation Details
    ///
    /// This method:
    /// 1. Converts cursor position to byte offset
    /// 2. Updates the buffer with the new text
    /// 3. Re-parses the entire buffer (full re-parse for simplicity)
    /// 4. Moves cursor forward by the inserted text length
    ///
    /// # Context
    ///
    /// This is dispatched by the input system when:
    /// - User types a character in insert mode
    /// - Text is pasted from clipboard
    /// - IME systems complete multi-character input
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::actions::edit::delete_left`] for backspace
    /// - [`crate::actions::edit::delete_right`] for delete
    pub fn insert_text(&mut self, text: &str, cx: &mut App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let cursor_offset = buffer_snapshot.point_to_offset(self.cursor_manager.position());

        self.buffer.update(cx, |buffer, _cx| {
            buffer.edit([(cursor_offset..cursor_offset, text)]);
        });

        // Re-parse entire buffer after edit
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let contents = buffer_snapshot.text();
        match self.parser.parse(&contents, &buffer_snapshot) {
            Ok(tokens) => {
                self.token_map.replace_tokens(tokens, &buffer_snapshot);
            },
            Err(e) => {
                tracing::error!("Failed to parse after insert: {}", e);
            },
        }

        // Move cursor forward by the inserted text length
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
        s.set_mode(crate::EditorMode::Insert);
        s.input("x");
        s.assert_cursor_notation("hex|llo");
    }

    #[test]
    fn insert_multiple_chars() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 5);
        s.set_mode(crate::EditorMode::Insert);
        s.input(" world");
        s.assert_cursor_notation("hello world|");
    }

    #[test]
    fn insert_at_start() {
        let mut s = Stoat::test();
        s.set_text("world");
        s.set_cursor(0, 0);
        s.set_mode(crate::EditorMode::Insert);
        s.input("hello ");
        s.assert_cursor_notation("hello |world");
    }

    #[test]
    fn insert_at_end() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 5);
        s.set_mode(crate::EditorMode::Insert);
        s.input("!");
        s.assert_cursor_notation("hello!|");
    }

    #[test]
    fn insert_newline() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 5);
        s.set_mode(crate::EditorMode::Insert);
        s.input("\n");
        s.assert_cursor_notation("hello\n|");
    }
}
