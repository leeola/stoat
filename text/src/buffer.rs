//! Text buffer with rope storage and lazy AST

use crate::{
    TextSize,
    edit::{Edit, EditError, EditOperation, RopeEdit},
    range::TextRange,
    syntax::SyntaxNode,
    view::TextView,
};
use parking_lot::RwLock;
use ropey::Rope;
use std::sync::{Arc, Weak};

/// Unique identifier for a buffer
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId(u64);

impl BufferId {
    pub(crate) fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// Event describing a change to the buffer
#[derive(Debug, Clone)]
pub struct ChangeEvent {
    /// The range that was affected
    pub range: TextRange,
    /// The length of text that was deleted
    pub deleted_len: TextSize,
    /// The length of text that was inserted
    pub inserted_len: TextSize,
    /// The new version number after the change
    pub new_version: u64,
}

/// A text buffer with efficient rope storage and lazy AST
pub struct TextBuffer {
    inner: Arc<BufferInner>,
}

pub(crate) struct BufferInner {
    /// Unique ID for this buffer
    id: BufferId,
    /// The rope storing the actual text
    rope: RwLock<Rope>,
    /// Cached AST with version tracking
    ast: RwLock<AstCache>,
    /// Version number for invalidation
    version: RwLock<u64>,
    /// All views of this buffer
    views: RwLock<Vec<Weak<crate::view::TextViewInner>>>,
}

struct AstCache {
    /// The cached root node
    root: Option<SyntaxNode>,
    /// Version when this was parsed
    version: u64,
}

impl TextBuffer {
    /// Create a new buffer with the given text
    pub fn new(text: &str) -> Self {
        let rope = Rope::from_str(text);
        let inner = Arc::new(BufferInner {
            id: BufferId::new(),
            rope: RwLock::new(rope),
            ast: RwLock::new(AstCache {
                root: None,
                version: 0,
            }),
            version: RwLock::new(1),
            views: RwLock::new(Vec::new()),
        });

        // Parse initially
        let buffer = Self { inner };
        buffer.ensure_parsed();
        buffer
    }

    /// Get the buffer ID
    pub fn id(&self) -> BufferId {
        self.inner.id
    }

    /// Get the current text as a string
    pub fn text(&self) -> String {
        self.inner.rope.read().to_string()
    }

    /// Get the length in bytes
    pub fn len(&self) -> usize {
        self.inner.rope.read().len_bytes()
    }

    /// Check if the buffer is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the root syntax node
    pub fn syntax(&self) -> SyntaxNode {
        self.ensure_parsed();
        self.inner
            .ast
            .read()
            .root
            .as_ref()
            .expect("AST should be parsed")
            .clone()
    }

    /// Create a view of the entire buffer
    pub fn create_view(&self) -> TextView {
        TextView::new(self.clone(), self.syntax())
    }

    /// Create a view of a specific node
    pub fn create_view_of(&self, node: SyntaxNode) -> TextView {
        TextView::new(self.clone(), node)
    }

    /// Find nodes matching a predicate
    pub fn find_nodes(&self, predicate: impl Fn(&SyntaxNode) -> bool) -> Vec<SyntaxNode> {
        let mut results = Vec::new();
        let root = self.syntax();
        self.find_nodes_recursive(&root, &predicate, &mut results);
        results
    }

    fn find_nodes_recursive(
        &self,
        node: &SyntaxNode,
        predicate: &impl Fn(&SyntaxNode) -> bool,
        results: &mut Vec<SyntaxNode>,
    ) {
        if predicate(node) {
            results.push(node.clone());
        }
        // TODO: Iterate through children when node parsing is implemented
    }

    /// Ensure the AST is parsed and up to date
    fn ensure_parsed(&self) {
        let current_version = *self.inner.version.read();
        let mut ast = self.inner.ast.write();

        if ast.version < current_version || ast.root.is_none() {
            // Need to parse
            let text = self.text();
            let parse_result = crate::syntax::parse::parse(&text);
            ast.root = Some(parse_result.root);
            ast.version = current_version;
        }
    }

