//! Query API for searching and navigating the rope AST

use crate::{
    ast::{AstNode, TextPos, TextRange},
    kind::SyntaxKind,
    semantic::{SemanticId, SemanticKind},
};
use std::sync::Arc;

/// Filter function type for queries
type NodeFilter<'a> = Box<dyn Fn(&Arc<AstNode>) -> bool + 'a>;

/// A query builder for searching nodes in the AST
pub struct Query<'a> {
    root: &'a Arc<AstNode>,
    filters: Vec<NodeFilter<'a>>,
}

impl<'a> Query<'a> {
    /// Create a new query starting from the given root
    pub fn new(root: &'a Arc<AstNode>) -> Self {
        Self {
            root,
            filters: Vec::new(),
        }
    }

    /// Filter nodes by syntax kind
    pub fn kind(mut self, kind: SyntaxKind) -> Self {
        self.filters.push(Box::new(move |node| node.kind() == kind));
        self
    }

    /// Filter nodes by multiple syntax kinds
    pub fn kinds(mut self, kinds: &'a [SyntaxKind]) -> Self {
        self.filters
            .push(Box::new(move |node| kinds.contains(&node.kind())));
        self
    }

    /// Filter nodes that overlap with a given range
    pub fn in_range(mut self, range: TextRange) -> Self {
        self.filters.push(Box::new(move |node| {
            let node_range = node.range();
            // Check if ranges overlap
            node_range.start.0 < range.end.0 && node_range.end.0 > range.start.0
        }));
        self
    }

    /// Filter nodes that contain a given position
    pub fn at_offset(mut self, offset: usize) -> Self {
        self.filters.push(Box::new(move |node| {
            let range = node.range();
            offset >= range.start.0 && offset < range.end.0
        }));
        self
    }

    /// Filter token nodes only
    pub fn tokens(mut self) -> Self {
        self.filters.push(Box::new(|node| node.is_token()));
        self
    }

    /// Filter syntax nodes only
    pub fn syntax_nodes(mut self) -> Self {
        self.filters.push(Box::new(|node| node.is_syntax()));
        self
    }

    /// Filter nodes containing specific text
    pub fn containing_text(mut self, text: &'a str) -> Self {
        self.filters.push(Box::new(move |node| {
            if let Some(token_text) = node.token_text() {
                token_text.contains(text)
            } else {
                false
            }
        }));
        self
    }

    /// Filter nodes with semantic information
    pub fn has_semantic(mut self) -> Self {
        self.filters
            .push(Box::new(|node| node.semantic().is_some()));
        self
    }

    /// Filter nodes by semantic ID
    pub fn semantic_id(mut self, id: SemanticId) -> Self {
        self.filters.push(Box::new(move |node| {
            node.semantic().map(|sem| sem.id == id).unwrap_or(false)
        }));
        self
    }

    /// Filter nodes by semantic kind
    pub fn semantic_kind(mut self, kind: SemanticKind) -> Self {
        self.filters.push(Box::new(move |node| {
            node.semantic().map(|sem| sem.kind == kind).unwrap_or(false)
        }));
        self
    }

    /// Execute the query and return all matching nodes
    pub fn find_all(self) -> Vec<&'a Arc<AstNode>> {
        let mut results = Vec::new();
        self.find_all_impl(self.root, &mut results);
        results
    }

    /// Execute the query and return the first matching node
    pub fn find_first(self) -> Option<&'a Arc<AstNode>> {
        self.find_first_impl(self.root)
    }

    /// Count matching nodes without collecting them
    pub fn count(self) -> usize {
        self.count_impl(self.root)
    }

    /// Helper to recursively find all matching nodes
    fn find_all_impl(&self, node: &'a Arc<AstNode>, results: &mut Vec<&'a Arc<AstNode>>) {
        // Check if node matches all filters
        if self.matches(node) {
            results.push(node);
        }

        // Recurse into children
        if let Some(children) = node.children() {
            for (child, _) in children {
                self.find_all_impl(child, results);
            }
        }
    }

    /// Helper to find the first matching node
    fn find_first_impl(&self, node: &'a Arc<AstNode>) -> Option<&'a Arc<AstNode>> {
        // Check if node matches all filters
        if self.matches(node) {
            return Some(node);
        }

        // Recurse into children
        if let Some(children) = node.children() {
            for (child, _) in children {
                if let Some(found) = self.find_first_impl(child) {
                    return Some(found);
                }
            }
        }

        None
    }

    /// Helper to count matching nodes
    fn count_impl(&self, node: &Arc<AstNode>) -> usize {
        let mut count = 0;

        // Check if node matches all filters
        if self.matches(node) {
            count += 1;
        }

        // Recurse into children
        if let Some(children) = node.children() {
            for (child, _) in children {
                count += self.count_impl(child);
            }
        }

        count
    }

    /// Check if a node matches all filters
    fn matches(&self, node: &Arc<AstNode>) -> bool {
        self.filters.iter().all(|filter| filter(node))
    }
}

