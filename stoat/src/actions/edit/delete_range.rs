//! Delete range helper
//!
//! Internal helper for deleting a range of text from the buffer. This is used by
//! all deletion commands to ensure consistent behavior and proper buffer re-parsing.

use crate::Stoat;
use gpui::App;
use text::Point;
use tracing::trace;

impl Stoat {
    /// Helper method to delete a range of text.
    ///
    /// Deletes the text between start and end points (inclusive of start, exclusive of end),
    /// and re-parses the buffer to update syntax highlighting.
    ///
    /// # Arguments
    ///
    /// * `range` - The range of text to delete, specified as Point positions
    ///
    /// # Behavior
    ///
    /// - Converts points to byte offsets
    /// - Updates buffer with deletion
    /// - Re-parses entire buffer to update token map
    /// - No cursor movement (caller is responsible)
    ///
    /// # Visibility
    ///
    /// This is `pub(crate)` to allow other action modules to use it while keeping it
    /// internal to the crate.
    ///
    /// # Implementation Details
    ///
    /// Currently performs a full buffer re-parse after each deletion. This is simple
    /// but may be optimized in the future to use incremental parsing.
    ///
    /// # Related
    ///
    /// Used by:
    /// - [`crate::actions::edit::delete_left`]
    /// - [`crate::actions::edit::delete_right`]
    /// - [`crate::actions::edit::delete_line`]
    /// - [`crate::actions::edit::delete_to_end_of_line`]
    pub(crate) fn delete_range(&mut self, range: std::ops::Range<Point>, cx: &mut App) {
        let buffer_snapshot = self.buffer_snapshot(cx);
        let start_offset = buffer_snapshot.point_to_offset(range.start);
        let end_offset = buffer_snapshot.point_to_offset(range.end);

        if start_offset < end_offset {
            let len = end_offset - start_offset;
            trace!(start = ?range.start, end = ?range.end, bytes = len, "Deleting range");

            // Update buffer and reparse through active item
            let active_item = self.active_buffer_item(cx);
            active_item.update(cx, |item, cx| {
                // Edit buffer
                item.buffer().update(cx, |buffer, _| {
                    buffer.edit([(start_offset..end_offset, "")]);
                });

                // Reparse to update syntax highlighting
                if let Err(e) = item.reparse(cx) {
                    tracing::error!("Failed to parse after delete: {}", e);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    // Note: delete_range is an internal helper method.
    // It's tested indirectly through the public deletion commands:
    // - delete_left (tested in delete_left.rs)
    // - delete_right (tested in delete_right.rs)
    // - delete_line (tested in delete_line.rs)
    // - delete_to_end_of_line (tested in delete_to_end_of_line.rs)

    #[test]
    fn delete_range_via_delete_left() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 2);
        s.command("DeleteLeft");
        assert_eq!(s.text(), "hllo");
    }

    #[test]
    fn delete_range_via_delete_right() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 1);
        s.command("DeleteRight");
        assert_eq!(s.text(), "hllo");
    }
}
