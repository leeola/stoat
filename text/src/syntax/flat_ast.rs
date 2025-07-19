//! Flat, ID-based AST implementation for efficient memory usage and traversal

use crate::{TextSize, range::TextRange, syntax::kind::SyntaxKind};
use smallvec::SmallVec;
use std::sync::atomic::{AtomicU64, Ordering};

/// Unique identifier for a syntax node
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub(crate) u64);

impl NodeId {
    /// Create a new unique node ID
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// Unique identifier for a syntax token
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenId(u32);

/// Reference to either a node or token
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementId {
    Node(NodeId),
    Token(TokenId),
}

/// Data for a syntax node stored in flat array
#[derive(Debug, Clone)]
pub struct NodeData {
    /// Unique ID for this node
    pub id: NodeId,
    /// The kind of this node
    pub kind: SyntaxKind,
    /// Byte range in the source text
    pub range: TextRange,
    /// Parent node ID (None for root)
    pub parent: Option<NodeId>,
    /// Child elements (nodes or tokens)
    /// Using SmallVec to optimize for nodes with few children
    pub children: SmallVec<[ElementId; 4]>,
}

/// Data for a syntax token (leaf node with text)
#[derive(Debug, Clone)]
pub struct TokenData {
    /// The kind of this token
    pub kind: SyntaxKind,
    /// Byte range in the source text
    pub range: TextRange,
    /// The actual text content
    pub text: String,
}

/// Flat AST structure with all nodes stored in vectors
#[derive(Clone)]
pub struct FlatAst {
    /// All nodes stored flat
    nodes: Vec<NodeData>,
    /// All tokens stored flat
    tokens: Vec<TokenData>,
    /// Root node ID
    root: NodeId,
    /// Maps NodeId to index in nodes vector for fast lookup
    node_index: rustc_hash::FxHashMap<NodeId, usize>,
}

impl Default for FlatAst {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            tokens: Vec::new(),
            root: NodeId(0),
            node_index: rustc_hash::FxHashMap::default(),
        }
    }
}

impl FlatAst {
    /// Create a new empty AST
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new AST with pre-allocated capacity
    pub fn with_capacity(nodes: usize, tokens: usize) -> Self {
        Self {
            nodes: Vec::with_capacity(nodes),
            tokens: Vec::with_capacity(tokens),
            root: NodeId(0),
            node_index: rustc_hash::FxHashMap::default(),
        }
    }

    /// Get the root node ID
    pub fn root(&self) -> NodeId {
        self.root
    }

    /// Set the root node ID
    pub(crate) fn set_root(&mut self, root: NodeId) {
        self.root = root;
    }

    /// Add a node to the AST
    pub(crate) fn add_node(&mut self, mut node: NodeData) -> NodeId {
        let id = NodeId::new();
        node.id = id;

        let index = self.nodes.len();
        self.nodes.push(node);
        self.node_index.insert(id, index);

        id
    }

    /// Add a token to the AST
    pub(crate) fn add_token(&mut self, token: TokenData) -> TokenId {
        let id = TokenId(self.tokens.len() as u32);
        self.tokens.push(token);
        id
    }

    /// Get a node by ID
    pub fn get_node(&self, id: NodeId) -> Option<&NodeData> {
        self.node_index
            .get(&id)
            .and_then(|&idx| self.nodes.get(idx))
    }

    /// Get a mutable node by ID (for building)
    pub(crate) fn get_node_mut(&mut self, id: NodeId) -> Option<&mut NodeData> {
        self.node_index
            .get(&id)
            .and_then(|&idx| self.nodes.get_mut(idx))
    }

    /// Get a token by ID
    pub fn get_token(&self, id: TokenId) -> Option<&TokenData> {
        self.tokens.get(id.0 as usize)
    }

    /// Get node data by element ID
    pub fn get_element_node(&self, id: ElementId) -> Option<&NodeData> {
        match id {
            ElementId::Node(node_id) => self.get_node(node_id),
            ElementId::Token(_) => None,
        }
    }

    /// Get token data by element ID
    pub fn get_element_token(&self, id: ElementId) -> Option<&TokenData> {
        match id {
            ElementId::Node(_) => None,
            ElementId::Token(token_id) => self.get_token(token_id),
        }
    }

    /// Find the node containing the given text offset
    pub fn find_node_at_offset(&self, offset: TextSize) -> Option<NodeId> {
        // Start from root and traverse down
        let mut current = self.root;

        loop {
            let node = self.get_node(current)?;

            // Check if offset is within this node
            if !node.range.contains(offset) {
                return None;
            }

            // Find child containing the offset
            let mut found_child = None;
            for &child_id in &node.children {
                if let ElementId::Node(child_node_id) = child_id {
                    if let Some(child) = self.get_node(child_node_id) {
                        if child.range.contains(offset) {
                            found_child = Some(child_node_id);
                            break;
                        }
                    }
                }
            }

            match found_child {
                Some(child_id) => current = child_id,
                None => return Some(current),
            }
        }
    }

    /// Iterate over all nodes
    pub fn nodes(&self) -> impl Iterator<Item = &NodeData> {
        self.nodes.iter()
    }

    /// Iterate over all tokens
    pub fn tokens(&self) -> impl Iterator<Item = &TokenData> {
        self.tokens.iter()
    }

