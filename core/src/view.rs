use crate::node::{NodeId, NodeType};
use serde::{Deserialize, Serialize};

/// Position in a grid coordinate system
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct GridPosition {
    pub row: i32,
    pub col: i32,
}

impl GridPosition {
    pub fn new(row: i32, col: i32) -> Self {
        Self { row, col }
    }

    /// Offset this position by the given delta
    pub fn offset(&self, row_delta: i32, col_delta: i32) -> Self {
        Self {
            row: self.row + row_delta,
            col: self.col + col_delta,
        }
    }

    /// Manhattan distance between two positions
    pub fn distance(&self, other: &Self) -> i32 {
        (self.row - other.row).abs() + (self.col - other.col).abs()
    }
}

#[derive(Debug, Default, Clone)]
pub struct View {
    pub nodes: Vec<NodeView>,
}

impl View {
    pub fn add_node_view(&mut self, id: NodeId, node_type: NodeType, pos: GridPosition) {
        self.nodes.push(NodeView { id, node_type, pos });
    }
}

#[derive(Debug, Clone)]
pub struct NodeView {
    pub id: NodeId,
    pub node_type: NodeType,
    pub pos: GridPosition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewEvent {
    Close,
}
