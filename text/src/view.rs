//! View management for text buffers
//!
//! Provides the [`View`] type that represents a viewport into a [`crate::buffer::Buffer`].
//! Multiple views can exist for the same buffer, each potentially showing different
//! portions of the text with different display settings.

use crate::buffer::Buffer;
use std::ops::Range;
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
    /// Create a new view for the given buffer
    pub fn new(buffer: &Buffer) -> Self {
        Self {
            buffer_id: buffer.id(),
            viewport: TextRange::new(0, buffer.rope().len_bytes()),
            scroll_line: 0,
            scroll_column: 0,
            visible_lines: 50, // Default to 50 visible lines
            show_line_numbers: true,
            show_whitespace: false,
        }
    }

    /// Get the buffer ID this view is associated with
    pub fn buffer_id(&self) -> u64 {
        self.buffer_id
    }

    /// Set the number of visible lines in this view
    pub fn set_visible_lines(&mut self, lines: usize) {
        self.visible_lines = lines;
    }

    /// Get the current scroll position (top line)
    pub fn scroll_line(&self) -> usize {
        self.scroll_line
    }

    /// Set the scroll position
    pub fn set_scroll_line(&mut self, line: usize) {
        self.scroll_line = line;
    }

    /// Get the viewport as a line range for efficient iteration
    pub fn line_viewport(&self) -> Range<usize> {
        self.scroll_line..self.scroll_line + self.visible_lines
    }

    /// Create an iterator over visible lines in the buffer
    pub fn iter_lines<'a>(&self, buffer: &'a Buffer) -> RopeLineIter<'a> {
        RopeLineIter::new(buffer, self.line_viewport())
    }

    /// Count the total number of lines in a buffer
    pub fn count_lines(buffer: &Buffer) -> usize {
        let mut line_count = 1; // Start with 1 (empty buffer has 1 line)

        for token_idx in 0..buffer.rope().len_tokens() {
            if let Some(token) = buffer.rope().token_at(token_idx) {
                if let Some(text) = token.token_text() {
                    if text == "\n" {
                        line_count += 1;
                    }
                }
            }
        }

        line_count
    }
}

/// Zero-allocation iterator over text lines in a rope buffer
///
/// This iterator provides efficient line-by-line access within a specified viewport range
/// for optimal rendering performance.
pub struct RopeLineIter<'a> {
    /// Reference to the buffer containing the text
    buffer: &'a Buffer,

    /// Current line number being processed
    current_line: usize,

    /// End line number (exclusive)
    end_line: usize,

    /// Current token index in the rope
    current_token: usize,

    /// Accumulated text for the current line
    line_buffer: String,

    /// Whether we're at the end of iteration
    finished: bool,
}

impl<'a> RopeLineIter<'a> {
    /// Create a new line iterator for the specified viewport range
    pub fn new(buffer: &'a Buffer, viewport: Range<usize>) -> Self {
        Self {
            buffer,
            current_line: viewport.start,
            end_line: viewport.end,
            current_token: 0,
            line_buffer: String::new(),
            finished: viewport.is_empty(),
        }
    }

