use crate::node::NodeType;

#[derive(Debug, Default, Clone)]
pub struct View {
    pub nodes: Vec<NodeView>,
}

#[derive(Debug, Clone)]
pub struct NodeView {
    pub node_type: NodeType,
    pub pos: (usize, usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewEvent {
    Close,
}
