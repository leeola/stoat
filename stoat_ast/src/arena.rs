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
    use crate::{assert_kind, assert_text, kind::SyntaxKind, tree};

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

        // Build tree bottom-up
        let hello = tree!(arena, Text("Hello"));
        let world = tree!(arena, Text("world"));
        let para = tree!(arena, Paragraph[hello, world]);
        let doc = tree!(arena, Document[para]);

        assert_kind!(doc, Document);
        assert_eq!(doc.children().len(), 1);

        let para_ref = doc.children()[0];
        assert_kind!(para_ref, Paragraph);
        assert_eq!(para_ref.children().len(), 2);

        assert_text!(para_ref.children()[0], "Hello");
        assert_text!(para_ref.children()[1], "world");
    }
}
