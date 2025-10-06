//! Delete left command
//!
//! Deletes the character to the left of the cursor (backspace). At the beginning of a line,
//! merges with the previous line.

use crate::Stoat;
use gpui::App;
use text::Point;
use tracing::trace;

impl Stoat {
    /// Delete character to the left of cursor (backspace).
    ///
    /// Removes the character immediately before the cursor. At the beginning of a line,
    /// removes the newline and merges with the previous line.
    ///
    /// # Behavior
    ///
    /// - If mid-line: deletes previous character
    /// - If at line start: merges with previous line (removes newline)
    /// - If at buffer start: no effect
    /// - Cursor moves to deletion point
    /// - In file_finder mode, deletes from end of input buffer and re-filters
    ///
    /// # Mode-Aware Routing
    ///
    /// - **file_finder mode**: Deletes from end of input buffer, triggers file filtering
    /// - **Other modes**: Deletes from main buffer at cursor position
    ///
    /// # Implementation
    ///
    /// Uses the [`delete_range`] helper to handle the actual deletion and buffer re-parsing.
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::actions::edit::delete_right`] for forward delete
    /// - [`crate::actions::edit::delete_range`] for the underlying deletion mechanism
    pub fn delete_left(&mut self, cx: &mut App) {
        // Route to file finder input buffer if in file_finder mode
        if self.mode() == "file_finder" {
            if let Some(input_buffer) = &self.file_finder_input {
                let snapshot = input_buffer.read(cx).snapshot();
                let len = snapshot.len();

                if len > 0 {
                    // Delete last character
                    input_buffer.update(cx, |buffer, _cx| {
                        buffer.edit([(len - 1..len, "")]);
                    });

                    // Re-filter files based on new query
                    let query = self.file_finder_query(cx);
                    self.filter_files(&query);

                    trace!(query = ?query, filtered_count = self.file_finder_filtered.len(), "File finder: filtered after delete");
                }
            }
            return;
        }

        // Main buffer deletion for all other modes
        let current_pos = self.cursor_manager.position();
        if current_pos.column > 0 {
            // Delete character on same line
            let start = Point::new(current_pos.row, current_pos.column - 1);
            let end = current_pos;
            trace!(from = ?start, to = ?end, "Delete left within line");
            self.delete_range(start..end, cx);
            self.cursor_manager.move_to(start);
        } else if current_pos.row > 0 {
            // Merge with previous line
            let buffer_snapshot = self.buffer_snapshot(cx);
            let prev_row = current_pos.row - 1;
            let prev_line_len = buffer_snapshot.line_len(prev_row);
            let start = Point::new(prev_row, prev_line_len);
            let end = Point::new(current_pos.row, 0);
            trace!(from = ?start, to = ?end, "Delete left: merging with previous line");
            self.delete_range(start..end, cx);
            self.cursor_manager.move_to(start);
        } else {
            trace!(pos = ?current_pos, "At buffer start, cannot delete left");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn delete_left_within_line() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 3);
        s.command("DeleteLeft");
        s.assert_cursor_notation("he|lo");
    }

    #[test]
    fn delete_left_at_line_start() {
        let mut s = Stoat::test();
        s.set_text("foo\nbar");
        s.set_cursor(1, 0);
        s.command("DeleteLeft");
        s.assert_cursor_notation("foo|bar");
    }

    #[test]
    fn delete_left_at_buffer_start() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 0);
        s.command("DeleteLeft");
        s.assert_cursor_notation("|hello");
    }

    #[test]
    fn delete_left_multiple() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 5);
        s.input("shift-x shift-x shift-x");
        s.assert_cursor_notation("he|");
    }
}
