//! Text buffer with rope storage and flat AST

use crate::{
    TextSize,
    edit::{Edit, EditError, EditOperation, FlatEdit, RopeEdit},
    range::TextRange,
    syntax::{FlatAst, FlatSyntaxNode, IncrementalParser, SyntaxNode, TextChange},
    view::TextView,
};
use parking_lot::RwLock;
use ropey::Rope;
use std::sync::{Arc, Weak};

/// A text buffer with efficient rope storage and flat AST
pub struct FlatTextBuffer {
    inner: Arc<FlatBufferInner>,
}

pub(crate) struct FlatBufferInner {
    /// Unique ID for this buffer
    id: crate::buffer::BufferId,
    /// The rope storing the actual text
    rope: RwLock<Rope>,
    /// Incremental parser with cached AST
    parser: RwLock<IncrementalParserCache>,
    /// Pending changes since last parse
    pending_changes: RwLock<Vec<TextChange>>,
    /// Version number for invalidation
    version: RwLock<u64>,
    /// All views of this buffer
    views: RwLock<Vec<Weak<crate::view::TextViewInner>>>,
}

struct IncrementalParserCache {
    /// The incremental parser
    parser: Option<IncrementalParser>,
    /// Version when this was last parsed
    version: u64,
}

impl FlatTextBuffer {
    /// Create a new buffer with the given text
    pub fn new(text: &str) -> Self {
        let rope = Rope::from_str(text);
        let inner = Arc::new(FlatBufferInner {
            id: crate::buffer::BufferId::new(),
            rope: RwLock::new(rope),
            parser: RwLock::new(IncrementalParserCache {
                parser: None,
                version: 0,
            }),
            pending_changes: RwLock::new(Vec::new()),
            version: RwLock::new(1),
            views: RwLock::new(Vec::new()),
        });

        // Parse initially
        let buffer = Self { inner };
        buffer.ensure_parsed();
        buffer
    }

    /// Get the buffer ID
    pub fn id(&self) -> crate::buffer::BufferId {
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

    /// Get the root syntax node (returns flat node)
    pub fn flat_syntax(&self) -> FlatSyntaxNode {
        self.ensure_parsed();
        let parser_cache = self.inner.parser.read();
        let parser = parser_cache
            .parser
            .as_ref()
            .expect("Parser should be initialized");
        let ast = parser.ast();

        FlatSyntaxNode::new(Arc::new(ast.clone()), ast.root())
    }

    /// Get the root syntax node (legacy compatibility)
    pub fn syntax(&self) -> SyntaxNode {
        self.flat_syntax()
            .to_legacy()
            .expect("Root node should exist")
    }

    /// Get the flat AST directly
    pub fn flat_ast(&self) -> Arc<FlatAst> {
        self.ensure_parsed();
        let parser_cache = self.inner.parser.read();
        let parser = parser_cache
            .parser
            .as_ref()
            .expect("Parser should be initialized");
        Arc::new(parser.ast().clone())
    }

    /// Create a view of the entire buffer
    pub fn create_view(&self) -> TextView {
        TextView::new(self.to_legacy_buffer(), self.syntax())
    }

    /// Create a view of a specific node
    pub fn create_view_of(&self, node: SyntaxNode) -> TextView {
        TextView::new(self.to_legacy_buffer(), node)
    }

    /// Convert to legacy buffer type for compatibility
    fn to_legacy_buffer(&self) -> crate::buffer::TextBuffer {
        // This is a temporary method for compatibility
        // In a real implementation, we'd update TextView to work with FlatTextBuffer
        crate::buffer::TextBuffer::new(&self.text())
    }

    /// Find nodes matching a predicate using flat traversal
    pub fn find_nodes(&self, predicate: impl Fn(&FlatSyntaxNode) -> bool) -> Vec<FlatSyntaxNode> {
        let mut results = Vec::new();
        let root = self.flat_syntax();
        Self::find_nodes_recursive(&root, &predicate, &mut results);
        results
    }

    fn find_nodes_recursive(
        node: &FlatSyntaxNode,
        predicate: &impl Fn(&FlatSyntaxNode) -> bool,
        results: &mut Vec<FlatSyntaxNode>,
    ) {
        if predicate(node) {
            results.push(node.clone());
        }

        // Traverse children
        for child in node.children() {
            Self::find_nodes_recursive(&child, predicate, results);
        }
    }

    /// Ensure the AST is parsed and up to date using incremental parsing
    fn ensure_parsed(&self) {
        let current_version = *self.inner.version.read();
        let mut parser_cache = self.inner.parser.write();

        // Check if we need to parse or reparse
        if parser_cache.version < current_version || parser_cache.parser.is_none() {
            let text = self.text();

            if parser_cache.parser.is_none() {
                // First parse - create parser with full AST
                let flat_ast = crate::syntax::parse::parse_markdown_to_flat_ast(&text);
                parser_cache.parser = Some(IncrementalParser::new(flat_ast));
            } else {
                // Incremental parse - apply pending changes
                let mut pending_changes = self.inner.pending_changes.write();
                if let Some(ref mut parser) = parser_cache.parser {
                    // Apply all pending changes
                    for change in pending_changes.drain(..) {
                        if let Err(e) = parser.apply_change(&change) {
                            // If incremental parsing fails, fall back to full reparse
                            eprintln!(
                                "Incremental parsing failed: {e}, falling back to full reparse"
                            );
                            let flat_ast = crate::syntax::parse::parse_markdown_to_flat_ast(&text);
                            *parser = IncrementalParser::new(flat_ast);
                            break;
                        }
                    }

                    // Perform incremental reparse
                    if let Err(e) = parser.reparse(&text) {
                        // If incremental reparse fails, fall back to full reparse
                        eprintln!("Incremental reparse failed: {e}, falling back to full reparse");
                        let flat_ast = crate::syntax::parse::parse_markdown_to_flat_ast(&text);
                        *parser = IncrementalParser::new(flat_ast);
                    }
                }
            }

            parser_cache.version = current_version;
        }
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

        // Create text change for incremental parsing
        let text_change = TextChange::new(change_range, deleted_len, inserted_len);

        // Add to pending changes
        self.inner.pending_changes.write().push(text_change);

        // Increment version
        let new_version = {
            let mut version = self.inner.version.write();
            *version += 1;
            *version
        };

        // Create change event
        let event = crate::buffer::ChangeEvent {
            range: change_range,
            deleted_len,
            inserted_len,
            new_version,
        };

        // Notify views of the change
        self.notify_views(&event);

        Ok(())
    }

    /// Apply a flat edit to the buffer (native flat edit support)
    pub fn apply_flat_edit(&self, edit: &FlatEdit) -> Result<(), EditError> {
        // For now, convert to legacy edit and apply
        // FIXME: This should be implemented more efficiently
        let ast = self.flat_ast();
        let legacy_edit = edit.to_legacy(&ast).ok_or(EditError::NodeNotFound)?;
        self.apply_edit(&legacy_edit)
    }

    /// Get access to the incremental parser for advanced operations
    pub fn get_incremental_parser(&self) -> Option<IncrementalParser> {
        self.ensure_parsed();
        let parser_cache = self.inner.parser.read();
        parser_cache.parser.as_ref().map(|p| {
            // FIXME: This clones the entire parser, which is not ideal
            // In practice, we'd want to provide a different API
            IncrementalParser::new(p.ast().clone())
        })
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

                let range = TextRange::new(abs_start, abs_end);
                Ok(RopeEdit::delete(range))
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

                let range = TextRange::new(abs_start, abs_end);
                Ok(RopeEdit::replace(range, text.clone()))
            },
        }
    }

    /// Apply a rope edit
    fn apply_rope_edit(&self, edit: &RopeEdit) -> Result<(), EditError> {
        let mut rope = self.inner.rope.write();

        // Convert byte positions to char indices
        let start_char = rope.byte_to_char(edit.range.start().into());
        let end_char = rope.byte_to_char(edit.range.end().into());

        // Remove the old text
        rope.remove(start_char..end_char);

        // Insert the new text
        if !edit.text.is_empty() {
            rope.insert(start_char, &edit.text);
        }

        Ok(())
    }

    /// Notify all views of a change
    fn notify_views(&self, event: &crate::buffer::ChangeEvent) {
        let mut views = self.inner.views.write();

        // Remove any dead weak references
        views.retain(|weak| weak.strong_count() > 0);

        // Notify remaining views
        for weak_view in views.iter() {
            if let Some(view) = weak_view.upgrade() {
                view.on_buffer_change(event);
            }
        }
    }
}

