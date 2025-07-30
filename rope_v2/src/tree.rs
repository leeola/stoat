use crate::{
    node::{NodeId, SSTNode},
    syntax::{LanguageId, SemanticKind},
};
use ropey::Rope;
use std::collections::HashMap;

/// The syntax tree structure holding all nodes and the source text
pub struct SyntaxTree {
    /// Source text stored as a rope
    pub(crate) rope: Rope,
    /// All nodes in the tree, indexed by NodeId
    pub(crate) nodes: HashMap<NodeId, SSTNode>,
    /// The root node of the tree
    pub(crate) root: NodeId,
    /// Counter for generating unique NodeIds
    pub(crate) next_id: usize,
}

impl SyntaxTree {
    /// Create a new syntax tree from source text
    pub fn new(text: &str) -> Self {
        let rope = Rope::from_str(text);
        let mut nodes = HashMap::new();

        // Create root node spanning entire text
        let root_node = SSTNode::new(
            SemanticKind::Module,
            "root".to_string(),
            LanguageId::Rust, // Default to Rust, should be parameterized
            0,
            rope.len_bytes(),
        );

        let root_id = NodeId(0);
        nodes.insert(root_id, root_node);

        Self {
            rope,
            nodes,
            root: root_id,
            next_id: 1,
        }
    }

    /// Get the rope containing the source text
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// Get a node by its ID
    pub fn node(&self, id: NodeId) -> Option<&SSTNode> {
        self.nodes.get(&id)
    }

    /// Get a mutable node by its ID
    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut SSTNode> {
        self.nodes.get_mut(&id)
    }

    /// Get the root node
    pub fn root(&self) -> &SSTNode {
        self.nodes
            .get(&self.root)
            .expect("Tree must have a root node")
    }

    /// Get the text slice for a node
    pub fn node_text(&self, id: NodeId) -> Option<String> {
        let node = self.node(id)?;
        let start_char = self.rope.byte_to_char(node.start);
        let end_char = self.rope.byte_to_char(node.end);
        Some(self.rope.slice(start_char..end_char).to_string())
    }

