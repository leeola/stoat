use crate::{
    node::{Node, NodeId},
    view::View,
    Result,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Default)]
pub struct Workspace {
    // Node storage will be added here when new node types are implemented
    links: Vec<Link>,
    view: View,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub from_node: NodeId,
    pub from_port: String,
    pub to_node: NodeId,
    pub to_port: String,
    // Transformation field removed - transformations were data-specific
}

/// Serializable representation of a node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableNode {
    pub id: NodeId,
    pub node_type: String,
    pub name: String,
    pub config: crate::value::Value, // Node-specific configuration
}

/// Serializable representation of workspace state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableWorkspace {
    pub links: Vec<Link>,
    pub nodes: Vec<SerializableNode>,
    pub view_data: Option<String>, // Simplified view serialization
}

impl From<&Workspace> for SerializableWorkspace {
    fn from(workspace: &Workspace) -> Self {
        let nodes = Vec::new(); // No nodes to serialize yet

        Self {
            links: workspace.links.clone(),
            nodes,
            view_data: None, // TODO: implement view serialization
        }
    }
}

impl SerializableWorkspace {}

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a workspace from a serializable representation
    pub fn from_serializable(serializable: SerializableWorkspace) -> Self {
        // No node types to reconstruct yet
        if !serializable.nodes.is_empty() {
            eprintln!(
                "Warning: {} nodes in serialized workspace cannot be reconstructed - no node types implemented",
                serializable.nodes.len()
            );
        }

        Self {
            links: serializable.links,
            view: View::default(), // TODO: deserialize view from view_data
        }
    }

    /// Create a link between two nodes
    pub fn link_nodes(
        &mut self,
        _from_node: NodeId,
        _from_port: String,
        _to_node: NodeId,
        _to_port: String,
    ) -> Result<()> {
        // Currently no nodes exist to validate
        Err(crate::Error::Node {
            message: "No node types implemented yet".to_string(),
        })
    }

    /// Execute a specific node and return its outputs
    pub fn execute_node(
        &mut self,
        _node_id: NodeId,
    ) -> Result<HashMap<String, crate::value::Value>> {
        // No nodes exist to execute
        Err(crate::Error::Node {
            message: "No node types implemented yet".to_string(),
        })
    }

    pub fn get_node(&self, _id: NodeId) -> Option<&dyn Node> {
        // No nodes exist
        None
    }

    pub fn list_nodes(&self) -> Vec<(NodeId, &dyn Node)> {
        // No nodes exist
        Vec::new()
    }

    pub fn view(&self) -> &View {
        &self.view
    }
}
