use crate::syntax::{LanguageId, SemanticKind};

/// Unique identifier for nodes in the syntax tree
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub(crate) usize);

/// A node in the syntax tree, referencing text via byte offsets
#[derive(Debug, Clone)]
pub struct SSTNode {
    /// Semantic kind for universal queries
    pub semantic_kind: SemanticKind,
    /// Original tree-sitter node type (e.g., "function_item", "function_definition")
    pub syntax_kind: String,
    /// Language this node belongs to
    pub language: LanguageId,
    /// Byte offset where this node starts in the rope
    pub start: usize,
    /// Byte offset where this node ends in the rope
    pub end: usize,
    /// Parent node, if any
    pub parent: Option<NodeId>,
    /// Child nodes in order
    pub children: Vec<NodeId>,
}

impl SSTNode {
    /// Create a new SST node
    pub fn new(
        semantic_kind: SemanticKind,
        syntax_kind: String,
        language: LanguageId,
        start: usize,
        end: usize,
    ) -> Self {
        Self {
            semantic_kind,
            syntax_kind,
            language,
            start,
            end,
            parent: None,
            children: Vec::new(),
        }
    }

    /// Get the text span length in bytes
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Check if this is a leaf node (no children)
    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}