/// Query result that provides additional context
pub struct QueryResult<'a> {
    /// The matching node
    pub node: &'a Arc<AstNode>,
    /// Path from root to this node
    pub path: Vec<&'a Arc<AstNode>>,
    /// Depth of the node in the tree
    pub depth: usize,
}

/// Advanced query builder with path tracking
pub struct PathQuery<'a> {
    root: &'a Arc<AstNode>,
    filters: Vec<NodeFilter<'a>>,
}

impl<'a> PathQuery<'a> {
    /// Create a new path query
    pub fn new(root: &'a Arc<AstNode>) -> Self {
        Self {
            root,
            filters: Vec::new(),
        }
    }

    /// Add a filter (same filters as Query)
    pub fn filter<F>(mut self, f: F) -> Self
    where
        F: Fn(&Arc<AstNode>) -> bool + 'a,
    {
        self.filters.push(Box::new(f));
        self
    }

    /// Find all nodes with their paths
    pub fn find_all(self) -> Vec<QueryResult<'a>> {
        let mut results = Vec::new();
        let mut path = Vec::new();
        self.find_all_impl(self.root, &mut path, 0, &mut results);
        results
    }

    fn find_all_impl(
        &self,
        node: &'a Arc<AstNode>,
        path: &mut Vec<&'a Arc<AstNode>>,
        depth: usize,
        results: &mut Vec<QueryResult<'a>>,
    ) {
        path.push(node);

        // Check if node matches all filters
        if self.matches(node) {
            results.push(QueryResult {
                node,
                path: path.clone(),
                depth,
            });
        }

        // Recurse into children
        if let Some(children) = node.children() {
            for (child, _) in children {
                self.find_all_impl(child, path, depth + 1, results);
            }
        }

        path.pop();
    }

    fn matches(&self, node: &Arc<AstNode>) -> bool {
        self.filters.iter().all(|filter| filter(node))
    }
}

/// Utility functions for common queries
pub struct QueryUtils;

impl QueryUtils {
    /// Find the deepest token at the given offset
    pub fn token_at_offset(root: &Arc<AstNode>, offset: usize) -> Option<&Arc<AstNode>> {
        Self::token_at_offset_impl(root, TextPos(offset))
    }

    fn token_at_offset_impl(node: &Arc<AstNode>, pos: TextPos) -> Option<&Arc<AstNode>> {
        let range = node.range();

        // Check if position is within this node
        if pos.0 < range.start.0 || pos.0 >= range.end.0 {
            return None;
        }

        // If this is a token, return it
        if node.is_token() {
            return Some(node);
        }

        // Otherwise, search children
        if let Some(children) = node.children() {
            for (child, _) in children {
                if let Some(found) = Self::token_at_offset_impl(child, pos) {
                    return Some(found);
                }
            }
        }

        None
    }

    /// Get line number for a given offset (0-based)
    pub fn line_at_offset(root: &Arc<AstNode>, offset: usize) -> usize {
        let mut line = 0;
        let mut byte_pos = 0;

        Self::count_lines_to_offset(root, offset, &mut line, &mut byte_pos);
        line
    }

    fn count_lines_to_offset(
        node: &Arc<AstNode>,
        target_offset: usize,
        line_count: &mut usize,
        byte_pos: &mut usize,
    ) {
        match node.as_ref() {
            AstNode::Token { text, range, .. } => {
                if range.start.0 > target_offset {
                    return;
                }

                let local_offset = target_offset.saturating_sub(range.start.0);
                let check_until = local_offset.min(text.len());

                for ch in text[..check_until].chars() {
                    if ch == '\n' {
                        *line_count += 1;
                    }
                }

                *byte_pos = range.start.0 + check_until;
            },
            AstNode::Syntax { children, .. } => {
                for (child, _) in children {
                    if *byte_pos >= target_offset {
                        break;
                    }
                    Self::count_lines_to_offset(child, target_offset, line_count, byte_pos);
                }
            },
        }
    }

