//! Helper methods for BufferSnapshot compatibility.
//!
//! FIXME: These are stubs. Replace with proper Buffer/BufferSnapshot usage
//! from the text crate once we integrate with the full editor.

use text::{BufferSnapshot, Point};

/// Extension trait providing helper methods for BufferSnapshot.
pub trait BufferSnapshotExt {
    /// Get the text of a specific line.
    fn line(&self, row: u32) -> String;

    /// Get the length of a specific line in bytes.
    fn line_len(&self, row: u32) -> u32;

    /// Get the maximum valid point in the buffer.
    fn max_point(&self) -> Point;
}

impl BufferSnapshotExt for BufferSnapshot {
    fn line(&self, _row: u32) -> String {
        // FIXME: Implement using real BufferSnapshot API
        String::new()
    }

    fn line_len(&self, _row: u32) -> u32 {
        // FIXME: Implement using real BufferSnapshot API
        0
    }

    fn max_point(&self) -> Point {
        // FIXME: Implement using real BufferSnapshot API
        Point::new(0, 0)
    }
}
