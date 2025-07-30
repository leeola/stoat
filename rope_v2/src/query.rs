use crate::{
    node::{NodeId, SSTNode},
    syntax::SemanticKind,
    tree::SyntaxTree,
};

/// Query builder for finding nodes
pub struct NodeQuery {
    pub(crate) kind_filter: Option<SemanticKind>,
    pub(crate) ancestor_filter: Option<SemanticKind>,
}

impl NodeQuery {
    /// Create a new query
    pub fn new() -> Self {
        Self {
            kind_filter: None,
            ancestor_filter: None,
        }
    }

    /// Filter by semantic kind
    pub fn with_kind(mut self, kind: SemanticKind) -> Self {
        self.kind_filter = Some(kind);
        self
    }

    /// Filter by ancestor kind
    pub fn with_ancestor(mut self, kind: SemanticKind) -> Self {
        self.ancestor_filter = Some(kind);
        self
    }

    /// Execute the query on a syntax tree
    pub fn execute(&self, tree: &SyntaxTree) -> Vec<NodeId> {
        let mut results = Vec::new();

        // First, collect all nodes matching the kind filter (or all nodes if no filter)
        let candidates: Vec<NodeId> = if let Some(kind) = self.kind_filter {
            tree.find_by_kind(kind)
        } else {
            tree.nodes.keys().copied().collect()
        };

        // Then filter by ancestor if specified
        for node_id in candidates {
            if let Some(ancestor_kind) = self.ancestor_filter {
                if tree.is_in_context(node_id, ancestor_kind) {
                    results.push(node_id);
                }
            } else {
                results.push(node_id);
            }
        }

        results
    }
}

/// Cursor for traversing the syntax tree
pub struct TreeCursor {
    pub(crate) tree: *const SyntaxTree,
    pub(crate) current: NodeId,
}

impl TreeCursor {
    /// Create a cursor starting at the root
    pub fn new(tree: &SyntaxTree) -> Self {
        Self {
            tree: tree as *const SyntaxTree,
            current: tree.root,
        }
    }

    /// Get the current node
    pub fn node(&self) -> &SSTNode {
        unsafe {
            (*self.tree)
                .node(self.current)
                .expect("Cursor points to invalid node")
        }
    }

    /// Move to parent node
    pub fn goto_parent(&mut self) -> bool {
        let current_node = unsafe { (*self.tree).node(self.current) };
        if let Some(node) = current_node {
            if let Some(parent_id) = node.parent {
                self.current = parent_id;
                return true;
            }
        }
        false
    }

    /// Move to first child
    pub fn goto_first_child(&mut self) -> bool {
        let current_node = unsafe { (*self.tree).node(self.current) };
        if let Some(node) = current_node {
            if let Some(&first_child) = node.children.first() {
                self.current = first_child;
                return true;
            }
        }
        false
    }

    /// Move to next sibling
    pub fn goto_next_sibling(&mut self) -> bool {
        let current_node = unsafe { (*self.tree).node(self.current) };
        if let Some(node) = current_node {
            if let Some(parent_id) = node.parent {
                let parent = unsafe { (*self.tree).node(parent_id) };
                if let Some(parent_node) = parent {
                    // Find current node in parent's children
                    if let Some(pos) = parent_node
                        .children
                        .iter()
                        .position(|&id| id == self.current)
                    {
                        // Check if there's a next sibling
                        if pos + 1 < parent_node.children.len() {
                            self.current = parent_node.children[pos + 1];
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Move to a specific child index
    pub fn goto_child(&mut self, index: usize) -> bool {
        let current_node = unsafe { (*self.tree).node(self.current) };
        if let Some(node) = current_node {
            if let Some(&child_id) = node.children.get(index) {
                self.current = child_id;
                return true;
            }
        }
        false
    }
}
