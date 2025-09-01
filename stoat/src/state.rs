//! Editor state representation.
//!
//! This module defines the complete state of the editor as immutable data.
//! All state transitions happen by creating new state instances, making
//! the system predictable and enabling features like time-travel debugging.

use crate::actions::{EditMode, TextPosition, TextRange};
use std::path::PathBuf;

/// Complete immutable state of the text editor.
///
/// The editor state contains all information needed to represent the current
/// state of the editor, including buffer content, cursor position, viewport,
/// and any metadata about the current file.
#[derive(Debug, Clone)]
pub struct EditorState {
    /// Text content of the buffer
    pub buffer: TextBuffer,

    /// Current cursor state
    pub cursor: Cursor,

    /// Current editing mode
    pub mode: EditMode,

    /// Viewport state for scrolling and rendering
    pub viewport: Viewport,

    /// File metadata
    pub file: FileInfo,

    /// Whether the buffer has unsaved changes
    pub is_dirty: bool,

    /// Whether to show command info panel
    pub show_command_info: bool,
}

impl EditorState {
    /// Creates a new empty editor state.
    pub fn new() -> Self {
        Self {
            buffer: TextBuffer::new(),
            cursor: Cursor::new(),
            mode: EditMode::Normal,
            viewport: Viewport::new(),
            file: FileInfo::new(),
            is_dirty: false,
            show_command_info: false,
        }
    }

    /// Creates an editor state with the given text content.
    pub fn with_text(text: &str) -> Self {
        Self {
            buffer: TextBuffer::with_text(text),
            cursor: Cursor::new(),
            mode: EditMode::Normal,
            viewport: Viewport::new(),
            file: FileInfo::new(),
            is_dirty: false,
            show_command_info: false,
        }
    }

    /// Returns the current cursor position.
    pub fn cursor_position(&self) -> TextPosition {
        self.cursor.position
    }

    /// Returns the current text selection, if any.
    pub fn selection(&self) -> Option<TextRange> {
        self.cursor.selection
    }

    /// Returns the complete text content.
    pub fn text(&self) -> &str {
        &self.buffer.text
    }

    /// Returns the number of lines in the buffer.
    pub fn line_count(&self) -> usize {
        self.buffer.lines().count()
    }

    /// Returns a specific line from the buffer.
    pub fn line(&self, index: usize) -> Option<&str> {
        self.buffer.lines().nth(index)
    }

    /// Builder for creating test states
    pub fn builder() -> EditorStateBuilder {
        EditorStateBuilder::new()
    }
}

impl Default for EditorState {
    fn default() -> Self {
        Self::new()
    }
}

/// Text buffer containing the document content.
#[derive(Debug, Clone)]
pub struct TextBuffer {
    /// The complete text content
    text: String,
}

impl TextBuffer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
        }
    }

    pub fn with_text(text: &str) -> Self {
        Self {
            text: text.to_string(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.text.lines()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }
}

/// Cursor state including position and selection.
#[derive(Debug, Clone)]
pub struct Cursor {
    /// Current cursor position
    pub position: TextPosition,

    /// Text selection range, if any
    pub selection: Option<TextRange>,

    /// Desired column for vertical movement (vim's 'virtualedit' concept)
    pub desired_column: usize,
}

impl Cursor {
    pub fn new() -> Self {
        Self {
            position: TextPosition::start(),
            selection: None,
            desired_column: 0,
        }
    }

    pub fn at_position(position: TextPosition) -> Self {
        Self {
            position,
            selection: None,
            desired_column: position.column,
        }
    }
}

/// Viewport state for scrolling and visible area.
#[derive(Debug, Clone)]
pub struct Viewport {
    /// Horizontal scroll offset in characters
    pub scroll_x: f32,

    /// Vertical scroll offset in lines
    pub scroll_y: f32,

    /// Width of the viewport in pixels
    pub width: f32,

    /// Height of the viewport in pixels
    pub height: f32,

    /// Character width for monospace calculations
    pub char_width: f32,

    /// Line height in pixels
    pub line_height: f32,
}

impl Viewport {
    pub fn new() -> Self {
        Self {
            scroll_x: 0.0,
            scroll_y: 0.0,
            width: 800.0,
            height: 600.0,
            char_width: 8.0,
            line_height: 16.0,
        }
    }

    /// Returns the number of visible lines in the viewport
    pub fn visible_lines(&self) -> usize {
        (self.height / self.line_height) as usize
    }

    /// Returns the number of visible columns in the viewport
    pub fn visible_columns(&self) -> usize {
        (self.width / self.char_width) as usize
    }
}

/// File information and metadata.
#[derive(Debug, Clone)]
pub struct FileInfo {
    /// Path to the file, if any
    pub path: Option<PathBuf>,

    /// File name for display purposes
    pub name: String,
}

impl FileInfo {
    pub fn new() -> Self {
        Self {
            path: None,
            name: "Untitled".to_string(),
        }
    }

    pub fn with_path(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Untitled")
            .to_string();

        Self {
            path: Some(path),
            name,
        }
    }
}

/// Builder for creating editor states (useful for testing).
pub struct EditorStateBuilder {
    state: EditorState,
}

impl EditorStateBuilder {
    pub fn new() -> Self {
        Self {
            state: EditorState::new(),
        }
    }

    pub fn with_text<S: AsRef<str>>(mut self, text: S) -> Self {
        self.state.buffer = TextBuffer::with_text(text.as_ref());
        self
    }

    pub fn with_cursor(mut self, line: usize, column: usize) -> Self {
        self.state.cursor.position = TextPosition::new(line, column);
        self.state.cursor.desired_column = column;
        self
    }

    pub fn in_mode(mut self, mode: EditMode) -> Self {
        self.state.mode = mode;
        self
    }

    pub fn with_selection(mut self, start: TextPosition, end: TextPosition) -> Self {
        self.state.cursor.selection = Some(TextRange::new(start, end));
        self
    }

    pub fn with_file(mut self, path: PathBuf) -> Self {
        self.state.file = FileInfo::with_path(path);
        self
    }

    pub fn dirty(mut self, dirty: bool) -> Self {
        self.state.is_dirty = dirty;
        self
    }

    pub fn with_command_info(mut self, show: bool) -> Self {
        self.state.show_command_info = show;
        self
    }

    pub fn build(self) -> EditorState {
        self.state
    }
}
