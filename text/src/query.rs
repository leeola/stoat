//! AST query operations

use crate::syntax::{SyntaxNode, unified_kind::SyntaxKind};

/// Query builder for finding nodes in the AST
pub struct Query {
    /// The root node to search from
    root: SyntaxNode,
}

impl Query {
    /// Create a new query starting from a node
    pub fn new(root: SyntaxNode) -> Self {
        Self { root }
    }

    /// Find all nodes matching a predicate
    pub fn find_all(&self, predicate: impl Fn(&SyntaxNode) -> bool) -> Vec<SyntaxNode> {
        let mut results = Vec::new();
        self.find_all_recursive(&self.root, &predicate, &mut results);
        results
    }

    fn find_all_recursive(
        &self,
        node: &SyntaxNode,
        predicate: &impl Fn(&SyntaxNode) -> bool,
        results: &mut Vec<SyntaxNode>,
    ) {
        if predicate(node) {
            results.push(node.clone());
        }

        // TODO: Traverse children when node implementation is complete
    }

    /// Find the first node matching a predicate
    pub fn find_first(&self, predicate: impl Fn(&SyntaxNode) -> bool) -> Option<SyntaxNode> {
        self.find_first_recursive(&self.root, &predicate)
    }

    fn find_first_recursive(
        &self,
        node: &SyntaxNode,
        predicate: &impl Fn(&SyntaxNode) -> bool,
    ) -> Option<SyntaxNode> {
        if predicate(node) {
            return Some(node.clone());
        }

        // TODO: Traverse children when node implementation is complete
        None
    }

    /// Find nodes by kind
    pub fn by_kind(&self, kind: SyntaxKind) -> Vec<SyntaxNode> {
        self.find_all(|node| node.kind() == kind)
    }

    /// Find nodes containing the given offset
    pub fn at_offset(&self, offset: usize) -> Vec<SyntaxNode> {
        self.find_all(|node| node.text_range().contains((offset as u32).into()))
    }
}
