//! Cursor and selection management
//!
//! Provides the [`Cursor`] type for tracking positions and selections within text buffers.
//! Supports multi-cursor editing where multiple cursors can exist across different buffers
//! and even across different [`crate::node::Node`] instances sharing the same buffer.

/// A cursor position with optional selection in a text buffer
///
/// Cursors track both a position and potentially a selection range. In multi-cursor
/// scenarios, each cursor operates independently and can span across different
/// buffers when performing cross-file operations.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct Cursor {
    /// Which token the cursor is positioned at (0-based)
    token_index: usize,

    /// Position within the token (0-based character offset)
    char_offset: usize,

    /// Buffer ID this cursor belongs to
    buffer_id: u64,

    /// Whether this cursor is the primary cursor
    is_primary: bool,
}

impl Cursor {
    /// Create a new cursor at the beginning of a buffer
    pub fn new(buffer_id: u64) -> Self {
        Self {
            token_index: 0,
            char_offset: 0,
            buffer_id,
            is_primary: true,
        }
    }

    /// Get the token index this cursor is positioned at
    pub fn token_index(&self) -> usize {
        self.token_index
    }

    /// Get the character offset within the current token
    pub fn char_offset(&self) -> usize {
        self.char_offset
    }

    /// Get the buffer ID this cursor belongs to
    pub fn buffer_id(&self) -> u64 {
        self.buffer_id
    }

    /// Check if this is the primary cursor
    pub fn is_primary(&self) -> bool {
        self.is_primary
    }

    /// Update the cursor position
    pub fn set_position(&mut self, token_index: usize, char_offset: usize) {
        self.token_index = token_index;
        self.char_offset = char_offset;
    }
}
