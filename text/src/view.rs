//! View management for text buffers
//!
//! Provides the [`View`] type that represents a viewport into a [`crate::buffer::Buffer`].
//! Multiple views can exist for the same buffer, each potentially showing different
//! portions of the text with different display settings.

use stoat_rope::ast::TextRange;

/// A view into a text buffer
///
/// Views define what portion of a buffer is visible and how it should be displayed.
/// Each [`crate::node::Node`] can have its own view configuration, allowing the same
/// buffer to be displayed differently across the canvas.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct View {
    /// The buffer this view is displaying
    buffer_id: u64,

    /// The visible range of text in this view
    viewport: TextRange,

    /// Current scroll position (line number at top of view)
    scroll_line: usize,

    /// Current horizontal scroll position
    scroll_column: usize,

    /// Number of visible lines in this view
    visible_lines: usize,

    /// Whether line numbers are shown
    show_line_numbers: bool,

    /// Whether whitespace characters are visible
    show_whitespace: bool,
}

impl View {
    // Implementation will follow in later phases
}
