//! Helper methods for BufferSnapshot compatibility.

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
    fn line(&self, row: u32) -> String {
        let start = Point::new(row, 0);
        let end = Point::new(row, self.line_len(row));
        self.chars_for_range(start..end).collect()
    }

    fn line_len(&self, row: u32) -> u32 {
        self.as_rope().line_len(row)
    }

    fn max_point(&self) -> Point {
        BufferSnapshot::max_point(self)
    }
}