    /// Register a view
    pub(crate) fn register_view(&self, view: Weak<crate::view::TextViewInner>) {
        self.inner.views.write().push(view);
    }

    /// Apply an edit to the buffer
    pub fn apply_edit(&self, edit: &Edit) -> Result<(), EditError> {
        // Convert AST edit to rope edit
        let rope_edit = self.convert_to_rope_edit(edit)?;

        // Calculate change metrics before applying
        let deleted_len = rope_edit.range.len();
        let inserted_len = TextSize::from(rope_edit.text.len() as u32);
        let change_range = rope_edit.range;

        // Apply to rope
        self.apply_rope_edit(&rope_edit)?;

        // Increment version and invalidate AST
        let new_version = {
            let mut version = self.inner.version.write();
            *version += 1;

            // Clear AST cache
            let mut ast = self.inner.ast.write();
            ast.root = None;

            *version
        };

        // Create change event
        let event = ChangeEvent {
            range: change_range,
            deleted_len,
            inserted_len,
            new_version,
        };

        // Notify views of the change
        self.notify_views(&event);

        Ok(())
    }

    /// Convert an AST-based edit to a rope edit
    fn convert_to_rope_edit(&self, edit: &Edit) -> Result<RopeEdit, EditError> {
        let node_range = edit.target.text_range();

        match &edit.operation {
            EditOperation::Replace(text) => Ok(RopeEdit::replace(node_range, text.clone())),
            EditOperation::InsertBefore(text) => {
                Ok(RopeEdit::insert(node_range.start(), text.clone()))
            },
            EditOperation::InsertAfter(text) => {
                Ok(RopeEdit::insert(node_range.end(), text.clone()))
            },
            EditOperation::InsertAt { offset, text } => {
                // Convert offset relative to node to absolute position
                let position = node_range.start() + TextSize::from(*offset as u32);
                if position > node_range.end() {
                    return Err(EditError::RangeOutOfBounds {
                        position,
                        length: node_range.end(),
                    });
                }
                Ok(RopeEdit::insert(position, text.clone()))
            },
            EditOperation::Delete => Ok(RopeEdit::delete(node_range)),
            EditOperation::WrapWith { before, after } => {
                // This requires two edits, for now just do a simple replace
                let current_text = edit.target.text();
                let wrapped = format!("{before}{current_text}{after}");
                Ok(RopeEdit::replace(node_range, wrapped))
            },
            EditOperation::Unwrap => {
                // This is complex and requires understanding node structure
                // For now, return an error
                Err(EditError::NodeNotFound)
            },
            EditOperation::DeleteRange { start, end } => {
                // Convert node-relative offsets to absolute positions
                let abs_start = node_range.start() + TextSize::from(*start as u32);
                let abs_end = node_range.start() + TextSize::from(*end as u32);

                // Validate bounds
                if abs_end > node_range.end() {
                    return Err(EditError::RangeOutOfBounds {
                        position: abs_end,
                        length: node_range.end(),
                    });
                }

                Ok(RopeEdit::delete(TextRange::new(abs_start, abs_end)))
            },
            EditOperation::ReplaceRange { start, end, text } => {
                // Convert node-relative offsets to absolute positions
                let abs_start = node_range.start() + TextSize::from(*start as u32);
                let abs_end = node_range.start() + TextSize::from(*end as u32);

                // Validate bounds
                if abs_end > node_range.end() {
                    return Err(EditError::RangeOutOfBounds {
                        position: abs_end,
                        length: node_range.end(),
                    });
                }

                Ok(RopeEdit::replace(
                    TextRange::new(abs_start, abs_end),
                    text.clone(),
                ))
            },
        }
    }

