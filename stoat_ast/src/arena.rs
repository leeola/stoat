//! Arena allocator for AST nodes

use crate::Node;

/// Arena allocator for AST nodes
///
/// All nodes are allocated in this arena and have the same lifetime.
/// This enables efficient structural sharing and fast allocation.
pub struct Arena<'arena> {
    arena: typed_arena::Arena<Node<'arena>>,
}

impl<'arena> Arena<'arena> {
    /// Create a new arena
    pub fn new() -> Self {
        Self {
            arena: typed_arena::Arena::new(),
        }
    }

    /// Allocate a new node in the arena
    pub fn alloc(&self, node: Node<'arena>) -> &Node<'arena> {
        self.arena.alloc(node)
    }
}

impl<'arena> Default for Arena<'arena> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::SyntaxKind;

    #[test]
    fn test_basic_node_creation() {
        let arena = Arena::new();
        let node = arena.alloc(Node::leaf(SyntaxKind::Text, "hello"));

        assert_eq!(node.kind(), SyntaxKind::Text);
        assert_eq!(node.text(), "hello");
        assert!(node.is_leaf());
    }

    #[test]
    fn test_multiple_nodes() {
        let arena = Arena::new();

        let node1 = arena.alloc(Node::leaf(SyntaxKind::Text, "hello"));
        let node2 = arena.alloc(Node::leaf(SyntaxKind::Identifier, "world"));

        // Nodes should have different addresses but same arena lifetime
        assert_ne!(node1 as *const Node<'_>, node2 as *const Node<'_>);
        assert_eq!(node1.text(), "hello");
        assert_eq!(node2.text(), "world");
    }

    #[test]
    fn test_tree_building() {
        let arena = Arena::new();

        // Create leaf nodes
        let text1 = arena.alloc(Node::leaf(SyntaxKind::Text, "Hello"));
        let text2 = arena.alloc(Node::leaf(SyntaxKind::Text, "world"));

        // Create a paragraph containing the text nodes
        let paragraph = arena.alloc(Node::internal(SyntaxKind::Paragraph, vec![text1, text2]));

        // Create a document containing the paragraph
        let document = arena.alloc(Node::internal(SyntaxKind::Document, vec![paragraph]));

        // Verify tree structure
        assert_eq!(document.kind(), SyntaxKind::Document);
        assert!(!document.is_leaf());
        assert_eq!(document.children().len(), 1);

        let para = document.children()[0];
        assert_eq!(para.kind(), SyntaxKind::Paragraph);
        assert_eq!(para.children().len(), 2);

        assert_eq!(para.children()[0].text(), "Hello");
        assert_eq!(para.children()[1].text(), "world");
    }
}