    /// Add a new node to the tree
    pub fn add_node(&mut self, node: SSTNode) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        self.nodes.insert(id, node);
        id
    }

    /// Update byte offsets after a text edit
    pub fn update_offsets(&mut self, edit_pos: usize, delta: isize) {
        for node in self.nodes.values_mut() {
            // Update nodes that start after the edit position
            if node.start > edit_pos {
                node.start = (node.start as isize + delta) as usize;
            }
            // Update nodes that end after the edit position
            if node.end > edit_pos {
                node.end = (node.end as isize + delta) as usize;
            }
        }
    }

    /// Find all nodes of a specific semantic kind
    pub fn find_by_kind(&self, kind: SemanticKind) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.semantic_kind == kind)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Get the path from root to a node
    pub fn path_to_node(&self, id: NodeId) -> Vec<NodeId> {
        let mut path = Vec::new();
        let mut current = Some(id);

        // Build path from node to root
        while let Some(node_id) = current {
            path.push(node_id);
            current = self.node(node_id).and_then(|n| n.parent);
        }

        // Reverse to get path from root to node
        path.reverse();
        path
    }

    /// Check if a node is within a specific context (e.g., "in function")
    pub fn is_in_context(&self, id: NodeId, context_kind: SemanticKind) -> bool {
        let mut current = self.node(id).and_then(|n| n.parent);

        while let Some(parent_id) = current {
            if let Some(parent_node) = self.node(parent_id) {
                if parent_node.semantic_kind == context_kind {
                    return true;
                }
                current = parent_node.parent;
            } else {
                break;
            }
        }

        false
    }

    /// Edit text in the rope and update node offsets
    pub fn edit_text(&mut self, start: usize, end: usize, new_text: &str) {
        let start_char = self.rope.byte_to_char(start);
        let end_char = self.rope.byte_to_char(end);

        // Calculate the byte delta for offset updates
        let old_len = end - start;
        let new_len = new_text.len();
        let delta = new_len as isize - old_len as isize;

        // Perform the edit on the rope
        self.rope.remove(start_char..end_char);
        self.rope.insert(start_char, new_text);

        // Update node offsets
        self.update_offsets(start, delta);

        // Update root node to span entire text
        if let Some(root) = self.nodes.get_mut(&self.root) {
            root.end = self.rope.len_bytes();
        }
    }

    /// Insert text at a specific position
    pub fn insert_text(&mut self, pos: usize, text: &str) {
        self.edit_text(pos, pos, text);
    }

    /// Delete text in a range
    pub fn delete_text(&mut self, start: usize, end: usize) {
        self.edit_text(start, end, "");
    }

    /// Remove a node from the tree
    pub fn remove_node(&mut self, id: NodeId) -> Option<SSTNode> {
        let node = self.nodes.remove(&id)?;

        // Remove from parent's children list
        if let Some(parent_id) = node.parent {
            if let Some(parent) = self.nodes.get_mut(&parent_id) {
                parent.children.retain(|&child_id| child_id != id);
            }
        }

        // Orphan all children (they remain in tree but lose parent reference)
        for &child_id in &node.children {
            if let Some(child) = self.nodes.get_mut(&child_id) {
                child.parent = None;
            }
        }

        Some(node)
    }

    /// Replace an existing node
    pub fn replace_node(&mut self, id: NodeId, mut new_node: SSTNode) -> Option<SSTNode> {
        let old_node = self.nodes.get(&id)?;

        // Preserve parent and children relationships
        new_node.parent = old_node.parent;
        new_node.children = old_node.children.clone();

        self.nodes.insert(id, new_node)
    }

    /// Insert a child at a specific position in parent's children list
    pub fn insert_child(&mut self, parent_id: NodeId, index: usize, child_id: NodeId) -> bool {
        // Update child's parent reference
        if let Some(child) = self.nodes.get_mut(&child_id) {
            child.parent = Some(parent_id);
        } else {
            return false;
        }

        // Insert into parent's children list
        if let Some(parent) = self.nodes.get_mut(&parent_id) {
            parent
                .children
                .insert(index.min(parent.children.len()), child_id);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test utility: create a tree with a simple structure
    fn test_tree() -> SyntaxTree {
        SyntaxTree::new("fn main() { let x = 42; }")
    }

    // Test utility: add a node and return its ID
    fn add_test_node(
        tree: &mut SyntaxTree,
        kind: SemanticKind,
        start: usize,
        end: usize,
    ) -> NodeId {
        tree.add_node(SSTNode::new(
            kind,
            "test".to_string(),
            LanguageId::Rust,
            start,
            end,
        ))
    }

    #[test]
    fn text_editing() {
        let mut tree = test_tree();

        // Insert
        tree.insert_text(9, " world");
        assert_eq!(tree.rope().to_string(), "fn main() world { let x = 42; }");

        // Delete
        tree.delete_text(9, 15);
        assert_eq!(tree.rope().to_string(), "fn main() { let x = 42; }");

        // Replace
        tree.edit_text(20, 22, "100");
        assert_eq!(tree.rope().to_string(), "fn main() { let x = 100; }");
    }

    #[test]
    fn node_manipulation() {
        let mut tree = test_tree();
        let fn_id = add_test_node(&mut tree, SemanticKind::Function, 0, 25);
        let block_id = add_test_node(&mut tree, SemanticKind::Block, 10, 25);

        // Insert child relationship
        assert!(tree.insert_child(fn_id, 0, block_id));
        assert_eq!(tree.node(fn_id).unwrap().children, vec![block_id]);
        assert_eq!(tree.node(block_id).unwrap().parent, Some(fn_id));

        // Replace node
        let new_fn = SSTNode::new(
            SemanticKind::Function,
            "fn_item".to_string(),
            LanguageId::Rust,
            0,
            30,
        );
        tree.replace_node(fn_id, new_fn);
        assert_eq!(tree.node(fn_id).unwrap().syntax_kind, "fn_item");
        assert_eq!(tree.node(fn_id).unwrap().children, vec![block_id]); // Children preserved

        // Remove node
        let removed = tree.remove_node(fn_id).unwrap();
        assert_eq!(removed.semantic_kind, SemanticKind::Function);
        assert!(tree.node(fn_id).is_none());
        assert!(tree.node(block_id).unwrap().parent.is_none()); // Child orphaned
    }
}