    /// Apply a rope edit
    fn apply_rope_edit(&self, edit: &RopeEdit) -> Result<(), EditError> {
        let mut rope = self.inner.rope.write();
        let rope_len = rope.len_bytes();

        // Validate range
        let start_byte = edit.range.start().into();
        let end_byte = edit.range.end().into();

        if start_byte > end_byte {
            return Err(EditError::InvalidRange {
                start: edit.range.start(),
                end: edit.range.end(),
            });
        }

        if end_byte > rope_len {
            return Err(EditError::RangeOutOfBounds {
                position: edit.range.end(),
                length: TextSize::from(rope_len as u32),
            });
        }

        // Convert byte indices to char indices (ropey works with char indices)
        let start_char = rope.byte_to_char(start_byte);
        let end_char = rope.byte_to_char(end_byte);

        // Apply the edit
        rope.remove(start_char..end_char);
        rope.insert(start_char, &edit.text);

        Ok(())
    }

    /// Notify all registered views of a change
    fn notify_views(&self, event: &ChangeEvent) {
        let mut views = self.inner.views.write();

        // Remove any dead weak references
        views.retain(|weak| weak.strong_count() > 0);

        // Notify each view
        for view_weak in views.iter() {
            if let Some(view) = view_weak.upgrade() {
                view.on_buffer_change(event);
            }
        }
    }

    /// Get the number of lines in the buffer
    pub fn line_count(&self) -> usize {
        self.inner.rope.read().len_lines()
    }

    /// Get line at index (0-indexed)
    pub fn line(&self, line_idx: usize) -> Option<String> {
        let rope = self.inner.rope.read();
        if line_idx < rope.len_lines() {
            Some(rope.line(line_idx).to_string())
        } else {
            None
        }
    }

    /// Convert byte offset to line number (0-indexed)
    pub fn offset_to_line(&self, offset: TextSize) -> usize {
        let rope = self.inner.rope.read();
        let byte_idx = u32::from(offset) as usize;
        if byte_idx > rope.len_bytes() {
            rope.len_lines().saturating_sub(1)
        } else {
            let char_idx = rope.byte_to_char(byte_idx);
            rope.char_to_line(char_idx)
        }
    }

    /// Get byte offset of line start
    pub fn line_to_offset(&self, line_idx: usize) -> TextSize {
        let rope = self.inner.rope.read();
        if line_idx >= rope.len_lines() {
            TextSize::from(rope.len_bytes() as u32)
        } else {
            let char_idx = rope.line_to_char(line_idx);
            let byte_idx = rope.char_to_byte(char_idx);
            TextSize::from(byte_idx as u32)
        }
    }

    /// Get the start of the line containing the given offset
    pub fn line_start_offset(&self, offset: TextSize) -> TextSize {
        let line_idx = self.offset_to_line(offset);
        self.line_to_offset(line_idx)
    }

    /// Get the end of the line containing the given offset (including newline if present)
    pub fn line_end_offset(&self, offset: TextSize) -> TextSize {
        let rope = self.inner.rope.read();
        let line_idx = self.offset_to_line(offset);

        if line_idx >= rope.len_lines() {
            return TextSize::from(rope.len_bytes() as u32);
        }

        // Get the start of the next line
        if line_idx + 1 < rope.len_lines() {
            let next_line_char = rope.line_to_char(line_idx + 1);
            let next_line_byte = rope.char_to_byte(next_line_char);
            TextSize::from(next_line_byte as u32)
        } else {
            // Last line, return end of buffer
            TextSize::from(rope.len_bytes() as u32)
        }
    }

    /// Check if offset is at a word boundary
    pub fn is_word_boundary(&self, offset: TextSize) -> bool {
        let rope = self.inner.rope.read();
        let byte_idx = u32::from(offset) as usize;

        if byte_idx == 0 || byte_idx >= rope.len_bytes() {
            return true;
        }

        let char_idx = rope.byte_to_char(byte_idx);
        if char_idx == 0 || char_idx >= rope.len_chars() {
            return true;
        }

        // Get the character at this position and the previous one
        let curr_char = rope.char(char_idx);
        let prev_char = rope.char(char_idx - 1);

        // Word boundary if transitioning between word and non-word chars
        curr_char.is_whitespace() != prev_char.is_whitespace()
            || (curr_char.is_alphanumeric() != prev_char.is_alphanumeric()
                && !curr_char.is_whitespace()
                && !prev_char.is_whitespace())
    }

