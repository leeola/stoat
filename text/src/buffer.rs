//! Buffer management for text editing
//!
//! Provides the core [`Buffer`] type that wraps [`rope::RopeAst`] for efficient text storage
//! and manipulation. Buffers can be shared across multiple [`crate::node::Node`] instances,
//! allowing different views and cursors to operate on the same underlying text data.

use crate::cursor::Cursor;
use snafu::Snafu;
use stoat_rope::{RopeAst, builder::AstBuilder};

/// Errors that can occur during buffer operations
#[derive(Debug, Snafu)]
pub enum EditError {
    /// Token not found at the specified index
    #[snafu(display("Token not found at index {index}"))]
    TokenNotFound { index: usize },

    /// Character offset is beyond the token's text length
    #[snafu(display("Character offset {offset} is beyond token text length {max_length}"))]
    InvalidCharOffset { offset: usize, max_length: usize },

    /// The token is not a text token and cannot be edited
    #[snafu(display("Token at index {index} is not a text token (kind: {kind:?})"))]
    NotTextToken {
        index: usize,
        kind: stoat_rope::kind::SyntaxKind,
    },

    /// Internal rope operation failed
    #[snafu(display("Rope operation failed: {message}"))]
    RopeError { message: String },
}

/// A text buffer that can be shared across multiple nodes
///
/// The Buffer is the central data structure for text storage in the editor. It wraps
/// a [`RopeAst`] to provide efficient text manipulation while supporting multiple
/// concurrent views and cursors operating on the same text.
#[derive(Debug)]
#[allow(dead_code)]
pub struct Buffer {
    /// The underlying rope AST containing the text and structure
    rope: RopeAst,

    /// Unique identifier for this buffer
    id: u64,

    /// Optional file path if this buffer is associated with a file
    file_path: Option<std::path::PathBuf>,

    /// Language/syntax information for this buffer
    language: Option<String>,
}

impl Buffer {
    /// Create a new buffer from a RopeAst
    pub fn from_rope(rope: RopeAst, id: u64) -> Self {
        Self {
            rope,
            id,
            file_path: None,
            language: None,
        }
    }

    /// Get the buffer ID
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Get a reference to the underlying rope
    pub fn rope(&self) -> &RopeAst {
        &self.rope
    }

    /// Insert a character at the cursor position
    pub fn insert_char_at_cursor(
        &mut self,
        cursor: &mut Cursor,
        ch: char,
    ) -> Result<(), EditError> {
        // Get the current token at cursor position
        let token =
            self.rope
                .token_at(cursor.token_index())
                .ok_or_else(|| EditError::TokenNotFound {
                    index: cursor.token_index(),
                })?;

        // Check if it's a text token that can be edited
        let token_text = token.token_text().ok_or_else(|| EditError::NotTextToken {
            index: cursor.token_index(),
            kind: token.kind(),
        })?;

        // Validate char offset is within bounds
        let char_count = token_text.chars().count();
        if cursor.char_offset() > char_count {
            return Err(EditError::InvalidCharOffset {
                offset: cursor.char_offset(),
                max_length: char_count,
            });
        }

        // Build new text by inserting the character
        let chars: Vec<char> = token_text.chars().collect();
        let mut new_text = String::new();

        // Add characters before cursor
        for (i, &c) in chars.iter().enumerate() {
            if i == cursor.char_offset() {
                new_text.push(ch);
            }
            new_text.push(c);
        }

        // If cursor is at the end, append the character
        if cursor.char_offset() == chars.len() {
            new_text.push(ch);
        }

        // Create new token with updated text and updated range
        let old_range = token.range();
        let new_range =
            stoat_rope::ast::TextRange::new(old_range.start.0, old_range.start.0 + new_text.len());
        let new_token = (*AstBuilder::token(token.kind(), new_text, new_range)).clone();

        // Replace the token in the rope
        self.rope
            .replace(
                cursor.token_index()..cursor.token_index() + 1,
                vec![new_token],
            )
            .map_err(|e| EditError::RopeError {
                message: format!("{e:?}"),
            })?;

        // Update cursor position to after the inserted character
        cursor.set_position(cursor.token_index(), cursor.char_offset() + 1);

        Ok(())
    }

    /// Convert cursor position to byte offset
    pub fn cursor_to_byte_offset(&self, cursor: &Cursor) -> Option<usize> {
        // Get byte offset of the token start
        let token_start = self.rope.token_index_to_byte_offset(cursor.token_index())?;

        // Get the token to calculate character offset in bytes
        let token = self.rope.token_at(cursor.token_index())?;
        let token_text = token.token_text()?;

        // Convert character offset to byte offset within the token
        let chars: Vec<char> = token_text.chars().collect();
        if cursor.char_offset() > chars.len() {
            return None;
        }

        let byte_offset_in_token: usize = chars
            .iter()
            .take(cursor.char_offset())
            .map(|c| c.len_utf8())
            .sum();

        Some(token_start + byte_offset_in_token)
    }

