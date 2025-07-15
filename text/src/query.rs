//! AST query operations

use crate::syntax::{Syntax, SyntaxNode};

/// Query builder for finding nodes in the AST
pub struct Query<S: Syntax> {
    /// The root node to search from
    root: SyntaxNode<S>,
}

impl<S: Syntax> Query<S> {
    /// Create a new query starting from a node
    pub fn new(root: SyntaxNode<S>) -> Self {
        Self { root }
    }

    /// Find all nodes matching a predicate
    pub fn find_all(&self, predicate: impl Fn(&SyntaxNode<S>) -> bool) -> Vec<SyntaxNode<S>> {
        let mut results = Vec::new();
        self.find_all_recursive(&self.root, &predicate, &mut results);
        results
    }

    fn find_all_recursive(
        &self,
        node: &SyntaxNode<S>,
        predicate: &impl Fn(&SyntaxNode<S>) -> bool,
        results: &mut Vec<SyntaxNode<S>>,
    ) {
        if predicate(node) {
            results.push(node.clone());
        }

        // TODO: Traverse children when node implementation is complete
    }

    /// Find the first node matching a predicate
    pub fn find_first(&self, predicate: impl Fn(&SyntaxNode<S>) -> bool) -> Option<SyntaxNode<S>> {
        self.find_first_recursive(&self.root, &predicate)
    }

    fn find_first_recursive(
        &self,
        node: &SyntaxNode<S>,
        predicate: &impl Fn(&SyntaxNode<S>) -> bool,
    ) -> Option<SyntaxNode<S>> {
        if predicate(node) {
            return Some(node.clone());
        }

        // TODO: Traverse children when node implementation is complete
        None
    }

    /// Find nodes by kind
    pub fn by_kind(&self, kind: S::Kind) -> Vec<SyntaxNode<S>> {
        self.find_all(|node| node.kind() == kind)
    }

    /// Find nodes containing the given offset
    pub fn at_offset(&self, offset: usize) -> Vec<SyntaxNode<S>> {
        self.find_all(|node| node.text_range().contains((offset as u32).into()))
    }
}