    /// Find the next word boundary after the given offset
    pub fn next_word_boundary(&self, offset: TextSize) -> TextSize {
        let rope = self.inner.rope.read();
        let mut byte_idx = u32::from(offset) as usize;
        let len = rope.len_bytes();

        if byte_idx >= len {
            return TextSize::from(len as u32);
        }

        // Skip current word
        let mut char_idx = rope.byte_to_char(byte_idx);
        while char_idx < rope.len_chars() {
            let ch = rope.char(char_idx);
            if ch.is_whitespace() {
                break;
            }
            char_idx += 1;
        }

        // Skip whitespace
        while char_idx < rope.len_chars() {
            let ch = rope.char(char_idx);
            if !ch.is_whitespace() {
                break;
            }
            char_idx += 1;
        }

        byte_idx = rope.char_to_byte(char_idx);
        TextSize::from(byte_idx as u32)
    }

    /// Find the previous word boundary before the given offset
    pub fn prev_word_boundary(&self, offset: TextSize) -> TextSize {
        let rope = self.inner.rope.read();
        let byte_idx = u32::from(offset) as usize;

        if byte_idx == 0 {
            return TextSize::from(0);
        }

        let mut char_idx = rope.byte_to_char(byte_idx);
        if char_idx > 0 {
            char_idx -= 1;

            // Skip whitespace backwards
            while char_idx > 0 {
                let ch = rope.char(char_idx);
                if !ch.is_whitespace() {
                    break;
                }
                char_idx = char_idx.saturating_sub(1);
            }

            // Skip word backwards
            while char_idx > 0 {
                let ch = rope.char(char_idx - 1);
                if ch.is_whitespace() {
                    break;
                }
                char_idx -= 1;
            }
        }

        let byte_idx = rope.char_to_byte(char_idx);
        TextSize::from(byte_idx as u32)
    }
}

impl Clone for TextBuffer {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{edit::EditOperation, test_helpers::*};

    #[test]
    fn test_buffer_creation() {
        let buffer = simple_buffer("hello world");
        assert_eq!(buffer.text(), "hello world");
        assert_eq!(buffer.len(), 11);
        assert!(!buffer.is_empty());
    }

    #[test]
    fn test_empty_buffer() {
        let buffer = simple_buffer("");
        assert_eq!(buffer.text(), "");
        assert_eq!(buffer.len(), 0);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_apply_replace_edit() {
        let buffer = simple_buffer("hello world");
        assert_buffer_text(&buffer, &apply_replace(&buffer, "goodbye world"));
    }

    #[test]
    fn test_apply_insert_before_edit() {
        let buffer = simple_buffer("world");
        assert_eq!(apply_insert_before(&buffer, "hello "), "hello world");
    }

    #[test]
    fn test_apply_insert_after_edit() {
        let buffer = simple_buffer("hello");
        assert_eq!(apply_insert_after(&buffer, " world"), "hello world");
    }

    #[test]
    fn test_apply_delete_edit() {
        let buffer = simple_buffer("hello world");
        assert_eq!(apply_delete(&buffer), "");
    }

    #[test]
    fn test_apply_wrap_edit() {
        let buffer = simple_buffer("content");
        let root = buffer.syntax();
        let edit = Edit {
            target: root,
            operation: EditOperation::WrapWith {
                before: "(".to_string(),
                after: ")".to_string(),
            },
        };
        buffer.apply_edit(&edit).expect("Edit should succeed");
        assert_buffer_text(&buffer, "(content)");
    }

    #[test]
    fn test_version_increments_on_edit() {
        let buffer = simple_buffer("hello");
        let initial_version = *buffer.inner.version.read();
        apply_replace(&buffer, "world");
        let new_version = *buffer.inner.version.read();
        assert_eq!(new_version, initial_version + 1);
    }
}
