//! Cursor and selection management
//!
//! Provides the [`Cursor`] type for tracking positions and selections within text buffers.
//! Supports multi-cursor editing where multiple cursors can exist across different buffers
//! and even across different [`crate::node::Node`] instances sharing the same buffer.

use stoat_rope::ast::TextPos;

/// A cursor position with optional selection in a text buffer
///
/// Cursors track both a position and potentially a selection range. In multi-cursor
/// scenarios, each cursor operates independently and can span across different
/// buffers when performing cross-file operations.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct Cursor {
    /// The current cursor position (where the cursor is)
    position: TextPos,

    /// The anchor position for selections (where selection started)
    /// When None, there is no selection
    anchor: Option<TextPos>,

    /// Buffer ID this cursor belongs to
    buffer_id: u64,

    /// Whether this cursor is the primary cursor
    is_primary: bool,
}

impl Cursor {
    // Implementation will follow in later phases
}