    /// Build the next line by collecting tokens until newline or end
    fn build_next_line(&mut self) -> Option<String> {
        if self.finished || self.current_line >= self.end_line {
            return None;
        }

        self.line_buffer.clear();
        let mut line_number = 0;
        let mut found_target_line = false;

        // If this is the first call, we need to skip to the starting line
        if self.current_token == 0 && self.current_line > 0 {
            // Skip to the target starting line
            for token_idx in 0..self.buffer.rope().len_tokens() {
                if let Some(token) = self.buffer.rope().token_at(token_idx) {
                    if let Some(text) = token.token_text() {
                        if text == "\n" {
                            line_number += 1;
                            if line_number == self.current_line {
                                self.current_token = token_idx + 1;
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Now collect text for the current target line
        for token_idx in self.current_token..self.buffer.rope().len_tokens() {
            if let Some(token) = self.buffer.rope().token_at(token_idx) {
                if let Some(text) = token.token_text() {
                    if text == "\n" {
                        // End of current line
                        found_target_line = true;
                        self.current_token = token_idx + 1;
                        self.current_line += 1;
                        break;
                    } else {
                        // Accumulate text for the current line
                        self.line_buffer.push_str(text);
                    }
                }
            }
        }

        if !found_target_line {
            // We've reached the end of tokens - finish the last line
            if !self.line_buffer.is_empty() {
                self.current_line += 1;
                self.current_token = self.buffer.rope().len_tokens();
            } else {
                self.finished = true;
                return None;
            }
        }

        Some(self.line_buffer.clone())
    }
}

impl<'a> Iterator for RopeLineIter<'a> {
    type Item = String; // FIXME: Should be &str for zero allocation

    fn next(&mut self) -> Option<Self::Item> {
        self.build_next_line()
    }
}

/// Convert rope cursor position to line/column for GUI rendering
pub fn cursor_to_line_col(buffer: &Buffer, cursor: &crate::cursor::Cursor) -> (usize, usize) {
    let mut line = 0;
    let mut col = 0;
    let mut current_token = 0;

    // Find which line the cursor's token is on
    while current_token < cursor.token_index() {
        if let Some(token) = buffer.rope().token_at(current_token) {
            if let Some(text) = token.token_text() {
                if text == "\n" {
                    line += 1;
                    col = 0;
                } else {
                    col += text.chars().count();
                }
            }
        }
        current_token += 1;
    }

    // Add the character offset within the current token
    col += cursor.char_offset();

    (line, col)
}

/// Convert line/column position to rope cursor position
pub fn line_col_to_cursor(
    buffer: &Buffer,
    line: usize,
    column: usize,
) -> Option<crate::cursor::Cursor> {
    let mut current_line = 0;
    let mut current_col = 0;

    for token_idx in 0..buffer.rope().len_tokens() {
        if let Some(token) = buffer.rope().token_at(token_idx) {
            if let Some(text) = token.token_text() {
                if text == "\n" {
                    if current_line == line && column <= current_col {
                        // Target position is on this line
                        let char_offset = column.saturating_sub(current_col - text.chars().count());
                        let mut cursor = crate::cursor::Cursor::new(buffer.id());
                        cursor.set_position(token_idx, char_offset);
                        return Some(cursor);
                    }
                    current_line += 1;
                    current_col = 0;
                } else {
                    let text_len = text.chars().count();
                    if current_line == line && column <= current_col + text_len {
                        // Target position is within this token
                        let char_offset = column - current_col;
                        let mut cursor = crate::cursor::Cursor::new(buffer.id());
                        cursor.set_position(token_idx, char_offset);
                        return Some(cursor);
                    }
                    current_col += text_len;
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;
    use stoat_rope::{RopeAst, ast::TextRange, builder::AstBuilder, kind::SyntaxKind};

    fn create_test_buffer() -> Buffer {
        // Create buffer with "hello\nworld\ntest" (3 lines)
        let tokens = vec![
            AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5)),
            AstBuilder::token(SyntaxKind::Newline, "\n", TextRange::new(5, 6)),
            AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11)),
            AstBuilder::token(SyntaxKind::Newline, "\n", TextRange::new(11, 12)),
            AstBuilder::token(SyntaxKind::Text, "test", TextRange::new(12, 16)),
        ];

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 16))
            .add_children(tokens)
            .finish();
        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 16))
            .add_child(paragraph)
            .finish();

        let rope = RopeAst::from_root(doc);
        Buffer::from_rope(rope, 1)
    }

    #[test]
    fn test_view_creation() {
        let buffer = create_test_buffer();
        let view = View::new(&buffer);

        assert_eq!(view.buffer_id(), 1);
        assert_eq!(view.scroll_line(), 0);
        assert_eq!(view.visible_lines, 50);
    }

    #[test]
    fn test_line_counting() {
        let buffer = create_test_buffer();
        let line_count = View::count_lines(&buffer);
        assert_eq!(line_count, 3);
    }

    #[test]
    fn test_line_iteration() {
        let buffer = create_test_buffer();
        let view = View::new(&buffer);

        let lines: Vec<String> = view.iter_lines(&buffer).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "hello");
        assert_eq!(lines[1], "world");
        assert_eq!(lines[2], "test");
    }

    #[test]
    fn test_viewport_limiting() {
        let buffer = create_test_buffer();
        let mut view = View::new(&buffer);

        // Set to show only 2 lines starting from line 0
        view.set_visible_lines(2);

        let lines: Vec<String> = view.iter_lines(&buffer).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "hello");
        assert_eq!(lines[1], "world");
    }

    #[test]
    fn test_scroll_position() {
        let buffer = create_test_buffer();
        let mut view = View::new(&buffer);

        // Scroll to start at line 1, show 2 lines
        view.set_scroll_line(1);
        view.set_visible_lines(2);

        let lines: Vec<String> = view.iter_lines(&buffer).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "world");
        assert_eq!(lines[1], "test");
    }

    #[test]
    fn test_cursor_conversion() {
        let buffer = create_test_buffer();
        let mut cursor = crate::cursor::Cursor::new(1);

        // Test cursor at start of second line (token 2, offset 0)
        cursor.set_position(2, 0); // "world" token
        let (line, col) = cursor_to_line_col(&buffer, &cursor);
        assert_eq!(line, 1);
        assert_eq!(col, 0);

        // Test cursor in middle of first line
        cursor.set_position(0, 2); // "hello" token, offset 2
        let (line, col) = cursor_to_line_col(&buffer, &cursor);
        assert_eq!(line, 0);
        assert_eq!(col, 2);
    }

    #[test]
    fn test_line_col_to_cursor() {
        let buffer = create_test_buffer();

        // Test converting line 1, column 2 to cursor
        let cursor = line_col_to_cursor(&buffer, 1, 2);
        assert!(cursor.is_some());

        let cursor = cursor.expect("cursor should be found");
        assert_eq!(cursor.token_index(), 2); // "world" token
        assert_eq!(cursor.char_offset(), 2); // offset 2 within "world"
    }
}
