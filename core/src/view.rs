use crate::node::{NodeId, NodeType};

#[derive(Debug, Default, Clone)]
pub struct View {
    pub nodes: Vec<NodeView>,
}

impl View {
    pub fn add_node_view(&mut self, id: NodeId, node_type: NodeType, pos: (usize, usize)) {
        self.nodes.push(NodeView { id, node_type, pos });
    }
}

#[derive(Debug, Clone)]
pub struct NodeView {
    pub id: NodeId,
    pub node_type: NodeType,
    pub pos: (usize, usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewEvent {
    Close,
}
