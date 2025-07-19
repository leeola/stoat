//! Compatibility layer for migrating from Arc-based nodes to flat AST

use crate::{
    TextSize,
    range::TextRange,
    syntax::{
        flat_ast::{ElementId, FlatAst, NodeId, SyntaxNodeRef},
        kind::SyntaxKind,
        node::{SyntaxElement, SyntaxNode, SyntaxToken},
    },
};
use std::sync::Arc;

/// Wrapper that provides SyntaxNode interface backed by flat AST
pub struct FlatSyntaxNode {
    /// Reference to the flat AST
    ast: Arc<FlatAst>,
    /// ID of this node in the AST
    id: NodeId,
}

impl FlatSyntaxNode {
    /// Create a new flat syntax node
    pub fn new(ast: Arc<FlatAst>, id: NodeId) -> Self {
        Self { ast, id }
    }

    /// Convert to the old SyntaxNode type (for gradual migration)
    pub fn to_legacy(&self) -> Option<SyntaxNode> {
        self.ast.get_node(self.id).map(|node_data| {
            let children = self.children_as_elements();
            SyntaxNode::new_with_children(node_data.kind, node_data.range, children)
        })
    }

    /// Get children as SyntaxElement vector (for legacy compatibility)
    fn children_as_elements(&self) -> Vec<SyntaxElement> {
        let mut elements = Vec::new();

        if let Some(node_data) = self.ast.get_node(self.id) {
            for &child_id in &node_data.children {
                match child_id {
                    ElementId::Node(node_id) => {
                        let child_flat = FlatSyntaxNode::new(self.ast.clone(), node_id);
                        if let Some(child_legacy) = child_flat.to_legacy() {
                            elements.push(SyntaxElement::Node(child_legacy));
                        }
                    },
                    ElementId::Token(token_id) => {
                        if let Some(token_data) = self.ast.get_token(token_id) {
                            let token = SyntaxToken::new(
                                token_data.kind,
                                token_data.range,
                                Arc::from(token_data.text.as_str()),
                            );
                            elements.push(SyntaxElement::Token(token));
                        }
                    },
                }
            }
        }

        elements
    }

    /// Get the kind of this node
    pub fn kind(&self) -> Option<SyntaxKind> {
        self.ast.get_node(self.id).map(|n| n.kind)
    }

    /// Get the text range of this node
    pub fn text_range(&self) -> TextRange {
        self.ast
            .get_node(self.id)
            .map(|n| n.range)
            .unwrap_or_else(|| TextRange::empty(TextSize::from(0)))
    }

    /// Get the parent node
    pub fn parent(&self) -> Option<FlatSyntaxNode> {
        self.ast
            .get_node(self.id)
            .and_then(|n| n.parent)
            .map(|parent_id| FlatSyntaxNode::new(self.ast.clone(), parent_id))
    }

    /// Get child nodes
    pub fn children(&self) -> Vec<FlatSyntaxNode> {
        let mut children = Vec::new();

        if let Some(node_data) = self.ast.get_node(self.id) {
            for &child_id in &node_data.children {
                if let ElementId::Node(node_id) = child_id {
                    children.push(FlatSyntaxNode::new(self.ast.clone(), node_id));
                }
            }
        }

        children
    }

    /// Get tokens in this subtree
    pub fn tokens(&self) -> Vec<SyntaxToken> {
        let mut tokens = Vec::new();
        self.collect_tokens(&mut tokens);
        tokens
    }

    fn collect_tokens(&self, tokens: &mut Vec<SyntaxToken>) {
        if let Some(node_data) = self.ast.get_node(self.id) {
            for &child_id in &node_data.children {
                match child_id {
                    ElementId::Token(token_id) => {
                        if let Some(token_data) = self.ast.get_token(token_id) {
                            let token = SyntaxToken::new(
                                token_data.kind,
                                token_data.range,
                                Arc::from(token_data.text.as_str()),
                            );
                            tokens.push(token);
                        }
                    },
                    ElementId::Node(node_id) => {
                        let child = FlatSyntaxNode::new(self.ast.clone(), node_id);
                        child.collect_tokens(tokens);
                    },
                }
            }
        }
    }

    /// Find the token at the given offset
    pub fn find_token_at_offset(&self, offset: TextSize) -> Option<SyntaxToken> {
        // Use the flat AST's efficient lookup
        if let Some(containing_node_id) = self.ast.find_node_at_offset(offset) {
            // Get a reference to traverse from the found node
            if let Some(node_data) = self.ast.get_node(containing_node_id) {
                let node_ref = SyntaxNodeRef::new(&self.ast, node_data);

                // Look for token at exact offset
                for token_data in node_ref.tokens() {
                    if token_data.range.contains(offset) {
                        return Some(SyntaxToken::new(
                            token_data.kind,
                            token_data.range,
                            Arc::from(token_data.text.as_str()),
                        ));
                    }
                }
            }
        }

        None
    }
}

impl Clone for FlatSyntaxNode {
    fn clone(&self) -> Self {
        Self {
            ast: self.ast.clone(),
            id: self.id,
        }
    }
}

/// Bridge trait for converting between representations
pub trait AstBridge {
    /// Convert a legacy SyntaxNode to use flat AST
    fn from_legacy(node: &SyntaxNode) -> Arc<FlatAst>;

    /// Convert a flat AST to legacy format
    fn to_legacy(ast: &FlatAst) -> SyntaxNode;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{flat_builder::FlatTreeBuilder, kind::SyntaxKind};

    #[test]
    fn test_compat_basic() {
        let mut builder = FlatTreeBuilder::new();

        // Build a simple AST
        builder.start_node(SyntaxKind::Root);
        builder.add_token(SyntaxKind::Word, "hello".to_string());
        builder.add_token(SyntaxKind::Whitespace, " ".to_string());
        builder.add_token(SyntaxKind::Word, "world".to_string());
        builder.finish_node();

        let ast = Arc::new(builder.finish());
        let root = FlatSyntaxNode::new(ast.clone(), ast.root());

        // Test basic operations
        assert_eq!(root.kind(), Some(SyntaxKind::Root));
        assert_eq!(root.text_range(), TextRange::new(0.into(), 11.into()));

        // Test token collection
        let tokens = root.tokens();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].text(), "hello");
        assert_eq!(tokens[1].text(), " ");
        assert_eq!(tokens[2].text(), "world");
    }

    #[test]
    fn test_compat_to_legacy() {
        let mut builder = FlatTreeBuilder::new();

        builder.start_node(SyntaxKind::Root);
        builder.add_token(SyntaxKind::Word, "test".to_string());
        builder.finish_node();

        let ast = Arc::new(builder.finish());
        let root = FlatSyntaxNode::new(ast.clone(), ast.root());

        // Convert to legacy format
        let legacy = root.to_legacy().expect("Should convert to legacy");
        assert_eq!(legacy.kind(), SyntaxKind::Root);
        assert_eq!(legacy.text_range(), TextRange::new(0.into(), 4.into()));

        // Check children were converted
        let children = legacy.children();
        assert_eq!(children.len(), 1);
        match &children[0] {
            SyntaxElement::Token(t) => {
                assert_eq!(t.kind(), SyntaxKind::Word);
                assert_eq!(t.text(), "test");
            },
            _ => panic!("Expected token"),
        }
    }
}
