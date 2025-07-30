use crate::{
    node::{NodeId, SSTNode},
    syntax::{LanguageId, SemanticKind},
    tree::SyntaxTree,
};
use ropey::Rope;
use std::collections::HashMap;

/// Builder for constructing syntax trees
pub struct TreeBuilder {
    pub(crate) rope: Rope,
    pub(crate) nodes: HashMap<NodeId, SSTNode>,
    pub(crate) next_id: usize,
    pub(crate) current_parent: Option<NodeId>,
    pub(crate) language: LanguageId,
    /// Stack of nodes being built (for nested structures)
    node_stack: Vec<(NodeId, SemanticKind, String, usize)>,
}

impl TreeBuilder {
    /// Create a new tree builder
    pub fn new(text: &str, language: LanguageId) -> Self {
        let rope = Rope::from_str(text);
        let mut nodes = HashMap::new();

        // Create root node
        let root_node = SSTNode::new(
            SemanticKind::Module,
            "root".to_string(),
            language,
            0,
            rope.len_bytes(),
        );

        let root_id = NodeId(0);
        nodes.insert(root_id, root_node);

        Self {
            rope,
            nodes,
            next_id: 1,
            current_parent: Some(root_id),
            language,
            node_stack: Vec::new(),
        }
    }

    /// Start a new composite node
    pub fn start_node(&mut self, semantic_kind: SemanticKind, syntax_kind: String, start: usize) {
        let node_id = NodeId(self.next_id);
        self.next_id += 1;

        // Push current node info onto stack
        self.node_stack
            .push((node_id, semantic_kind, syntax_kind, start));

        // This node becomes the new parent for children
        self.current_parent = Some(node_id);
    }

    /// Finish the current composite node
    pub fn finish_node(&mut self, end: usize) -> NodeId {
        let (node_id, semantic_kind, syntax_kind, start) = self
            .node_stack
            .pop()
            .expect("finish_node called without matching start_node");

        // Create the node
        let mut node = SSTNode::new(semantic_kind, syntax_kind, self.language, start, end);

        // Set parent from before this node was started
        let parent_of_finished = if self.node_stack.is_empty() {
            Some(NodeId(0)) // Root is parent
        } else {
            Some(self.node_stack.last().unwrap().0)
        };

        node.parent = parent_of_finished;

        // Add this node as child of its parent
        if let Some(parent_id) = parent_of_finished {
            if let Some(parent) = self.nodes.get_mut(&parent_id) {
                parent.children.push(node_id);
            }
        }

        // Insert the node
        self.nodes.insert(node_id, node);

        // Update current parent to the parent of the node we just finished
        self.current_parent = parent_of_finished;

        node_id
    }

    /// Add a token node
    pub fn add_token(
        &mut self,
        semantic_kind: SemanticKind,
        syntax_kind: String,
        start: usize,
        end: usize,
    ) -> NodeId {
        let node_id = NodeId(self.next_id);
        self.next_id += 1;

        let mut node = SSTNode::new(semantic_kind, syntax_kind, self.language, start, end);

        // Set parent
        node.parent = self.current_parent;

        // Add as child of current parent
        if let Some(parent_id) = self.current_parent {
            if let Some(parent) = self.nodes.get_mut(&parent_id) {
                parent.children.push(node_id);
            }
        }

        self.nodes.insert(node_id, node);
        node_id
    }

    /// Build the final syntax tree
    pub fn finish(self) -> SyntaxTree {
        assert!(self.node_stack.is_empty(), "Unfinished nodes in builder");

        SyntaxTree {
            rope: self.rope,
            nodes: self.nodes,
            root: NodeId(0),
            next_id: self.next_id,
        }
    }
}
