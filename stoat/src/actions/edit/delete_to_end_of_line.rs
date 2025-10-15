//! Delete to end of line action implementation and tests.
//!
//! This module implements the [`delete_to_end_of_line`](crate::Stoat::delete_to_end_of_line)
//! action, which removes all text from the cursor position to the end of the current line.
//! Unlike [`delete_line`](crate::Stoat::delete_line), this action preserves the newline
//! character and keeps the cursor at its current position.
//!
//! This action is useful for quickly clearing the remainder of a line without affecting the
//! line structure.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Delete from cursor to end of line.
    ///
    /// Removes all text from the cursor position to the end of the current line,
    /// preserving the newline character. The cursor stays at its current position.
    ///
    /// # Behavior
    ///
    /// - Deletes from cursor to end of line (exclusive of newline)
    /// - Cursor stays at current position
    /// - If cursor is already at end of line: no effect
    /// - Empty lines remain empty
    ///
    /// # Implementation
    ///
    /// Compares cursor column with line length to determine if deletion is needed.
    /// Only performs deletion if there's text between cursor and line end.
    ///
    /// # Related Actions
    ///
    /// - [`delete_line`](crate::Stoat::delete_line) - Delete entire line including newline
    /// - [`delete_right`](crate::Stoat::delete_right) - Delete single character
    pub fn delete_to_end_of_line(&mut self, cx: &mut Context<Self>) {
        let cursor = self.cursor.position();

        // Get line length
        let line_len = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer = buffer_item.buffer().read(cx);
            buffer.line_len(cursor.row)
        };

        // Only delete if not already at end of line
        if cursor.column < line_len {
            let end = text::Point::new(cursor.row, line_len);

            debug!(from = ?cursor, to = ?end, "Delete to end of line");

            // Perform deletion
            let buffer = self.active_buffer(cx).read(cx).buffer().clone();
            buffer.update(cx, |buffer, _| {
                let start_offset = buffer.point_to_offset(cursor);
                let end_offset = buffer.point_to_offset(end);
                buffer.edit([(start_offset..end_offset, "")]);
            });

            // Reparse
            self.active_buffer(cx).update(cx, |item, cx| {
                let _ = item.reparse(cx);
            });

            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
        } else {
            debug!(pos = ?cursor, "Already at end of line, nothing to delete");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn deletes_to_end_of_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello World", cx);
            s.set_cursor_position(text::Point::new(0, 6));
            s.delete_to_end_of_line(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Hello ");
        });
    }

    #[gpui::test]
    fn preserves_newline(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello\nWorld", cx);
            s.set_cursor_position(text::Point::new(0, 2));
            s.delete_to_end_of_line(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "He\nWorld");
        });
    }

    #[gpui::test]
    fn no_op_at_end_of_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.delete_to_end_of_line(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Hello");
        });
    }

    #[gpui::test]
    fn cursor_stays_in_place(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello World", cx);
            s.set_cursor_position(text::Point::new(0, 6));
            s.delete_to_end_of_line(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 6));
        });
    }
}
