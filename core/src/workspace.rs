use crate::{
    node::{Node, NodeId},
    transform::Transformation,
    view::View,
    Result,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub struct Workspace {
    nodes: HashMap<NodeId, Box<dyn Node>>,
    links: Vec<Link>,
    view: View,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            nodes: HashMap::new(),
            links: Vec::new(),
            view: View::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub from_node: NodeId,
    pub from_port: String,
    pub to_node: NodeId,
    pub to_port: String,
    pub transformation: Option<Transformation>,
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
            .iter()
            .map(|(id, node)| SerializableNode {
                id: *id,
                node_type: node.node_type().to_string(),
                name: node.name().to_string(),
                config: crate::value::Value::Null, /* TODO: implement node-specific config
                                                    * serialization */
            })
            .collect();

        Self {
            links: workspace.links.clone(),
            nodes,
            view_data: None, // TODO: implement view serialization
        }
    }
}

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a workspace from a serializable representation
    pub fn from_serializable(serializable: SerializableWorkspace) -> Self {
        let mut nodes = HashMap::new();

        // Reconstruct nodes using the registry
        for serializable_node in serializable.nodes {
            let node_name = serializable_node.name.clone();
            let node_type = serializable_node.node_type.clone();
            match crate::node::create_node_from_registry(
                &serializable_node.node_type,
                serializable_node.id,
                serializable_node.name,
                serializable_node.config,
            ) {
                Ok(node) => {
                    nodes.insert(serializable_node.id, node);
                },
                Err(e) => {
                    eprintln!(
                        "Warning: Failed to reconstruct node {} ({}): {}",
                        node_name, node_type, e
                    );
                },
            }
        }

        Self {
            nodes,
            links: serializable.links,
            view: View::default(), // TODO: deserialize view from view_data
        }
    }

    /// Add a node to the workspace with a specific ID
    pub fn add_node_with_id(&mut self, id: NodeId, node: Box<dyn Node>) {
        // Get node type before moving
        let node_type = node.node_type();

        // Add to nodes
        self.nodes.insert(id, node);

        // Add to view
        self.view.add_node_view(id, node_type, (0, 0));
    }

    /// Create a link between two nodes
    pub fn link_nodes(
        &mut self,
        from_node: NodeId,
        from_port: String,
        to_node: NodeId,
        to_port: String,
    ) -> Result<()> {
        self.link_nodes_with_transform(from_node, from_port, to_node, to_port, None)
    }

    /// Create a link between two nodes with an optional transformation
    pub fn link_nodes_with_transform(
        &mut self,
        from_node: NodeId,
        from_port: String,
        to_node: NodeId,
        to_port: String,
        transformation: Option<Transformation>,
    ) -> Result<()> {
        // Validate nodes exist
        if !self.nodes.contains_key(&from_node) {
            return Err(crate::Error::Node {
                message: format!("Source node {:?} not found", from_node),
            });
        }
        if !self.nodes.contains_key(&to_node) {
            return Err(crate::Error::Node {
                message: format!("Target node {:?} not found", to_node),
            });
        }

        // TODO: Validate port names exist on nodes

        let link = Link {
            from_node,
            from_port,
            to_node,
            to_port,
            transformation,
        };

        self.links.push(link);
        Ok(())
    }

    /// Execute a specific node and return its outputs
    pub fn execute_node(
        &mut self,
        node_id: NodeId,
    ) -> Result<HashMap<String, crate::value::Value>> {
        // Simple execution - collect inputs from linked nodes
        let mut inputs = HashMap::new();

        // Clone links to avoid borrowing issues
        let links = self.links.clone();

        for link in &links {
            if link.to_node == node_id {
                // Execute source node first (simplified - no cycle detection)
                let from_node =
                    self.nodes
                        .get_mut(&link.from_node)
                        .ok_or_else(|| crate::Error::Node {
                            message: format!("Source node {:?} not found", link.from_node),
                        })?;

                let from_outputs = from_node.execute(&HashMap::new())?;

                if let Some(value) = from_outputs.get(&link.from_port) {
                    // Apply transformation if present
                    let transformed_value = if let Some(ref transformation) = link.transformation {
                        transformation.apply(value)?
                    } else {
                        value.clone()
                    };
                    inputs.insert(link.to_port.clone(), transformed_value);
                }
            }
        }

        // Execute the target node
        let node = self
            .nodes
            .get_mut(&node_id)
            .ok_or_else(|| crate::Error::Node {
                message: format!("Node {:?} not found", node_id),
            })?;

        node.execute(&inputs)
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
}
