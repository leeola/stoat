//! Rope-based AST implementation for efficient text manipulation

pub mod ast;
pub mod batch;
pub mod builder;
pub mod cursor;
pub mod edit;
pub mod iter;
pub mod kind;
pub mod query;
pub mod semantic;

use ast::{AstError, AstNode, TextInfo, TextPos, TextRange};
pub use batch::{BatchBuilder, BatchedEdit};
pub use builder::{AstBuilder, NodeBuilder};
pub use cursor::AstCursor;
use edit::{EditOp, apply_edit};
pub use iter::{FilteredNodeIter, LineIter, NodeIter, TextChunkIter, TokenIter, TraversalOrder};
pub use query::{PathQuery, Query, QueryResult, QueryUtils};
pub use semantic::{SemanticId, SemanticInfo, SemanticKind};
use std::{fmt, sync::Arc};

/// A rope-based Abstract Syntax Tree for efficient text editing
pub struct RopeAst {
    /// Root node of the AST
    root: Arc<AstNode>,
    /// Total text info for quick access
    info: TextInfo,
}

impl RopeAst {
    /// Create a new RopeAst from a pre-built root node
    pub fn from_root(root: Arc<AstNode>) -> Self {
        let info = root.text_info();
        Self { root, info }
    }

    /// Get the root node
    pub fn root(&self) -> &Arc<AstNode> {
        &self.root
    }

    /// Get total text info
    pub fn text_info(&self) -> TextInfo {
        self.info
    }

    /// Get the total byte length
    pub fn len_bytes(&self) -> usize {
        self.info.bytes
    }

    /// Get the total character count
    pub fn len_chars(&self) -> usize {
        self.info.chars
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.info.bytes == 0
    }

    /// Extract text for a given range
    pub fn text_at_range(&self, range: TextRange) -> String {
        let mut result = String::new();
        Self::collect_text_in_range(&self.root, range, &mut result);
        result
    }

    /// Helper to collect text within a range
    fn collect_text_in_range(node: &AstNode, target_range: TextRange, buffer: &mut String) {
        let node_range = node.range();

        // Skip if no overlap
        if node_range.end.0 <= target_range.start.0 || node_range.start.0 >= target_range.end.0 {
            return;
        }

        match node {
            AstNode::Token { text, .. } => {
                // Calculate overlap
                let start = node_range.start.0.max(target_range.start.0);
                let end = node_range.end.0.min(target_range.end.0);

                if start < end {
                    let local_start = start - node_range.start.0;
                    let local_end = end - node_range.start.0;

                    // Convert byte offsets to char boundaries
                    let chars: Vec<char> = text.chars().collect();
                    let mut byte_pos = 0;
                    let mut char_start = 0;
                    let mut char_end = chars.len();

                    for (i, ch) in chars.iter().enumerate() {
                        if byte_pos == local_start {
                            char_start = i;
                        }
                        if byte_pos == local_end {
                            char_end = i;
                            break;
                        }
                        byte_pos += ch.len_utf8();
                    }

                    buffer.extend(chars[char_start..char_end].iter());
                }
            },
            AstNode::Syntax { children, .. } => {
                for (child, _) in children {
                    Self::collect_text_in_range(child, target_range, buffer);
                }
            },
        }
    }

    /// Find the node containing the given offset
    pub fn find_node_at_offset(&self, offset: usize) -> Option<&AstNode> {
        Self::find_node_at_offset_impl(&self.root, TextPos(offset))
    }

    /// Insert text at the given offset
    pub fn insert(&mut self, offset: usize, text: &str) -> Result<(), AstError> {
        let edit = EditOp::Insert {
            offset,
            text: text.into(),
        };
        self.apply_edit(edit)
    }

    /// Delete text in the given range
    pub fn delete(&mut self, range: TextRange) -> Result<(), AstError> {
        let edit = EditOp::Delete { range };
        self.apply_edit(edit)
    }

    /// Replace text in the given range
    pub fn replace(&mut self, range: TextRange, text: &str) -> Result<(), AstError> {
        let edit = EditOp::Replace {
            range,
            text: text.into(),
        };
        self.apply_edit(edit)
    }

    /// Apply an edit operation
    fn apply_edit(&mut self, edit: EditOp) -> Result<(), AstError> {
        self.root = apply_edit(&self.root, &edit)?;
        self.info = self.root.text_info();
        Ok(())
    }

    /// Apply a batch of edits
    pub fn apply_batch(&mut self, batch: &BatchedEdit) -> Result<(), AstError> {
        self.root = batch.apply(&self.root)?;
        self.info = self.root.text_info();
        Ok(())
    }

    fn find_node_at_offset_impl(node: &AstNode, pos: TextPos) -> Option<&AstNode> {
        let range = node.range();

        // Check if position is within this node
        if pos.0 < range.start.0 || pos.0 >= range.end.0 {
            return None;
        }

        // If this is a syntax node, try to find a more specific child
        if let Some(children) = node.children() {
            for (child, _) in children {
                if let Some(found) = Self::find_node_at_offset_impl(child, pos) {
                    return Some(found);
                }
            }
        }

        // Return this node if no child contains the position
        Some(node)
    }

    /// Create a new cursor at the beginning of the rope
    pub fn cursor(&self) -> AstCursor {
        AstCursor::new(self.root.clone())
    }

