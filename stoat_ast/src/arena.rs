//! Arena allocator for AST nodes

use crate::Node;

/// Arena allocator for AST nodes
///
/// All nodes are allocated in this arena and have the same lifetime.
/// This enables efficient structural sharing and fast allocation.
pub struct Arena {
    arena: typed_arena::Arena<Node>,
}

impl Arena {
    /// Create a new arena
    pub fn new() -> Self {
        Self {
            arena: typed_arena::Arena::new(),
        }
    }

    /// Allocate a new node in the arena
    pub fn alloc(&self, node: Node) -> &Node {
        self.arena.alloc(node)
    }
}

impl Default for Arena {
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
        let node = arena.alloc(Node::new(SyntaxKind::Text, "hello"));

        assert_eq!(node.kind(), SyntaxKind::Text);
        assert_eq!(node.text(), "hello");
    }

    #[test]
    fn test_multiple_nodes() {
        let arena = Arena::new();

        let node1 = arena.alloc(Node::new(SyntaxKind::Text, "hello"));
        let node2 = arena.alloc(Node::new(SyntaxKind::Identifier, "world"));

        // Nodes should have different addresses but same arena lifetime
        assert_ne!(node1 as *const Node, node2 as *const Node);
        assert_eq!(node1.text(), "hello");
        assert_eq!(node2.text(), "world");
    }
}
