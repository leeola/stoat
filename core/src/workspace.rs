use crate::{
    graph::NodeGraph,
    node::{Node, NodeId},
    view::View,
    view_state::ViewState,
    Result,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Default)]
pub struct Workspace {
    nodes: HashMap<NodeId, Box<dyn Node>>,
    links: Vec<Link>,
    view: View,
    graph: NodeGraph,
    view_state: ViewState,
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
        let nodes = workspace
            .nodes
            .values()
            .map(|node| SerializableNode {
                id: node.id(),
                node_type: node.node_type().to_string(),
                name: node.name().to_string(),
                config: {
                    let config_values = node.get_config_values();
                    let mut map = indexmap::IndexMap::new();
                    for (k, v) in config_values {
                        map.insert(k.into(), v);
                    }
                    crate::value::Value::Map(crate::value::Map(map))
                },
            })
            .collect();

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

    /// Add a node to the workspace
    pub fn add_node(&mut self, node: Box<dyn Node>) -> NodeId {
        let id = node.id();
        self.nodes.insert(id, node);
        id
    }

    /// Create a workspace from a serializable representation
    pub fn from_serializable(serializable: SerializableWorkspace) -> Self {
        use crate::node::create_node_from_registry;

        let mut nodes = HashMap::new();

        // Reconstruct nodes from serialized data
        for node_data in serializable.nodes {
            match create_node_from_registry(
                &node_data.node_type,
                node_data.id,
                node_data.name,
                node_data.config,
            ) {
                Ok(node) => {
                    nodes.insert(node.id(), node);
                },
                Err(e) => {
                    eprintln!("Failed to reconstruct node {}: {}", node_data.id.0, e);
                },
            }
        }

        Self {
            nodes,
            links: serializable.links,
            view: View::default(), // TODO: deserialize view from view_data
            graph: NodeGraph::default(),
            view_state: ViewState::default(),
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

    pub fn get_node(&self, id: NodeId) -> Option<&dyn Node> {
        self.nodes.get(&id).map(|n| n.as_ref())
    }

    pub fn list_nodes(&self) -> Vec<(NodeId, &dyn Node)> {
        self.nodes
            .iter()
            .map(|(id, node)| (*id, node.as_ref()))
            .collect()
    }

    pub fn view(&self) -> &View {
        &self.view
    }

    pub fn view_mut(&mut self) -> &mut View {
        &mut self.view
    }

    /// Get the node graph for querying relationships
    pub fn graph(&self) -> &NodeGraph {
        &self.graph
    }

    /// Get mutable access to the graph
    pub fn graph_mut(&mut self) -> &mut NodeGraph {
        &mut self.graph
    }

    /// Get the view state for rendering
    pub fn view_state(&self) -> &ViewState {
        &self.view_state
    }

    /// Get mutable access to view state
    pub fn view_state_mut(&mut self) -> &mut ViewState {
        &mut self.view_state
    }

    /// Initialize view layout for all nodes
    pub fn initialize_layout(&mut self) {
        let node_ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        self.view_state.initialize_default_layout(&node_ids);
    }
}