    /// Find all nodes of a specific kind
    pub fn find_by_kind(root: &Arc<AstNode>, kind: SyntaxKind) -> Vec<&Arc<AstNode>> {
        Query::new(root).kind(kind).find_all()
    }

    /// Count nodes of a specific kind
    pub fn count_by_kind(root: &Arc<AstNode>, kind: SyntaxKind) -> usize {
        Query::new(root).kind(kind).count()
    }

    /// Get all tokens in a range
    pub fn tokens_in_range(root: &Arc<AstNode>, range: TextRange) -> Vec<&Arc<AstNode>> {
        Query::new(root).tokens().in_range(range).find_all()
    }

    /// Find all nodes with a specific semantic ID
    pub fn find_by_semantic_id(root: &Arc<AstNode>, id: SemanticId) -> Vec<&Arc<AstNode>> {
        Query::new(root).semantic_id(id).find_all()
    }

    /// Find all nodes with a specific semantic kind
    pub fn find_by_semantic_kind(root: &Arc<AstNode>, kind: SemanticKind) -> Vec<&Arc<AstNode>> {
        Query::new(root).semantic_kind(kind).find_all()
    }

    /// Find all definitions in the tree
    pub fn find_definitions(root: &Arc<AstNode>) -> Vec<&Arc<AstNode>> {
        Query::new(root)
            .semantic_kind(SemanticKind::Definition)
            .find_all()
    }

