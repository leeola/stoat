//! Builder for constructing flat ASTs efficiently

use crate::{
    TextSize,
    range::TextRange,
    syntax::{
        flat_ast::{ElementId, FlatAst, NodeData, NodeId, TokenData},
        kind::Syntax,
    },
};
use smallvec::SmallVec;

/// Builder for constructing flat ASTs with a stack-based approach
pub struct FlatTreeBuilder<S: Syntax> {
    /// The AST being built
    ast: FlatAst<S>,
    /// Stack of open nodes (node ID, start position)
    node_stack: Vec<(NodeId, TextSize)>,
    /// Current position in the text
    current_pos: TextSize,
}

impl<S: Syntax> Default for FlatTreeBuilder<S> {
    fn default() -> Self {
        Self {
            ast: FlatAst::new(),
            node_stack: Vec::new(),
            current_pos: TextSize::from(0),
        }
    }
}

impl<S: Syntax> FlatTreeBuilder<S> {
    /// Create a new tree builder
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new tree builder with capacity hints
    pub fn with_capacity(nodes: usize, tokens: usize) -> Self {
        Self {
            ast: FlatAst::with_capacity(nodes, tokens),
            node_stack: Vec::new(),
            current_pos: TextSize::from(0),
        }
    }

    /// Start a new node
    pub fn start_node(&mut self, kind: S::Kind) {
        let parent = self.node_stack.last().map(|(id, _)| *id);

        let node_data = NodeData {
            id: NodeId(0), // Will be assigned by add_node
            kind,
            range: TextRange::empty(self.current_pos), // Will be updated on finish
            parent,
            children: SmallVec::new(),
        };

        let node_id = self.ast.add_node(node_data);
        self.node_stack.push((node_id, self.current_pos));
    }

    /// Finish the current node
    pub fn finish_node(&mut self) {
        if let Some((node_id, start_pos)) = self.node_stack.pop() {
            // Update node's range
            if let Some(node) = self.ast.get_node_mut(node_id) {
                node.range = TextRange::new(start_pos, self.current_pos);
            }

            // Add to parent's children if there is a parent
            if let Some((parent_id, _)) = self.node_stack.last() {
                if let Some(parent) = self.ast.get_node_mut(*parent_id) {
                    parent.children.push(ElementId::Node(node_id));
                }
            } else {
                // This is the root node
                self.ast.set_root(node_id);
            }
        }
    }

    /// Add a token
    pub fn add_token(&mut self, kind: S::Kind, text: String) {
        let start = self.current_pos;
        let len = TextSize::from(text.len() as u32);
        let end = start + len;

        let token_data = TokenData {
            kind,
            range: TextRange::new(start, end),
            text,
        };

        let token_id = self.ast.add_token(token_data);
        self.current_pos = end;

        // Add to current node's children
        if let Some((node_id, _)) = self.node_stack.last() {
            if let Some(node) = self.ast.get_node_mut(*node_id) {
                node.children.push(ElementId::Token(token_id));
            }
        }
    }

    /// Skip whitespace or other content without creating a token
    pub fn skip(&mut self, len: usize) {
        self.current_pos += TextSize::from(len as u32);
    }

    /// Get the current position
    pub fn current_position(&self) -> TextSize {
        self.current_pos
    }

    /// Finish building and return the AST
    pub fn finish(mut self) -> FlatAst<S> {
        // Finish any remaining open nodes
        while !self.node_stack.is_empty() {
            self.finish_node();
        }

        self.ast
    }

    /// Check if we're currently inside a node
    pub fn is_in_node(&self) -> bool {
        !self.node_stack.is_empty()
    }

    /// Get the kind of the current node (if any)
    pub fn current_node_kind(&self) -> Option<S::Kind> {
        self.node_stack
            .last()
            .and_then(|(id, _)| self.ast.get_node(*id).map(|node| node.kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{
        SyntaxNodeRef,
        simple::{SimpleKind, SimpleText},
    };

    #[test]
    fn test_builder_basic() {
        let mut builder = FlatTreeBuilder::<SimpleText>::new();

        // Build: Root { Word("hello"), Whitespace(" "), Word("world") }
        builder.start_node(SimpleKind::Root);
        builder.add_token(SimpleKind::Word, "hello".to_string());
        builder.add_token(SimpleKind::Whitespace, " ".to_string());
        builder.add_token(SimpleKind::Word, "world".to_string());
        builder.finish_node();

        let ast = builder.finish();

        // Verify structure
        let root = ast.get_node(ast.root()).expect("Root node should exist");
        assert_eq!(root.kind, SimpleKind::Root);
        assert_eq!(root.range, TextRange::new(0.into(), 11.into()));
        assert_eq!(root.children.len(), 3);

        // Check tokens
        let tokens: Vec<_> = ast.tokens().collect();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].text, "hello");
        assert_eq!(tokens[1].text, " ");
        assert_eq!(tokens[2].text, "world");
    }

    #[test]
    fn test_builder_nested() {
        let mut builder = FlatTreeBuilder::<SimpleText>::new();

        // Build: Root { Line { Word("hello") }, Line { Word("world") } }
        builder.start_node(SimpleKind::Root);

        builder.start_node(SimpleKind::Line);
        builder.add_token(SimpleKind::Word, "hello".to_string());
        builder.finish_node();

        builder.add_token(SimpleKind::Whitespace, "\n".to_string());

        builder.start_node(SimpleKind::Line);
        builder.add_token(SimpleKind::Word, "world".to_string());
        builder.finish_node();

        builder.finish_node();

        let ast = builder.finish();

        // Verify nested structure
        let root_ref = SyntaxNodeRef::new(
            &ast,
            ast.get_node(ast.root()).expect("Root node should exist"),
        );
        assert_eq!(root_ref.children().count(), 2); // Two line nodes

        let first_line = root_ref.children().next().expect("Should have first line");
        assert_eq!(first_line.kind(), SimpleKind::Line);
        assert_eq!(first_line.tokens().count(), 1);
        assert_eq!(
            first_line.tokens().next().expect("Should have token").text,
            "hello"
        );
    }

    #[test]
    fn test_builder_skip() {
        let mut builder = FlatTreeBuilder::<SimpleText>::new();

        builder.start_node(SimpleKind::Root);
        builder.add_token(SimpleKind::Word, "hello".to_string());
        builder.skip(10); // Skip some content
        builder.add_token(SimpleKind::Word, "world".to_string());
        builder.finish_node();

        let ast = builder.finish();

        let root = ast.get_node(ast.root()).expect("Root node should exist");
        // Range should include skipped content
        assert_eq!(root.range, TextRange::new(0.into(), 20.into()));
    }
}
