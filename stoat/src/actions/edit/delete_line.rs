//! Delete line action implementation and tests.
//!
//! This module implements the [`delete_line`](crate::Stoat::delete_line) action, which removes
//! the entire line where the cursor is positioned. The implementation handles both regular lines
//! (which include the trailing newline) and the last line of the buffer (which may not have a
//! trailing newline).
//!
//! After deletion, the cursor moves to the beginning of the line (or the next line if the
//! deleted line was not the last line).

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Delete current line.
    ///
    /// Removes the entire line where the cursor is positioned, including the trailing
    /// newline (except for the last line). The cursor moves to the beginning of the line.
    ///
    /// # Behavior
    ///
    /// - For non-last lines: deletes from line start to next line start (includes newline)
    /// - For last line: deletes from line start to line end (no newline to delete)
    /// - Cursor moves to beginning of the line (or next line if not last)
    /// - Empty buffer remains empty
    ///
    /// # Implementation
    ///
    /// Determines line boundaries based on whether the current line is the last line in the
    /// buffer. For non-last lines, includes the newline character in the deletion range.
    ///
    /// # Related Actions
    ///
    /// - [`delete_to_end_of_line`](crate::Stoat::delete_to_end_of_line) - Delete to line end only
    /// - [`new_line`](crate::Stoat::new_line) - Insert new line
    pub fn delete_line(&mut self, cx: &mut Context<Self>) {
        let cursor = self.cursor.position();

        // Get buffer snapshot to determine line boundaries
        let (line_start, line_end) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer = buffer_item.buffer().read(cx);
            let row_count = buffer.row_count();

            let line_start = text::Point::new(cursor.row, 0);

            // Include newline if not last line
            let line_end = if cursor.row < row_count - 1 {
                text::Point::new(cursor.row + 1, 0)
            } else {
                // Last line - delete to end of line
                let line_len = buffer.line_len(cursor.row);
                text::Point::new(cursor.row, line_len)
            };

            (line_start, line_end)
        };

        debug!(row = cursor.row, from = ?line_start, to = ?line_end, "Deleting line");

        // Perform deletion
        let buffer = self.active_buffer(cx).read(cx).buffer().clone();
        buffer.update(cx, |buffer, _| {
            let start_offset = buffer.point_to_offset(line_start);
            let end_offset = buffer.point_to_offset(line_end);
            buffer.edit([(start_offset..end_offset, "")]);
        });

        // Move cursor to line start
        self.cursor.move_to(line_start);

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
    fn deletes_entire_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.set_cursor_position(text::Point::new(1, 3));
            s.delete_line(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Line 1\nLine 3");
        });
    }

    #[gpui::test]
    fn deletes_last_line_without_newline(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2", cx);
            s.set_cursor_position(text::Point::new(1, 0));
            s.delete_line(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Line 1\n");
        });
    }

    #[gpui::test]
    fn moves_cursor_to_line_start(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello\nWorld", cx);
            s.set_cursor_position(text::Point::new(0, 3));
            s.delete_line(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 0));
        });
    }
}
