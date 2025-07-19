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

        // Recursively search through all child nodes
        for child in node.children() {
            if let crate::syntax::SyntaxElement::Node(child_node) = child {
                self.find_all_recursive(child_node, predicate, results);
            }
        }
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

        // Recursively search through all child nodes
        for child in node.children() {
            if let crate::syntax::SyntaxElement::Node(child_node) = child {
                if let Some(found) = self.find_first_recursive(child_node, predicate) {
                    return Some(found);
                }
            }
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TextBuffer, syntax::unified_kind::SyntaxKind};

    #[test]
    fn test_query_find_all_tree_traversal() {
        // Use simple parser to get actual tree structure
        let parse_result = crate::syntax::parse::parse_simple("hello world\nfoo bar");
        let root = parse_result.root;
        let query = Query::new(root);

        // Find all Line nodes (Words are tokens, not nodes)
        let lines = query.find_all(|node| node.kind() == SyntaxKind::Line);
        assert_eq!(lines.len(), 2, "Should find 2 lines");

        // Verify line content
        let line_texts: Vec<String> = lines.iter().map(|l| l.text().to_string()).collect();
        assert_eq!(line_texts, vec!["hello world", "foo bar"]);

        // Verify tree traversal works by finding all nodes
        let all_nodes = query.find_all(|_| true);
        assert_eq!(all_nodes.len(), 3, "Should find Root and 2 Line nodes");
    }

    #[test]
    fn test_query_find_first() {
        // Use simple parser to get actual tree structure
        let parse_result = crate::syntax::parse::parse_simple("hello world\nfoo bar");
        let root = parse_result.root;
        let query = Query::new(root);

        // Find first Line node (Words are tokens, not nodes)
        let first_line = query.find_first(|node| node.kind() == SyntaxKind::Line);
        assert!(first_line.is_some());
        assert_eq!(first_line.unwrap().text(), "hello world");

        // Find Root node
        let root_node = query.find_first(|node| node.kind() == SyntaxKind::Root);
        assert!(root_node.is_some());

        // Try to find non-existent node type
        let no_match = query.find_first(|node| node.kind() == SyntaxKind::Error);
        assert!(no_match.is_none());
    }

    #[test]
    fn test_query_by_kind() {
        // Use simple parser to get actual tree structure
        let parse_result = crate::syntax::parse::parse_simple("hello world\nfoo bar\nbaz");
        let root = parse_result.root;
        let query = Query::new(root);

        // Find by specific kinds - Words are tokens, not nodes
        let lines = query.by_kind(SyntaxKind::Line);
        assert_eq!(lines.len(), 3);

        // Root node
        let roots = query.by_kind(SyntaxKind::Root);
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn test_query_at_offset() {
        // Use simple parser to get actual tree structure
        let parse_result = crate::syntax::parse::parse_simple("hello world");
        let root = parse_result.root;
        let query = Query::new(root);

        // Find nodes at offset 0 (should include root and first line)
        let at_start = query.at_offset(0);
        assert!(at_start.len() >= 2);
        assert!(at_start.iter().any(|n| n.kind() == SyntaxKind::Root));
        assert!(at_start.iter().any(|n| n.kind() == SyntaxKind::Line));

        // Find nodes at offset 6 (in "world" - should find root and line)
        let at_world = query.at_offset(6);
        assert!(at_world.len() >= 2);
        assert!(at_world.iter().any(|n| n.kind() == SyntaxKind::Root));
        assert!(
            at_world
                .iter()
                .any(|n| n.kind() == SyntaxKind::Line && n.text() == "hello world")
        );
    }
}