    /// Create an iterator over all nodes in pre-order
    pub fn iter_nodes(&self) -> NodeIter<'_> {
        NodeIter::pre_order(&self.root)
    }

    /// Create an iterator over all tokens
    pub fn iter_tokens(&self) -> TokenIter<'_> {
        TokenIter::new(&self.root)
    }

    /// Create an iterator over lines
    pub fn iter_lines(&self) -> LineIter<'_> {
        LineIter::new(&self.root)
    }

    /// Create an iterator over text chunks
    pub fn iter_chunks(&self, chunk_size: usize) -> TextChunkIter<'_> {
        TextChunkIter::new(&self.root, chunk_size)
    }

    /// Create a query builder for this AST
    pub fn query(&self) -> Query<'_> {
        Query::new(&self.root)
    }

    /// Create a path query builder for this AST
    pub fn path_query(&self) -> PathQuery<'_> {
        PathQuery::new(&self.root)
    }
}

impl fmt::Display for RopeAst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut text = String::new();
        self.root.collect_text(&mut text);
        write!(f, "{text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::SyntaxKind;

    #[test]
    fn test_rope_ast_creation() {
        // Build AST using builder
        let token1 = AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5));
        let token2 = AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(5, 6));
        let token3 = AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11));

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 11))
            .add_child(token1)
            .add_child(token2)
            .add_child(token3)
            .finish();

        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 11))
            .add_child(paragraph)
            .finish();

        let rope = RopeAst::from_root(doc);

        assert_eq!(rope.to_string(), "hello world");
        assert_eq!(rope.len_bytes(), 11);
        assert_eq!(rope.len_chars(), 11);
        assert!(!rope.is_empty());
    }

    #[test]
    fn test_builder_structure() {
        // Build a more complex structure with newlines
        let tokens = vec![
            AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5)),
            AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(5, 6)),
            AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11)),
            AstBuilder::token(SyntaxKind::Newline, "\n", TextRange::new(11, 12)),
            AstBuilder::token(SyntaxKind::Text, "foo", TextRange::new(12, 15)),
            AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(15, 16)),
            AstBuilder::token(SyntaxKind::Text, "bar", TextRange::new(16, 19)),
        ];

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 19))
            .add_children(tokens)
            .finish();

        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 19))
            .add_child(paragraph)
            .finish();

        let rope = RopeAst::from_root(doc);
        let root = rope.root();
        assert_eq!(root.kind(), SyntaxKind::Document);

        let children = root.children().expect("document should have children");
        assert_eq!(children.len(), 1); // One paragraph

        let para = &children[0].0;
        assert_eq!(para.kind(), SyntaxKind::Paragraph);

        let tokens = para.children().expect("paragraph should have children");
        assert_eq!(tokens.len(), 7); // All 7 tokens
    }

    #[test]
    fn test_text_at_range() {
        // Build AST
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

        assert_eq!(rope.text_at_range(TextRange::new(0, 5)), "hello");
        assert_eq!(rope.text_at_range(TextRange::new(6, 11)), "world");
        assert_eq!(rope.text_at_range(TextRange::new(0, 11)), "hello world");
    }

    #[test]
    fn test_find_node_at_offset() {
        // Build AST
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

        let node = rope.find_node_at_offset(0);
        assert!(node.is_some());
        assert_eq!(
            node.expect("node should exist at offset 0").kind(),
            SyntaxKind::Text
        );

        let node = rope.find_node_at_offset(7);
        assert!(node.is_some());
        assert_eq!(
            node.expect("node should exist at offset 7").kind(),
            SyntaxKind::Text
        );
    }

    #[test]
    fn test_rope_insert() {
        // Build initial AST
        let token = AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5));
        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 5))
            .add_child(token)
            .finish();
        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 5))
            .add_child(paragraph)
            .finish();

        let mut rope = RopeAst::from_root(doc);
        assert_eq!(rope.to_string(), "hello");

        // Insert text
        let result = rope.insert(2, "XXX");
        assert!(result.is_ok());
        assert_eq!(rope.to_string(), "heXXXllo");
        assert_eq!(rope.len_bytes(), 8);
    }

    #[test]
    fn test_rope_delete() {
        // Build initial AST
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
        assert_eq!(rope.to_string(), "hello world");

        // Delete the space
        let result = rope.delete(TextRange::new(5, 6));
        assert!(result.is_ok());
        assert_eq!(rope.to_string(), "helloworld");
        assert_eq!(rope.len_bytes(), 10);
    }

    #[test]
    fn test_rope_replace() {
        // Build initial AST
        let token = AstBuilder::token(SyntaxKind::Text, "hello world", TextRange::new(0, 11));
        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 11))
            .add_child(token)
            .finish();
        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 11))
            .add_child(paragraph)
            .finish();

        let mut rope = RopeAst::from_root(doc);
        assert_eq!(rope.to_string(), "hello world");

        // Replace "world" with "rust"
        let result = rope.replace(TextRange::new(6, 11), "rust");
        assert!(result.is_ok());
        assert_eq!(rope.to_string(), "hello rust");
        assert_eq!(rope.len_bytes(), 10);
    }

    #[test]
    fn test_rope_query_integration() {
        // Build AST with multiple tokens
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

        // Test query() method
        let text_tokens = rope.query().kind(SyntaxKind::Text).find_all();
        assert_eq!(text_tokens.len(), 2);

        // Test finding tokens in range
        let tokens_in_range = rope
            .query()
            .tokens()
            .in_range(TextRange::new(0, 7))
            .find_all();
        // Range 0-7 includes "hello" (0-5), " " (5-6), and part of "world" (6-11)
        assert_eq!(tokens_in_range.len(), 3);

        // Test path_query() method
        let results = rope
            .path_query()
            .filter(|node| node.kind() == SyntaxKind::Text)
            .find_all();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].depth, 2); // Document -> Paragraph -> Token
    }
}
