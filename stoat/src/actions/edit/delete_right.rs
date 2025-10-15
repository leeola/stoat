//! Delete right action implementation and tests.
//!
//! This module implements the [`delete_right`](crate::Stoat::delete_right) action, which deletes
//! the character at the cursor position (forward delete). Unlike [`delete_left`], this action only
//! operates on the main buffer and doesn't route to finder/palette inputs.
//!
//! When at the end of a line, this action merges with the next line by deleting the newline
//! character.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Delete character at cursor position (delete right).
    ///
    /// Removes the character under the cursor without moving the cursor position. At line
    /// endings, merges the current line with the next line by removing the newline.
    ///
    /// # Behavior
    ///
    /// - Mid-line: Deletes character at cursor, cursor stays in place
    /// - At line end: Merges with next line (removes newline), cursor stays in place
    /// - At buffer end: No-op (nothing to delete)
    ///
    /// # Related Actions
    ///
    /// - [`delete_left`](crate::Stoat::delete_left) - Delete character before cursor
    /// - [`delete_word_right`](crate::Stoat::delete_word_right) - Delete next word
    pub fn delete_right(&mut self, cx: &mut Context<Self>) {
        let cursor = self.cursor.position();

        // Read buffer info in separate scope to release locks
        let (line_len, max_row) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer = buffer_item.buffer().read(cx);
            (buffer.line_len(cursor.row), buffer.max_point().row)
        };

        if cursor.column < line_len {
            // Delete character on same line
            let buffer = self.active_buffer(cx).read(cx).buffer().clone();
            buffer.update(cx, |buffer, _| {
                let start = buffer.point_to_offset(cursor);
                let end = buffer.point_to_offset(text::Point::new(cursor.row, cursor.column + 1));
                buffer.edit([(start..end, "")]);
            });

            // Cursor stays in place
        } else if cursor.row < max_row {
            // At line end: merge with next line
            let buffer = self.active_buffer(cx).read(cx).buffer().clone();
            buffer.update(cx, |buffer, _| {
                let start = buffer.point_to_offset(cursor);
                let end = buffer.point_to_offset(text::Point::new(cursor.row + 1, 0));
                buffer.edit([(start..end, "")]);
            });

            // Cursor stays in place
        }
        // Else: at buffer end, no-op

        // Reparse
        self.active_buffer(cx).update(cx, |item, cx| {
            let _ = item.reparse(cx);
        });

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn deletes_character_at_cursor(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.delete_right(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "ello");
            assert_eq!(s.cursor.position(), text::Point::new(0, 0));
        });
    }

    #[gpui::test]
    fn merges_lines_at_line_end(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello\nWorld", cx);
            s.set_cursor_position(text::Point::new(0, 5));
            s.delete_right(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "HelloWorld");
        });
    }

    #[gpui::test]
    fn no_op_at_buffer_end(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hi", cx);
            s.delete_right(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Hi");
        });
    }
}