    /// Convert byte offset to cursor position
    pub fn byte_offset_to_cursor(&self, offset: usize) -> Option<Cursor> {
        // Find which token contains this byte offset
        let token_index = self.rope.byte_offset_to_token_index(offset)?;

        // Get the token and its start position
        let token = self.rope.token_at(token_index)?;
        let token_start = self.rope.token_index_to_byte_offset(token_index)?;
        let token_text = token.token_text()?;

        // Calculate byte offset within the token
        let byte_offset_in_token = offset.saturating_sub(token_start);

        // Convert byte offset to character offset within the token
        let mut char_offset = 0;
        let mut current_byte_offset = 0;

        for ch in token_text.chars() {
            if current_byte_offset >= byte_offset_in_token {
                break;
            }
            current_byte_offset += ch.len_utf8();
            char_offset += 1;
        }

        // Create cursor at this position
        let mut cursor = Cursor::new(self.id);
        cursor.set_position(token_index, char_offset);
        Some(cursor)
    }

    /// Create cursor at start of buffer
    pub fn cursor_at_start(&self) -> Cursor {
        Cursor::new(self.id) // Already defaults to token 0, char 0
    }

    /// Create cursor at end of buffer
    pub fn cursor_at_end(&self) -> Cursor {
        let mut cursor = Cursor::new(self.id);
        if self.rope.len_tokens() > 0 {
            cursor.set_position(self.rope.len_tokens() - 1, 0);
            // Move to end of the last token
            if let Some(token) = self.rope.token_at(cursor.token_index()) {
                if let Some(text) = token.token_text() {
                    cursor.set_position(cursor.token_index(), text.chars().count());
                }
            }
        }
        cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_rope::{RopeAst, ast::TextRange, builder::AstBuilder, kind::SyntaxKind};

    #[test]
    fn test_simple_rope_replace() {
        // Test that exactly mimics the working rope test structure
        let tokens = vec![
            AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5)),
            AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(5, 6)),
            AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11)),
        ];

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 11))
            .add_children(tokens)
            .finish();
        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 11))
            .add_child(paragraph)
            .finish();

        let mut rope = RopeAst::from_root(doc);

        // Try direct replacement like the rope test does - replace first token
        let new_token = (*AstBuilder::token(SyntaxKind::Text, "hi", TextRange::new(0, 2))).clone();
        let result = rope.replace(0..1, vec![new_token]);
        assert!(result.is_ok(), "Replace should succeed: {result:?}");

        assert_eq!(rope.to_string(), "hi world");
    }

    #[test]
    fn test_insert_char_at_cursor() {
        // Create a multi-token AST structure like the working test
        let tokens = vec![
            AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5)),
            AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(5, 6)),
            AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11)),
        ];

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 11))
            .add_children(tokens)
            .finish();
        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 11))
            .add_child(paragraph)
            .finish();

        let rope = RopeAst::from_root(doc);
        let mut buffer = Buffer::from_rope(rope, 1);

        // Create cursor at position 2 in first token (between 'e' and 'l' in "hello")
        let mut cursor = Cursor::new(1);
        cursor.set_position(0, 2); // token 0, char offset 2

        // Insert 'x' to get "hexllo world"
        let result = buffer.insert_char_at_cursor(&mut cursor, 'x');
        assert!(
            result.is_ok(),
            "Character insertion should succeed: {result:?}"
        );

        // Check that the text was updated
        let updated_text = buffer.rope().to_string();
        assert_eq!(updated_text, "hexllo world");

        // Check that cursor moved to position 3
        assert_eq!(cursor.token_index(), 0);
        assert_eq!(cursor.char_offset(), 3);
    }

    #[test]
    fn test_cursor_position_conversion() {
        // Create AST with "hello world"
        let tokens = vec![
            AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5)),
            AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(5, 6)),
            AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11)),
        ];

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 11))
            .add_children(tokens)
            .finish();
        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 11))
            .add_child(paragraph)
            .finish();

        let rope = RopeAst::from_root(doc);
        let buffer = Buffer::from_rope(rope, 1);

        // Test cursor to byte offset conversion
        let mut cursor = Cursor::new(1);
        cursor.set_position(0, 2); // token 0 (hello), char 2 (between 'l' and 'l')

        let byte_offset = buffer.cursor_to_byte_offset(&cursor);
        assert_eq!(byte_offset, Some(2));

        // Test byte offset to cursor conversion
        let cursor_from_offset = buffer.byte_offset_to_cursor(7);
        assert!(cursor_from_offset.is_some());
        let cursor = cursor_from_offset.expect("cursor should be found at valid offset");
        assert_eq!(cursor.token_index(), 2); // "world" token
        assert_eq!(cursor.char_offset(), 1); // second character 'o'
    }

    #[test]
    fn test_cursor_movement_within_token() {
        // Create AST with "hello world"
        let tokens = vec![
            AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5)),
            AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(5, 6)),
            AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11)),
        ];

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 11))
            .add_children(tokens)
            .finish();
        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 11))
            .add_child(paragraph)
            .finish();

        let rope = RopeAst::from_root(doc);
        let buffer = Buffer::from_rope(rope, 1);

        // Test character movement within first token "hello"
        let mut cursor = Cursor::new(1);
        cursor.set_position(0, 2); // Middle of "hello"

        // Move char left
        assert!(cursor.move_char_left());
        assert_eq!(cursor.char_offset(), 1);

        // Move char left again
        assert!(cursor.move_char_left());
        assert_eq!(cursor.char_offset(), 0);

        // Can't move char left from start
        assert!(!cursor.move_char_left());
        assert_eq!(cursor.char_offset(), 0);

        // Move char right
        assert!(
            cursor
                .move_char_right(&buffer)
                .expect("move_char_right should succeed")
        );
        assert_eq!(cursor.char_offset(), 1);

        // Move to end of token
        cursor.set_position(0, 5); // End of "hello"
        assert!(
            !cursor
                .move_char_right(&buffer)
                .expect("move_char_right should succeed")
        ); // Already at end
        assert_eq!(cursor.char_offset(), 5);
    }

    #[test]
    fn test_cursor_movement_between_tokens() {
        // Create AST with "hello world"
        let tokens = vec![
            AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5)),
            AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(5, 6)),
            AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11)),
        ];

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 11))
            .add_children(tokens)
            .finish();
        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 11))
            .add_child(paragraph)
            .finish();

        let rope = RopeAst::from_root(doc);
        let buffer = Buffer::from_rope(rope, 1);

        // Start at first token
        let mut cursor = Cursor::new(1);
        cursor.set_position(0, 2); // Middle of "hello"

        // Move to next token (should go to start of " ")
        assert!(
            cursor
                .move_right(&buffer)
                .expect("move_right should succeed")
        );
        assert_eq!(cursor.token_index(), 1);
        assert_eq!(cursor.char_offset(), 0);

        // Move to next token (should go to start of "world")
        assert!(
            cursor
                .move_right(&buffer)
                .expect("move_right should succeed")
        );
        assert_eq!(cursor.token_index(), 2);
        assert_eq!(cursor.char_offset(), 0);

        // Can't move right from last token
        assert!(
            !cursor
                .move_right(&buffer)
                .expect("move_right should succeed")
        );
        assert_eq!(cursor.token_index(), 2);

        // Move back left (should go to end of " ")
        assert!(cursor.move_left(&buffer).expect("move_left should succeed"));
        assert_eq!(cursor.token_index(), 1);
        assert_eq!(cursor.char_offset(), 1); // End of " " (1 character)

        // Move back left again (should go to end of "hello")
        assert!(cursor.move_left(&buffer).expect("move_left should succeed"));
        assert_eq!(cursor.token_index(), 0);
        assert_eq!(cursor.char_offset(), 5); // End of "hello" (5 characters)

        // Can't move left from first token
        assert!(!cursor.move_left(&buffer).expect("move_left should succeed"));
        assert_eq!(cursor.token_index(), 0);
    }

    #[test]
    fn test_cursor_utility_methods() {
        // Create AST with "hello world"
        let tokens = vec![
            AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5)),
            AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(5, 6)),
            AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11)),
        ];

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 11))
            .add_children(tokens)
            .finish();
        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 11))
            .add_child(paragraph)
            .finish();

        let rope = RopeAst::from_root(doc);
        let buffer = Buffer::from_rope(rope, 1);

        // Test move_to_token_start
        let mut cursor = Cursor::new(1);
        cursor.set_position(0, 3); // Middle of "hello"
        cursor.move_to_token_start();
        assert_eq!(cursor.char_offset(), 0);

        // Test move_to_token_end
        cursor
            .move_to_token_end(&buffer)
            .expect("move_to_token_end should succeed");
        assert_eq!(cursor.char_offset(), 5); // End of "hello"

        // Test buffer cursor helpers
        let start_cursor = buffer.cursor_at_start();
        assert_eq!(start_cursor.token_index(), 0);
        assert_eq!(start_cursor.char_offset(), 0);

        let end_cursor = buffer.cursor_at_end();
        assert_eq!(end_cursor.token_index(), 2); // Last token "world"
        assert_eq!(end_cursor.char_offset(), 5); // End of "world"
    }
}
