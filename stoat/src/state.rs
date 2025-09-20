//! Editor state representation.
//!
//! This module defines the complete state of the editor as immutable data.
//! All state transitions happen by creating new state instances, making
//! the system predictable and enabling features like time-travel debugging.

use crate::actions::{EditMode, TextPosition, TextRange};
use std::{path::PathBuf, sync::Arc};
use stoat_rope::{ast::AstNode, kind::SyntaxKind, RopeAst};
use stoat_text::parser::{Language, Parser};

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

    /// Creates an editor state with the given text content and language.
    pub fn with_text_and_language(text: &str, language: Language) -> Self {
        Self {
            buffer: TextBuffer::with_text_and_language(text, language),
            cursor: Cursor::new(),
            mode: EditMode::Normal,
            viewport: Viewport::new(),
            file: FileInfo::new(),
            is_dirty: false,
            show_command_info: false,
        }
    }

    /// Creates an editor state from a file path and its content.
    pub fn from_file(path: impl AsRef<std::path::Path>, content: &str) -> Self {
        let path_buf = path.as_ref().to_path_buf();
        Self {
            buffer: TextBuffer::with_text(content),
            cursor: Cursor::new(),
            mode: EditMode::Normal,
            viewport: Viewport::new(),
            file: FileInfo::with_path(path_buf),
            is_dirty: false,
            show_command_info: false,
        }
    }

    /// Returns the current cursor position.
    pub fn cursor_position(&self) -> TextPosition {
        self.cursor.position()
    }

    /// Returns the current text selection, if any.
    pub fn selection(&self) -> Option<TextRange> {
        self.cursor.selection()
    }

    /// Returns the complete text content.
    pub fn text(&self) -> String {
        self.buffer.text()
    }

    /// Returns the number of lines in the buffer.
    pub fn line_count(&self) -> usize {
        self.buffer.lines().count()
    }

    /// Returns a specific line from the buffer.
    pub fn line(&self, index: usize) -> Option<String> {
        self.buffer.lines().nth(index)
    }

    /// Returns the language context at the current cursor position.
    ///
    /// Walks the AST to find the most specific language context
    /// at the cursor's location.
    pub fn language_at_cursor(&self) -> Option<stoat_rope::Language> {
        // Convert cursor position to byte offset
        let offset = self.position_to_offset(self.cursor.position());
        self.buffer.rope.language_at_offset(offset)
    }

    /// Convert a TextPosition to a byte offset in the buffer.
    pub fn position_to_offset(&self, position: TextPosition) -> usize {
        // Get the full text and split it into lines manually
        let text = self.buffer.text();
        let mut offset = 0;

        for (current_line, line) in text.lines().enumerate() {
            if current_line == position.line {
                // Found the target line, add column offset
                return offset + position.column.min(line.len());
            }
            offset += line.len() + 1; // +1 for newline
        }

        // Position is beyond the end of the buffer
        offset.saturating_sub(1).min(text.len())
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
    /// The rope AST containing text and structure
    rope: RopeAst,
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl TextBuffer {
    pub fn new() -> Self {
        // Create empty rope
        let root = Arc::new(AstNode::syntax(
            SyntaxKind::Document,
            stoat_rope::ast::TextRange::new(0, 0),
        ));
        Self {
            rope: RopeAst::from_root(root),
        }
    }

    pub fn with_text(text: &str) -> Self {
        Self::with_text_and_language(text, Language::PlainText)
    }

    pub fn with_text_and_language(text: &str, language: Language) -> Self {
        if text.is_empty() {
            return Self::new();
        }

        // Use the proper parser to create a structured AST with paragraphs
        let mut parser = Parser::from_language(language).expect("Failed to create parser");

        let rope_arc = parser.parse_text(text).expect("Failed to parse text");

        // Convert Arc<RopeAst> to RopeAst by dereferencing and cloning
        let rope = (*rope_arc).clone();

        Self { rope }
    }

    pub fn text(&self) -> String {
        // Use rope's native text extraction
        let total_range = stoat_rope::ast::TextRange::new(0, self.rope.len_bytes());
        self.rope.text_at_range(total_range)
    }

    pub fn lines(&self) -> impl Iterator<Item = String> + use<'_> {
        // The rope's line iterator expects explicit Newline tokens, but our
        // markdown parser includes newlines in text content. So we need to
        // split the text manually.
        let text = self.text();
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.lines().map(|s| s.to_string()).collect()
        };
        lines.into_iter()
    }

    pub fn is_empty(&self) -> bool {
        self.rope.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rope.len_bytes()
    }

    /// Get access to the underlying rope for advanced operations
    pub fn rope(&self) -> &RopeAst {
        &self.rope
    }

    /// Update the rope content
    pub fn set_rope(&mut self, rope: RopeAst) {
        self.rope = rope;
    }
}

/// Cursor state with Helix-style anchor/head model.
///
/// In this model, the cursor always represents a selection range.
/// When anchor == head, it's a single character selection (normal cursor).
#[derive(Debug, Clone)]
pub struct Cursor {
    /// Anchor position (where selection started)
    pub anchor: TextPosition,

    /// Head position (where cursor currently is)
    pub head: TextPosition,

    /// Desired column for vertical movement (vim's 'virtualedit' concept)
    pub desired_column: usize,
}

impl Default for Cursor {
    fn default() -> Self {
        Self::new()
    }
}

impl Cursor {
    pub fn new() -> Self {
        let start = TextPosition::start();
        Self {
            anchor: start,
            head: start,
            desired_column: 0,
        }
    }

    pub fn at_position(position: TextPosition) -> Self {
        Self {
            anchor: position,
            head: position,
            desired_column: position.column,
        }
    }

    /// Returns the current cursor position (the head)
    pub fn position(&self) -> TextPosition {
        self.head
    }

    /// Returns the selection range if anchor != head
    pub fn selection(&self) -> Option<TextRange> {
        if self.anchor == self.head {
            None
        } else {
            // Always return range with start <= end
            if self.anchor < self.head {
                Some(TextRange::new(self.anchor, self.head))
            } else {
                Some(TextRange::new(self.head, self.anchor))
            }
        }
    }

    /// Move head while keeping anchor fixed (extends selection)
    pub fn move_head(&mut self, new_head: TextPosition) {
        self.head = new_head;
        self.desired_column = self.head.column;
    }

    /// Move both anchor and head to same position (collapses selection)
    pub fn move_to(&mut self, position: TextPosition) {
        self.anchor = position;
        self.head = position;
        self.desired_column = self.head.column;
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

impl Default for Viewport {
    fn default() -> Self {
        Self::new()
    }
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

impl Default for FileInfo {
    fn default() -> Self {
        Self::new()
    }
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

impl Default for EditorStateBuilder {
    fn default() -> Self {
        Self::new()
    }
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
        let pos = TextPosition::new(line, column);
        self.state.cursor.anchor = pos;
        self.state.cursor.head = pos;
        self.state.cursor.desired_column = column;
        self
    }

    pub fn in_mode(mut self, mode: EditMode) -> Self {
        self.state.mode = mode;
        self
    }

    pub fn with_selection(mut self, start: TextPosition, end: TextPosition) -> Self {
        self.state.cursor.anchor = start;
        self.state.cursor.head = end;
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
