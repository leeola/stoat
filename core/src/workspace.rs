use crate::{
    node::{Node, NodeId},
    nodes::{csv::CsvSourceNode, json::JsonSourceNode, map::MapNode, table::TableViewerNode},
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
    json_nodes: HashMap<NodeId, JsonSourceNode>,
    map_nodes: HashMap<NodeId, MapNode>,
    table_nodes: HashMap<NodeId, TableViewerNode>,
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

        // Add JSON nodes
        for (id, json_node) in &workspace.json_nodes {
            nodes.push(SerializableNode {
                id: *id,
                node_type: json_node.node_type().to_string(),
                name: json_node.name().to_string(),
                config: {
                    let config_values = json_node.get_config_values();
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

        // Add Map nodes
        for (id, map_node) in &workspace.map_nodes {
            nodes.push(SerializableNode {
                id: *id,
                node_type: map_node.node_type().to_string(),
                name: map_node.name().to_string(),
                config: {
                    let config_values = map_node.get_config_values();
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

        // Add Table nodes
        for (id, table_node) in &workspace.table_nodes {
            nodes.push(SerializableNode {
                id: *id,
                node_type: table_node.node_type().to_string(),
                name: table_node.name().to_string(),
                config: {
                    let config_values = table_node.get_config_values();
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
        let mut json_nodes = HashMap::new();
        let map_nodes = HashMap::new();
        let mut table_nodes = HashMap::new();

        // Reconstruct nodes using the registry
        for serializable_node in serializable.nodes {
            let node_name = serializable_node.name.clone();
            let node_type = serializable_node.node_type.clone();

            // Handle special node types - create them directly
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
            } else if serializable_node.node_type == "json" {
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

                let json_node =
                    JsonSourceNode::new(serializable_node.id, serializable_node.name, file_path);
                json_nodes.insert(serializable_node.id, json_node);
            } else if serializable_node.node_type == "table" {
                // Extract cache_dir from config
                let cache_dir = match &serializable_node.config {
                    crate::value::Value::Map(ref map) => {
                        if let Some(cache_dir_value) = map.0.get("cache_dir") {
                            match cache_dir_value {
                                crate::value::Value::String(path) => {
                                    std::path::PathBuf::from(path.as_str())
                                },
                                _ => std::path::PathBuf::from("/tmp"), // Default fallback
                            }
                        } else {
                            std::path::PathBuf::from("/tmp") // Default fallback
                        }
                    },
                    _ => std::path::PathBuf::from("/tmp"), // Default fallback
                };

                let table_node = TableViewerNode::new_with_cache_dir(
                    serializable_node.id,
                    serializable_node.name,
                    cache_dir,
                );
                table_nodes.insert(serializable_node.id, table_node);
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
            json_nodes,
            map_nodes,
            table_nodes,
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

    /// Add a JSON node directly to the workspace
    pub fn add_json_node(&mut self, id: NodeId, json_node: JsonSourceNode) {
        let node_type = json_node.node_type();

        // Add to JSON nodes collection
        self.json_nodes.insert(id, json_node);

        // Add to view
        self.view.add_node_view(id, node_type, (0, 0));
    }

    /// Add a Map node directly to the workspace
    pub fn add_map_node(&mut self, id: NodeId, map_node: MapNode) {
        let node_type = map_node.node_type();

        // Add to Map nodes collection
        self.map_nodes.insert(id, map_node);

        // Add to view
        self.view.add_node_view(id, node_type, (0, 0));
    }

    /// Add a Table node directly to the workspace
    pub fn add_table_node(&mut self, id: NodeId, table_node: TableViewerNode) {
        let node_type = table_node.node_type();

        // Add to Table nodes collection
        self.table_nodes.insert(id, table_node);

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
                } else if let Some(json_node) = self.json_nodes.get_mut(&link.from_node) {
                    json_node.execute(&HashMap::new())?
                } else if let Some(map_node) = self.map_nodes.get_mut(&link.from_node) {
                    map_node.execute(&HashMap::new())?
                } else if let Some(table_node) = self.table_nodes.get_mut(&link.from_node) {
                    table_node.execute(&HashMap::new())?
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
        } else if let Some(json_node) = self.json_nodes.get_mut(&node_id) {
            json_node.execute(&inputs)
        } else if let Some(map_node) = self.map_nodes.get_mut(&node_id) {
            map_node.execute(&inputs)
        } else if let Some(table_node) = self.table_nodes.get_mut(&node_id) {
            table_node.execute(&inputs)
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
        } else if let Some(json_node) = self.json_nodes.get(&id) {
            Some(json_node as &dyn Node)
        } else if let Some(map_node) = self.map_nodes.get(&id) {
            Some(map_node as &dyn Node)
        } else if let Some(table_node) = self.table_nodes.get(&id) {
            Some(table_node as &dyn Node)
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

        // Add JSON nodes
        for (id, json_node) in &self.json_nodes {
            nodes.push((*id, json_node as &dyn Node));
        }

        // Add Map nodes
        for (id, map_node) in &self.map_nodes {
            nodes.push((*id, map_node as &dyn Node));
        }

        // Add Table nodes
        for (id, table_node) in &self.table_nodes {
            nodes.push((*id, table_node as &dyn Node));
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