    /// Find all references in the tree
    pub fn find_references(root: &Arc<AstNode>) -> Vec<&Arc<AstNode>> {
        Query::new(root)
            .semantic_kind(SemanticKind::Reference)
            .find_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{builder::AstBuilder, kind::SyntaxKind};

    fn create_test_ast() -> Arc<AstNode> {
        let tokens = [
            AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5)),
            AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(5, 6)),
            AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11)),
            AstBuilder::token(SyntaxKind::Newline, "\n", TextRange::new(11, 12)),
            AstBuilder::token(SyntaxKind::Text, "foo", TextRange::new(12, 15)),
        ];

        let para1 = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 12))
            .add_child(tokens[0].clone())
            .add_child(tokens[1].clone())
            .add_child(tokens[2].clone())
            .add_child(tokens[3].clone())
            .finish();

        let para2 = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(12, 15))
            .add_child(tokens[4].clone())
            .finish();

        AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 15))
            .add_child(para1)
            .add_child(para2)
            .finish()
    }

    #[test]
    fn test_query_by_kind() {
        let ast = create_test_ast();

        // Find all text tokens
        let text_nodes = Query::new(&ast).kind(SyntaxKind::Text).find_all();
        assert_eq!(text_nodes.len(), 3);

        // Find all paragraphs
        let para_nodes = Query::new(&ast).kind(SyntaxKind::Paragraph).find_all();
        assert_eq!(para_nodes.len(), 2);

        // Find document node
        let doc_nodes = Query::new(&ast).kind(SyntaxKind::Document).find_all();
        assert_eq!(doc_nodes.len(), 1);
    }

    #[test]
    fn test_query_by_range() {
        let ast = create_test_ast();

        // Find nodes in first paragraph
        let nodes = Query::new(&ast).in_range(TextRange::new(0, 11)).find_all();
        // Should find: Document, first Paragraph, and its 3 tokens
        assert!(nodes.len() >= 5);

        // Find nodes at specific offset
        let nodes = Query::new(&ast).at_offset(7).find_all();
        // Should find: Document, first Paragraph, and "world" token
        assert_eq!(nodes.len(), 3);
    }

    #[test]
    fn test_query_tokens() {
        let ast = create_test_ast();

        // Find all tokens
        let tokens = Query::new(&ast).tokens().find_all();
        assert_eq!(tokens.len(), 5);

        // Find text tokens only
        let text_tokens = Query::new(&ast).tokens().kind(SyntaxKind::Text).find_all();
        assert_eq!(text_tokens.len(), 3);
    }

    #[test]
    fn test_query_containing_text() {
        let ast = create_test_ast();

        // Find tokens containing "o"
        let nodes = Query::new(&ast).containing_text("o").find_all();
        assert_eq!(nodes.len(), 3); // "hello", "world", "foo"

        // Find tokens containing "oo"
        let nodes = Query::new(&ast).containing_text("oo").find_all();
        assert_eq!(nodes.len(), 1); // "foo"
    }

    #[test]
    fn test_query_utils() {
        let ast = create_test_ast();

        // Test token at offset
        let token = QueryUtils::token_at_offset(&ast, 7);
        assert!(token.is_some());
        assert_eq!(
            token.expect("should find token").token_text(),
            Some("world")
        );

        // Test line at offset
        assert_eq!(QueryUtils::line_at_offset(&ast, 0), 0); // First line
        assert_eq!(QueryUtils::line_at_offset(&ast, 11), 0); // Still first line
        assert_eq!(QueryUtils::line_at_offset(&ast, 12), 1); // Second line

        // Test find by kind
        let text_nodes = QueryUtils::find_by_kind(&ast, SyntaxKind::Text);
        assert_eq!(text_nodes.len(), 3);

        // Test count by kind
        assert_eq!(QueryUtils::count_by_kind(&ast, SyntaxKind::Text), 3);
        assert_eq!(QueryUtils::count_by_kind(&ast, SyntaxKind::Paragraph), 2);
    }

    #[test]
    fn test_path_query() {
        let ast = create_test_ast();

        // Find all text tokens with paths
        let results = PathQuery::new(&ast)
            .filter(|node| node.kind() == SyntaxKind::Text)
            .find_all();

        assert_eq!(results.len(), 3);

        // Check first result
        let first = &results[0];
        assert_eq!(first.node.token_text(), Some("hello"));
        assert_eq!(first.depth, 2); // Document -> Paragraph -> Token
        assert_eq!(first.path.len(), 3);
        assert_eq!(first.path[0].kind(), SyntaxKind::Document);
        assert_eq!(first.path[1].kind(), SyntaxKind::Paragraph);
        assert_eq!(first.path[2].kind(), SyntaxKind::Text);
    }

    #[test]
    fn test_complex_query() {
        let ast = create_test_ast();

        // Find text tokens in the first 10 bytes
        let nodes = Query::new(&ast)
            .tokens()
            .kind(SyntaxKind::Text)
            .in_range(TextRange::new(0, 10))
            .find_all();

        assert_eq!(nodes.len(), 2); // "hello" and "world"
    }

    #[test]
    fn test_semantic_queries() {
        use crate::semantic::{SemanticId, SemanticInfo};

        // Create AST with semantic info
        let sem_id1 = SemanticId::new(100);
        let sem_id2 = SemanticId::new(200);

        let token1 = AstBuilder::token_with_semantic(
            SyntaxKind::Identifier,
            "foo",
            TextRange::new(0, 3),
            SemanticInfo::definition(sem_id1),
        );

        let token2 = AstBuilder::token_with_semantic(
            SyntaxKind::Identifier,
            "foo",
            TextRange::new(10, 13),
            SemanticInfo::reference(sem_id1),
        );

        let token3 = AstBuilder::token_with_semantic(
            SyntaxKind::Identifier,
            "bar",
            TextRange::new(20, 23),
            SemanticInfo::definition(sem_id2),
        );

        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 23))
            .add_child(token1)
            .add_child(token2)
            .add_child(token3)
            .finish();

        // Test finding nodes with semantic info
        let semantic_nodes = Query::new(&doc).has_semantic().find_all();
        assert_eq!(semantic_nodes.len(), 3);

        // Test finding by semantic ID
        let id1_nodes = QueryUtils::find_by_semantic_id(&doc, sem_id1);
        assert_eq!(id1_nodes.len(), 2);

        // Test finding definitions
        let defs = QueryUtils::find_definitions(&doc);
        assert_eq!(defs.len(), 2);

        // Test finding references
        let refs = QueryUtils::find_references(&doc);
        assert_eq!(refs.len(), 1);

        // Test combined query
        let foo_refs = Query::new(&doc)
            .semantic_kind(SemanticKind::Reference)
            .containing_text("foo")
            .find_all();
        assert_eq!(foo_refs.len(), 1);
    }
}
