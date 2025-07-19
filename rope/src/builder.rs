//! Builder API for constructing rope ASTs

use crate::{
    ast::{AstNode, TextRange},
    kind::SyntaxKind,
    semantic::SemanticInfo,
};
use compact_str::CompactString;
use std::sync::Arc;

/// Builder for creating AST nodes
pub struct AstBuilder;

impl AstBuilder {
    /// Create a new token node
    pub fn token(
        kind: SyntaxKind,
        text: impl Into<CompactString>,
        range: TextRange,
    ) -> Arc<AstNode> {
        Arc::new(AstNode::token(kind, text.into(), range))
    }

    /// Create a new token node with semantic info
    pub fn token_with_semantic(
        kind: SyntaxKind,
        text: impl Into<CompactString>,
        range: TextRange,
        semantic: SemanticInfo,
    ) -> Arc<AstNode> {
        Arc::new(AstNode::token_with_semantic(
            kind,
            text.into(),
            range,
            semantic,
        ))
    }

    /// Start building a syntax node
    pub fn start_node(kind: SyntaxKind, range: TextRange) -> NodeBuilder {
        NodeBuilder::new(kind, range)
    }

    /// Start building a syntax node with semantic info
    pub fn start_node_with_semantic(
        kind: SyntaxKind,
        range: TextRange,
        semantic: SemanticInfo,
    ) -> NodeBuilder {
        NodeBuilder::new_with_semantic(kind, range, semantic)
    }
}

/// Builder for syntax nodes that allows chaining child additions
pub struct NodeBuilder {
    node: AstNode,
}

impl NodeBuilder {
    /// Create a new node builder
    fn new(kind: SyntaxKind, range: TextRange) -> Self {
        Self {
            node: AstNode::syntax(kind, range),
        }
    }

    /// Create a new node builder with semantic info
    fn new_with_semantic(kind: SyntaxKind, range: TextRange, semantic: SemanticInfo) -> Self {
        Self {
            node: AstNode::syntax_with_semantic(kind, range, semantic),
        }
    }

    /// Add a child to this node
    pub fn add_child(mut self, child: Arc<AstNode>) -> Self {
        // Ignore errors for now - in a real implementation we might want to handle these
        let _ = self.node.add_child(child);
        self
    }

    /// Add multiple children to this node
    pub fn add_children(mut self, children: impl IntoIterator<Item = Arc<AstNode>>) -> Self {
        for child in children {
            let _ = self.node.add_child(child);
        }
        self
    }

    /// Finish building and return the node
    pub fn finish(self) -> Arc<AstNode> {
        Arc::new(self.node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_token_creation() {
        let token = AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5));
        assert_eq!(token.kind(), SyntaxKind::Text);
        assert_eq!(token.token_text(), Some("hello"));
    }

    #[test]
    fn test_builder_node_creation() {
        let token1 = AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5));
        let token2 = AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(5, 6));
        let token3 = AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11));

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 11))
            .add_child(token1)
            .add_child(token2)
            .add_child(token3)
            .finish();

        assert_eq!(paragraph.kind(), SyntaxKind::Paragraph);
        assert_eq!(paragraph.children().expect("should have children").len(), 3);
    }

    #[test]
    fn test_builder_nested_structure() {
        let text_token = AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5));

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 5))
            .add_child(text_token)
            .finish();

        let document = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 5))
            .add_child(paragraph)
            .finish();

        assert_eq!(document.kind(), SyntaxKind::Document);
        let children = document.children().expect("should have children");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].0.kind(), SyntaxKind::Paragraph);
    }
}