    /// Get total memory usage estimate
    pub fn memory_usage(&self) -> usize {
        use std::mem::size_of;

        let node_memory = self.nodes.capacity() * size_of::<NodeData>();
        let token_memory = self.tokens.capacity() * size_of::<TokenData>();
        let index_memory = self.node_index.capacity() * (size_of::<NodeId>() + size_of::<usize>());

        node_memory + token_memory + index_memory
    }

    /// Get iterator over all node IDs
    pub fn node_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.node_index.keys().copied()
    }

    /// Get the root node ID as Option for compatibility
    pub fn root_id(&self) -> Option<NodeId> {
        if self.nodes.is_empty() {
            None
        } else {
            Some(self.root)
        }
    }
}

/// Lightweight reference to a node in the AST
pub struct SyntaxNodeRef<'a> {
    ast: &'a FlatAst,
    data: &'a NodeData,
}

impl<'a> SyntaxNodeRef<'a> {
    /// Create a new node reference
    pub(crate) fn new(ast: &'a FlatAst, data: &'a NodeData) -> Self {
        Self { ast, data }
    }

    /// Get the node ID
    pub fn id(&self) -> NodeId {
        self.data.id
    }

    /// Get the node kind
    pub fn kind(&self) -> SyntaxKind {
        self.data.kind
    }

    /// Get the text range
    pub fn range(&self) -> TextRange {
        self.data.range
    }

    /// Get the parent node
    pub fn parent(&self) -> Option<SyntaxNodeRef<'a>> {
        self.data.parent.and_then(|id| {
            self.ast
                .get_node(id)
                .map(|data| SyntaxNodeRef::new(self.ast, data))
        })
    }

    /// Iterate over child nodes
    pub fn children(&'a self) -> impl Iterator<Item = SyntaxNodeRef<'a>> + 'a {
        self.data.children.iter().filter_map(move |&child_id| {
            if let ElementId::Node(node_id) = child_id {
                self.ast
                    .get_node(node_id)
                    .map(|data| SyntaxNodeRef::new(self.ast, data))
            } else {
                None
            }
        })
    }

    /// Iterate over child tokens
    pub fn tokens(&'a self) -> impl Iterator<Item = &'a TokenData> + 'a {
        self.data.children.iter().filter_map(move |&child_id| {
            if let ElementId::Token(token_id) = child_id {
                self.ast.get_token(token_id)
            } else {
                None
            }
        })
    }

    /// Get text by concatenating child tokens
    pub fn text(&self) -> String {
        let mut result = String::new();
        self.collect_text(&mut result);
        result
    }

    fn collect_text(&self, buffer: &mut String) {
        for &child_id in &self.data.children {
            match child_id {
                ElementId::Token(token_id) => {
                    if let Some(token) = self.ast.get_token(token_id) {
                        buffer.push_str(&token.text);
                    }
                },
                ElementId::Node(node_id) => {
                    if let Some(node_data) = self.ast.get_node(node_id) {
                        let child_ref = SyntaxNodeRef::new(self.ast, node_data);
                        child_ref.collect_text(buffer);
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::kind::SyntaxKind;

    #[test]
    fn test_flat_ast_creation() {
        let mut ast = FlatAst::new();

        // Create root node
        let root_data = NodeData {
            id: NodeId(0), // Will be replaced
            kind: SyntaxKind::Root,
            range: TextRange::new(0.into(), 10.into()),
            parent: None,
            children: SmallVec::new(),
        };

        let root_id = ast.add_node(root_data);
        ast.set_root(root_id);

        // Verify
        assert_eq!(ast.root(), root_id);
        assert!(ast.get_node(root_id).is_some());
    }

    #[test]
    fn test_node_traversal() {
        let mut ast = FlatAst::new();

        // Build a simple tree
        let root_id = ast.add_node(NodeData {
            id: NodeId(0),
            kind: SyntaxKind::Root,
            range: TextRange::new(0.into(), 20.into()),
            parent: None,
            children: SmallVec::new(),
        });

        let child_id = ast.add_node(NodeData {
            id: NodeId(0),
            kind: SyntaxKind::Word,
            range: TextRange::new(0.into(), 5.into()),
            parent: Some(root_id),
            children: SmallVec::new(),
        });

        // Add child to root
        if let Some(root) = ast.get_node_mut(root_id) {
            root.children.push(ElementId::Node(child_id));
        }

        // Test traversal
        let root_ref =
            SyntaxNodeRef::new(&ast, ast.get_node(root_id).expect("Root node should exist"));
        assert_eq!(root_ref.children().count(), 1);
    }

    #[test]
    fn test_find_node_at_offset() {
        let mut ast = FlatAst::new();

        let root_id = ast.add_node(NodeData {
            id: NodeId(0),
            kind: SyntaxKind::Root,
            range: TextRange::new(0.into(), 20.into()),
            parent: None,
            children: SmallVec::new(),
        });
        ast.set_root(root_id);

        // Should find root for offset within range
        assert_eq!(ast.find_node_at_offset(5.into()), Some(root_id));

        // Should return None for offset outside range
        assert_eq!(ast.find_node_at_offset(25.into()), None);
    }
}
