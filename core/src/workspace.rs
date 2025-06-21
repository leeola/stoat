use crate::{
    node::{Node, NodeId},
    nodes::csv::CsvSourceNode,
    transform::Transformation,
    view::View,
    Result,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Default)]
pub struct Workspace {
    nodes: HashMap<NodeId, Box<dyn Node>>,
    csv_nodes: HashMap<NodeId, CsvSourceNode>,
    links: Vec<Link>,
    view: View,
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
        let mut nodes = Vec::new();

        // Add CSV nodes
        for (id, csv_node) in &workspace.csv_nodes {
            nodes.push(SerializableNode {
                id: *id,
                node_type: csv_node.node_type().to_string(),
                name: csv_node.name().to_string(),
                config: {
                    let config_values = csv_node.get_config_values();
                    if config_values.is_empty() {
                        crate::value::Value::Empty
                    } else {
                        use crate::value::Map;
                        let mut config_map = indexmap::IndexMap::new();
                        for (key, value) in config_values {
                            config_map.insert(compact_str::CompactString::from(key), value);
                        }
                        crate::value::Value::Map(Map(config_map))
                    }
                },
            });
        }

        // Add other nodes
        for (id, node) in &workspace.nodes {
            nodes.push(SerializableNode {
                id: *id,
                node_type: node.node_type().to_string(),
                name: node.name().to_string(),
                config: Self::get_node_config_for_serialization(node.as_ref()),
            });
        }

        Self {
            links: workspace.links.clone(),
            nodes,
            view_data: None, // TODO: implement view serialization
        }
    }
}

impl SerializableWorkspace {
    /// Get configuration for serialization from a node's config sockets
    ///
    /// This method converts config socket values to a format suitable for serialization
    /// and node reconstruction through the NodeInit registry.
    fn get_node_config_for_serialization(node: &dyn crate::node::Node) -> crate::value::Value {
        let config_values = node.get_config_values();

        if config_values.is_empty() {
            // No config sockets - return empty value
            crate::value::Value::Empty
        } else {
            // Convert config socket values to a map for serialization
            // This ensures that NodeInit implementations receive the expected map format
            use crate::value::Map;
            let mut config_map = indexmap::IndexMap::new();
            for (key, value) in config_values {
                config_map.insert(compact_str::CompactString::from(key), value);
            }
            crate::value::Value::Map(Map(config_map))
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
        let mut csv_nodes = HashMap::new();

        // Reconstruct nodes using the registry
        for serializable_node in serializable.nodes {
            let node_name = serializable_node.name.clone();
            let node_type = serializable_node.node_type.clone();

            // Handle CSV nodes specially - create them directly
            if serializable_node.node_type == "csv" {
                // Extract file path from config
                let file_path = match &serializable_node.config {
                    crate::value::Value::String(path) => path.to_string(),
                    crate::value::Value::Map(ref map) => {
                        if let Some(path_value) = map.0.get("path") {
                            match path_value {
                                crate::value::Value::String(path) => path.to_string(),
                                _ => String::new(),
                            }
                        } else {
                            String::new()
                        }
                    },
                    _ => String::new(),
                };

                let csv_node =
                    CsvSourceNode::new(serializable_node.id, serializable_node.name, file_path);
                csv_nodes.insert(serializable_node.id, csv_node);
            } else {
                // Handle other node types through registry
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
        }

        Self {
            nodes,
            csv_nodes,
            links: serializable.links,
            view: View::default(), // TODO: deserialize view from view_data
        }
    }

    /// Add a node to the workspace with a specific ID
    pub fn add_node_with_id(&mut self, id: NodeId, node: Box<dyn Node>) {
        // Get node type before moving
        let node_type = node.node_type();

        // Add to general nodes (we'll handle CSV nodes in a dedicated method)
        self.nodes.insert(id, node);

        // Add to view
        self.view.add_node_view(id, node_type, (0, 0));
    }

    /// Add a CSV node directly to the workspace
    pub fn add_csv_node(&mut self, id: NodeId, csv_node: CsvSourceNode) {
        let node_type = csv_node.node_type();

        // Add to CSV nodes collection
        self.csv_nodes.insert(id, csv_node);

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
                let from_outputs = if let Some(csv_node) = self.csv_nodes.get_mut(&link.from_node) {
                    csv_node.execute(&HashMap::new())?
                } else if let Some(from_node) = self.nodes.get_mut(&link.from_node) {
                    from_node.execute(&HashMap::new())?
                } else {
                    return Err(crate::Error::Node {
                        message: format!("Source node {:?} not found", link.from_node),
                    });
                };

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
        if let Some(csv_node) = self.csv_nodes.get_mut(&node_id) {
            csv_node.execute(&inputs)
        } else if let Some(node) = self.nodes.get_mut(&node_id) {
            node.execute(&inputs)
        } else {
            Err(crate::Error::Node {
                message: format!("Node {:?} not found", node_id),
            })
        }
    }

    pub fn get_node(&self, id: NodeId) -> Option<&dyn Node> {
        if let Some(csv_node) = self.csv_nodes.get(&id) {
            Some(csv_node as &dyn Node)
        } else {
            self.nodes.get(&id).map(|n| n.as_ref())
        }
    }

    pub fn list_nodes(&self) -> Vec<(NodeId, &dyn Node)> {
        let mut nodes = Vec::new();

        // Add CSV nodes
        for (id, csv_node) in &self.csv_nodes {
            nodes.push((*id, csv_node as &dyn Node));
        }

        // Add other nodes
        for (id, node) in &self.nodes {
            nodes.push((*id, node.as_ref()));
        }

        nodes
    }

    pub fn view(&self) -> &View {
        &self.view
    }
}