impl Clone for FlatTextBuffer {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::kind::SyntaxKind;

    #[test]
    fn test_flat_buffer_creation() {
        let buffer = FlatTextBuffer::new("hello world");
        assert_eq!(buffer.text(), "hello world");
        assert_eq!(buffer.len(), 11);
        assert!(!buffer.is_empty());
    }

    #[test]
    fn test_flat_ast_access() {
        let buffer = FlatTextBuffer::new("hello world");

        // Get flat syntax node - markdown parser creates Document root
        let root = buffer.flat_syntax();
        assert_eq!(root.kind(), Some(SyntaxKind::Document));

        // Get flat AST
        let ast = buffer.flat_ast();
        assert!(ast.get_node(ast.root()).is_some());
    }

    #[test]
    fn test_legacy_compatibility() {
        let buffer = FlatTextBuffer::new("hello world");

        // Get legacy syntax node - markdown parser creates Document root
        let root = buffer.syntax();
        assert_eq!(root.kind(), SyntaxKind::Document);
        assert_eq!(root.text_range(), TextRange::new(0.into(), 11.into()));
    }

    #[test]
    fn test_markdown_parsing() {
        let markdown_text = "# Hello World\n\nThis is a **bold** paragraph with `code`.";
        let buffer = FlatTextBuffer::new(markdown_text);

        // Verify the buffer contains the text
        assert_eq!(buffer.text(), markdown_text);

        // Get the flat syntax node
        let root = buffer.flat_syntax();
        assert_eq!(root.kind(), Some(SyntaxKind::Document));

        // The markdown parser should create a proper tree structure
        let ast = buffer.flat_ast();
        let root_id = ast.root();
        let root_node = ast.get_node(root_id).expect("Root node should exist");
        assert_eq!(root_node.kind, SyntaxKind::Document);
    }

    #[test]
    fn test_markdown_with_multiple_blocks() {
        let markdown = r#"# Title

First paragraph.

## Subtitle

Second paragraph with **emphasis**.

```rust
let code = "block";
```

> A quote
"#;
        let buffer = FlatTextBuffer::new(markdown);

        // Just ensure it parses without panicking
        let root = buffer.flat_syntax();
        assert_eq!(root.kind(), Some(SyntaxKind::Document));

        // Verify text is preserved
        assert_eq!(buffer.text(), markdown);
    }
}
