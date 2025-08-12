use crate::node::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The graph structure representing relationships between nodes
/// This is persisted as part of the workspace
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeGraph {
    /// All edges in the graph
    edges: Vec<Edge>,
    /// Node metadata (non-positional)
    node_info: HashMap<NodeId, NodeInfo>,
}

/// Information about a node (non-positional)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    /// User-visible label for the node
    pub label: Option<String>,
    /// Tags for categorization
    pub tags: Vec<String>,
}

/// An edge between two nodes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    pub edge_type: EdgeType,
}

/// Types of relationships between nodes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeType {
    /// Data flows from source to target
    DataFlow,
    /// Target depends on source
    Dependency,
    /// Source contains target (parent-child)
    Parent,
    /// Source precedes target in sequence
    Sequence,
    /// User-defined link
    Custom,
}

impl NodeGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an edge to the graph
    pub fn add_edge(&mut self, from: NodeId, to: NodeId, edge_type: EdgeType) {
        self.edges.push(Edge {
            from,
            to,
            edge_type,
        });
    }

    /// Remove all edges between two nodes
    pub fn remove_edges_between(&mut self, a: NodeId, b: NodeId) {
        self.edges
            .retain(|e| !((e.from == a && e.to == b) || (e.from == b && e.to == a)));
    }

    /// Get all edges from a specific node
    pub fn edges_from(&self, node: NodeId) -> Vec<&Edge> {
        self.edges.iter().filter(|e| e.from == node).collect()
    }

    /// Get all edges to a specific node
    pub fn edges_to(&self, node: NodeId) -> Vec<&Edge> {
        self.edges.iter().filter(|e| e.to == node).collect()
    }

    /// Get all nodes connected to a specific node
    pub fn neighbors(&self, node: NodeId) -> Vec<NodeId> {
        let mut neighbors = Vec::new();
        for edge in &self.edges {
            if edge.from == node {
                neighbors.push(edge.to);
            } else if edge.to == node {
                neighbors.push(edge.from);
            }
        }
        neighbors.sort_by_key(|n| n.0);
        neighbors.dedup();
        neighbors
    }

    /// Add or update node info
    pub fn set_node_info(&mut self, id: NodeId, info: NodeInfo) {
        self.node_info.insert(id, info);
    }

    /// Get node info
    pub fn get_node_info(&self, id: NodeId) -> Option<&NodeInfo> {
        self.node_info.get(&id)
    }
}
