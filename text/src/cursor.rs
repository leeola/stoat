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

    /// Move cursor one character left within current token
    /// Returns true if moved, false if already at start of token
    pub fn move_char_left(&mut self) -> bool {
        if self.char_offset > 0 {
            self.char_offset -= 1;
            true
        } else {
            false // At start of token, can't move further
        }
    }

    /// Move cursor one character right within current token
    /// Returns true if moved, false if already at end of token
    pub fn move_char_right(
        &mut self,
        buffer: &crate::buffer::Buffer,
    ) -> Result<bool, crate::buffer::EditError> {
        let token = buffer.rope().token_at(self.token_index).ok_or({
            crate::buffer::EditError::TokenNotFound {
                index: self.token_index,
            }
        })?;
        let token_text =
            token
                .token_text()
                .ok_or_else(|| crate::buffer::EditError::NotTextToken {
                    index: self.token_index,
                    kind: token.kind(),
                })?;

        let char_count = token_text.chars().count();
        if self.char_offset < char_count {
            self.char_offset += 1;
            Ok(true)
        } else {
            Ok(false) // At end of token, can't move further
        }
    }

    /// Move cursor to previous token (sets char_offset to end of previous token)
    /// Returns true if moved, false if already at first token
    pub fn move_left(
        &mut self,
        buffer: &crate::buffer::Buffer,
    ) -> Result<bool, crate::buffer::EditError> {
        if self.token_index > 0 {
            self.token_index -= 1;
            // Move to end of the new token
            let token = buffer.rope().token_at(self.token_index).ok_or({
                crate::buffer::EditError::TokenNotFound {
                    index: self.token_index,
                }
            })?;
            if let Some(token_text) = token.token_text() {
                self.char_offset = token_text.chars().count();
            } else {
                self.char_offset = 0;
            }
            Ok(true)
        } else {
            Ok(false) // Already at first token
        }
    }

    /// Move cursor to next token (sets char_offset to start of next token)
    /// Returns true if moved, false if already at last token
    pub fn move_right(
        &mut self,
        buffer: &crate::buffer::Buffer,
    ) -> Result<bool, crate::buffer::EditError> {
        let total_tokens = buffer.rope().len_tokens();
        if self.token_index + 1 < total_tokens {
            self.token_index += 1;
            self.char_offset = 0; // Start of new token
            Ok(true)
        } else {
            Ok(false) // Already at last token
        }
    }

    /// Move to start of current token
    pub fn move_to_token_start(&mut self) {
        self.char_offset = 0;
    }

    /// Move to end of current token
    pub fn move_to_token_end(
        &mut self,
        buffer: &crate::buffer::Buffer,
    ) -> Result<(), crate::buffer::EditError> {
        let token = buffer.rope().token_at(self.token_index).ok_or({
            crate::buffer::EditError::TokenNotFound {
                index: self.token_index,
            }
        })?;
        if let Some(token_text) = token.token_text() {
            self.char_offset = token_text.chars().count();
        }
        Ok(())
    }
}
