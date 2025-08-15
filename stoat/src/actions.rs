//! Action types for pure state transformations.
//!
//! Actions represent atomic changes that can be applied to editor state.
//! They are the result of processing events and contain all the information
//! needed to transform state in a pure, predictable way.

/// Pure state transformation actions.
///
/// Each action represents a single, atomic change to the editor state.
/// Actions are designed to be easily testable and can be combined to
/// implement complex operations.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorAction {
    /// Insert text at the specified position
    InsertText {
        position: TextPosition,
        text: String,
    },

    /// Delete text in the specified range
    DeleteText { range: TextRange },

    /// Replace text in a range with new text
    ReplaceText { range: TextRange, new_text: String },

    /// Move cursor to a new position
    MoveCursor { position: TextPosition },

    /// Set text selection range
    SetSelection { range: Option<TextRange> },

    /// Switch to a different editing mode
    SetMode { mode: EditMode },

    /// Set the viewport size
    SetViewportSize { width: f32, height: f32 },

    /// Scroll the viewport by offset
    ScrollViewport { delta_x: f32, delta_y: f32 },

    /// Replace entire buffer content
    SetContent { content: String },

    /// Set the file path for the current buffer
    SetFilePath { path: Option<std::path::PathBuf> },

    /// Mark buffer as clean (saved) or dirty (modified)
    SetDirty { dirty: bool },
}

// Basic types needed for actions - these will be proper imports later
/// Text position in the buffer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextPosition {
    pub line: usize,
    pub column: usize,
}

impl TextPosition {
    pub fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }

    pub fn start() -> Self {
        Self::new(0, 0)
    }
}

/// Text range in the buffer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    pub start: TextPosition,
    pub end: TextPosition,
}

impl TextRange {
    pub fn new(start: TextPosition, end: TextPosition) -> Self {
        Self { start, end }
    }
}

/// Editing modes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditMode {
    Normal,
    Insert,
    Visual { line_mode: bool },
    Command,
}
