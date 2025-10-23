///! Temporary stubs for text crate types until upstream compilation issue is resolved.
///!
///! FIXME: Remove this module once Zed's text crate smallvec issue is fixed.
///! These types are simplified versions that allow TabMap development to proceed.

/// Buffer coordinate representing a position in the raw text.
///
/// This is a temporary stub for [`text::Point`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Point {
    pub row: u32,
    pub column: u32,
}

/// Describes a buffer edit operation.
///
/// This is a temporary stub for [`text::BufferEdit`].
#[derive(Debug, Clone)]
pub struct BufferEdit {
    pub old_range: std::ops::Range<Point>,
    pub new_range: std::ops::Range<Point>,
}

/// Immutable snapshot of buffer state.
///
/// This is a temporary stub for [`text::BufferSnapshot`].
pub struct BufferSnapshot {
    text: String,
}

impl BufferSnapshot {
    /// Create a snapshot from text.
    pub fn from_text(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    /// Get a line from the buffer.
    pub fn line(&self, row: u32) -> &str {
        self.text.lines().nth(row as usize).unwrap_or("")
    }

    /// Get the maximum point in the buffer.
    pub fn max_point(&self) -> Point {
        let lines: Vec<&str> = self.text.lines().collect();
        let row = lines.len().saturating_sub(1) as u32;
        let column = lines.last().map(|l| l.len() as u32).unwrap_or(0);
        Point { row, column }
    }
}
