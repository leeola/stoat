use crate::{value::Value, Result};
use std::{collections::HashMap, fmt::Debug, future::Future, path::PathBuf, sync::LazyLock};

/// Initialization data for node creation and persistence
///
/// This trait defines how nodes can be created from serializable initialization data.
/// Each node type implements this to provide clean, type-safe node construction.
///
/// See also: [`crate::node::Node::to_init`]
pub trait NodeInit: Send + Sync + Debug {
    /// Create a boxed node from this initialization data
    ///
    /// This method constructs a fully functional node instance using the provided
    /// initialization parameters along with the stored configuration data.
    fn init(&self, id: NodeId, name: String, config: Value) -> Result<Box<dyn Node>>;

    /// Get the node type identifier for registry lookup
    ///
    /// This should return a stable string identifier that uniquely identifies
    /// this node type across serialization/deserialization cycles.
    fn name(&self) -> &'static str;
}

pub trait Node: Send + Sync + Debug {
    fn id(&self) -> NodeId;
    fn node_type(&self) -> NodeType;
    fn name(&self) -> &str;

    /// Execute this node with the given inputs, returning output values by port name
    fn execute(&mut self, inputs: &HashMap<String, Value>) -> Result<HashMap<String, Value>>;

    /// Get input port definitions
    fn input_ports(&self) -> Vec<Port>;

    /// Get output port definitions  
    fn output_ports(&self) -> Vec<Port>;

    /// Get socket configuration for this node
    fn sockets(&self) -> NodeSockets;

    /// Get presentation type for this node
    fn presentation(&self) -> NodePresentation;

    /// Current node status
    fn status(&self) -> NodeStatus;

    /// Whether node can execute (Ready or Idle)
    fn can_execute(&self) -> bool {
        matches!(self.status(), NodeStatus::Ready | NodeStatus::Idle)
    }

    /// Whether node has an error
    fn has_error(&self) -> bool {
        matches!(self.status(), NodeStatus::Error { .. })
    }

    /// Get configuration data from config sockets
    ///
    /// Returns a map of configuration values by socket name. This replaces the old
    /// config() method and allows nodes to expose multiple configuration parameters
    /// through different sockets (e.g., API nodes could have separate sockets for
    /// headers, request body, etc.).
    ///
    /// Returns empty map for nodes with no configuration sockets.
    fn get_config_values(&self) -> HashMap<String, Value> {
        HashMap::new()
    }

    /// Save node state to disk
    ///
    /// Default implementation returns an error indicating persistence is not supported.
    /// Nodes that support persistence should override this method.
    ///
    /// See also: [`Node::load_state`]
    fn save_state(
        &self,
        _save_data: NodeSaveData,
    ) -> Box<dyn Future<Output = Result<NodeSaveResult>> + Send + '_> {
        Box::new(std::future::ready(Err(crate::error::Error::Unsupported {
            operation: "Node persistence".to_string(),
            reason: format!(
                "Node type {} does not support save/load operations",
                self.node_type()
            ),
        })))
    }

    /// Load node state from disk
    ///
    /// Default implementation returns an error indicating persistence is not supported.
    /// Nodes that support persistence should override this method.
    ///
    /// See also: [`Node::save_state`]
    fn load_state(
        &mut self,
        _load_data: NodeLoadData,
    ) -> Box<dyn Future<Output = Result<NodeLoadResult>> + Send + '_> {
        Box::new(std::future::ready(Err(crate::error::Error::Unsupported {
            operation: "Node persistence".to_string(),
            reason: format!(
                "Node type {} does not support save/load operations",
                self.node_type()
            ),
        })))
    }
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeId(pub u64);

#[derive(Debug, Clone)]
pub struct Port {
    pub name: String,
    pub description: String,
}

impl Port {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

/// Data structure for saving node state to disk
#[derive(Debug, Clone)]
pub struct NodeSaveData {
    /// Directory where node data should be saved
    pub save_dir: PathBuf,
    /// ID of the node being saved
    pub node_id: NodeId,
    /// Current node data/state
    pub node_data: Value,
    /// Optional metadata for the save operation
    pub metadata: Option<Value>,
}

/// Data structure for loading node state from disk
#[derive(Debug, Clone)]
pub struct NodeLoadData {
    /// Directory where node data should be loaded from
    pub load_dir: PathBuf,
    /// ID of the node being loaded
    pub node_id: NodeId,
    /// Optional metadata for the load operation
    pub metadata: Option<Value>,
}

/// Result of a node save operation
#[derive(Debug, Clone)]
pub struct NodeSaveResult {
    /// Whether the save operation was successful
    pub success: bool,
    /// Optional message about the save operation
    pub message: Option<String>,
}

/// Result of a node load operation
#[derive(Debug, Clone)]
pub struct NodeLoadResult {
    /// Whether the load operation was successful
    pub success: bool,
    /// Optional message about the load operation
    pub message: Option<String>,
    /// Loaded data, if any
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeType {
    #[cfg(feature = "csv")]
    CsvSource,
    #[cfg(feature = "json")]
    JsonSource,
    Map,
    TableViewer,
}

impl std::fmt::Display for NodeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            #[cfg(feature = "csv")]
            NodeType::CsvSource => "csv",
            #[cfg(feature = "json")]
            NodeType::JsonSource => "json",
            NodeType::Map => "map",
            NodeType::TableViewer => "table",
        };
        write!(f, "{}", s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketType {
    /// Main data flow - circles
    Data,
    /// Configuration parameters - squares
    Config,
    // Control,  // TODO: Triggers/events - diamonds
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketInfo {
    pub socket_type: SocketType,
    pub name: String,
    pub required: bool,
}

impl SocketInfo {
    pub fn new(socket_type: SocketType, name: impl Into<String>, required: bool) -> Self {
        Self {
            socket_type,
            name: name.into(),
            required,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSockets {
    pub inputs: Vec<SocketInfo>,
    pub outputs: Vec<SocketInfo>,
}

impl NodeSockets {
    pub fn new(inputs: Vec<SocketInfo>, outputs: Vec<SocketInfo>) -> Self {
        Self { inputs, outputs }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodePresentation {
    Minimal,
    // ConfigPanel,    // TODO: Future
    // TextEditor,     // TODO: Future
    // ImageViewer,    // TODO: Future
    TableViewer,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NodeStatus {
    /// Node is ready to execute
    Ready,
    /// Node currently executing
    Running,
    /// Node completed successfully, idle
    Idle,
    /// Node has an error with user-facing message
    Error {
        message: String,
        error_type: ErrorType,
        recoverable: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ErrorType {
    /// Missing or invalid configuration
    Configuration,
    /// Runtime execution error
    Execution,
    /// I/O or resource error
    Resource,
    /// Dependency/connection error
    Dependency,
    /// Internal node error
    Internal,
}

/// Registry for node initialization implementations
///
/// Provides a centralized factory for creating nodes from type names and configuration.
/// Thread-safe singleton that manages all available node types.
///
/// See also: [`NodeInit`]
pub struct NodeInitRegistry {
    initializers: HashMap<&'static str, Box<dyn NodeInit>>,
}

impl NodeInitRegistry {
    /// Create a new empty registry
    fn new() -> Self {
        Self {
            initializers: HashMap::new(),
        }
    }

    /// Register a node init implementation
    ///
    /// This method takes ownership of the NodeInit implementation and stores it
    /// in the registry under the provided name.
    pub fn register(&mut self, init: Box<dyn NodeInit>) {
        let name = init.name();
        self.initializers.insert(name, init);
    }

    /// Create a node from type name and configuration
    ///
    /// Returns a boxed node instance if the type is registered, otherwise returns
    /// an error indicating the node type is unknown.
    pub fn create_node(
        &self,
        node_type: &str,
        id: NodeId,
        name: String,
        config: Value,
    ) -> Result<Box<dyn Node>> {
        self.initializers
            .get(node_type)
            .ok_or_else(|| crate::Error::Generic {
                message: format!("Unknown node type: {}", node_type),
            })?
            .init(id, name, config)
    }

    /// Get all registered node type names
    pub fn registered_types(&self) -> Vec<&'static str> {
        self.initializers.keys().copied().collect()
    }

    /// Check if a node type is registered
    pub fn is_registered(&self, node_type: &str) -> bool {
        self.initializers.contains_key(node_type)
    }
}

/// Global node initialization registry
///
/// This singleton provides access to all registered node types. It's initialized
/// with all built-in node types when first accessed.
///
/// See also: [`NodeInit`], [`NodeInitRegistry`]
pub static NODE_INIT_REGISTRY: LazyLock<std::sync::Mutex<NodeInitRegistry>> = LazyLock::new(|| {
    let mut registry = NodeInitRegistry::new();

    // Register built-in node types
    #[cfg(feature = "csv")]
    registry.register(Box::new(crate::nodes::csv::CsvInit));

    #[cfg(feature = "json")]
    registry.register(Box::new(crate::nodes::json::JsonInit));

    registry.register(Box::new(crate::nodes::map::MapInit));
    registry.register(Box::new(crate::nodes::table::TableInit));

    std::sync::Mutex::new(registry)
});

/// Create a node using the global registry
///
/// Convenience function for creating nodes without directly accessing the global registry.
///
/// See also: [`NODE_INIT_REGISTRY`]
pub fn create_node_from_registry(
    node_type: &str,
    id: NodeId,
    name: String,
    config: Value,
) -> Result<Box<dyn Node>> {
    NODE_INIT_REGISTRY
        .lock()
        .map_err(|_| crate::Error::Generic {
            message: "Failed to acquire registry lock".to_string(),
        })?
        .create_node(node_type, id, name, config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;
    use std::{collections::HashMap, path::PathBuf};

    #[derive(Debug)]
    struct MockNode {
        id: NodeId,
        name: String,
    }

    impl Node for MockNode {
        fn id(&self) -> NodeId {
            self.id
        }

        fn node_type(&self) -> NodeType {
            NodeType::Map
        }

        fn name(&self) -> &str {
            &self.name
        }

        fn execute(
            &mut self,
            _inputs: &HashMap<String, Value>,
        ) -> crate::Result<HashMap<String, Value>> {
            Ok(HashMap::new())
        }

        fn input_ports(&self) -> Vec<Port> {
            vec![]
        }

        fn output_ports(&self) -> Vec<Port> {
            vec![]
        }

        fn sockets(&self) -> NodeSockets {
            NodeSockets::new(vec![], vec![])
        }

        fn presentation(&self) -> NodePresentation {
            NodePresentation::Minimal
        }

        fn status(&self) -> NodeStatus {
            NodeStatus::Idle
        }
    }

    #[test]
    fn registry_basic_functionality() {
        // Test that we can create nodes using the global registry

        // Only test CSV if the feature is enabled
        #[cfg(feature = "csv")]
        {
            let csv_config = Value::String(compact_str::CompactString::from("test.csv"));
            let result =
                create_node_from_registry("csv", NodeId(1), "test_csv".to_string(), csv_config);
            assert!(result.is_ok());
        }

        let table_config = Value::Empty;
        let result =
            create_node_from_registry("table", NodeId(2), "test_table".to_string(), table_config);
        assert!(result.is_ok());

        // Test unknown node type
        let result =
            create_node_from_registry("unknown", NodeId(3), "test".to_string(), Value::Empty);
        assert!(result.is_err());
    }

    #[test]
    fn registry_registered_types() {
        let registry = NODE_INIT_REGISTRY.lock().unwrap();
        let types = registry.registered_types();

        // Should contain at least the built-in node types
        assert!(types.contains(&"table"));
        assert!(types.contains(&"map"));

        #[cfg(feature = "csv")]
        assert!(types.contains(&"csv"));

        #[cfg(feature = "json")]
        assert!(types.contains(&"json"));
    }

    #[test]
    fn registry_is_registered() {
        let registry = NODE_INIT_REGISTRY.lock().unwrap();

        assert!(registry.is_registered("table"));
        assert!(registry.is_registered("map"));
        assert!(!registry.is_registered("nonexistent"));

        #[cfg(feature = "csv")]
        assert!(registry.is_registered("csv"));

        #[cfg(feature = "json")]
        assert!(registry.is_registered("json"));
    }

    #[test]
    fn node_save_load_trait_exists() {
        let node1 = MockNode {
            id: NodeId(1),
            name: "test_node".to_string(),
        };

        let mut node2 = MockNode {
            id: NodeId(2),
            name: "test_node_2".to_string(),
        };

        let save_data = NodeSaveData {
            save_dir: PathBuf::from("/tmp"),
            node_id: NodeId(1),
            node_data: Value::Empty,
            metadata: None,
        };

        let load_data = NodeLoadData {
            load_dir: PathBuf::from("/tmp"),
            node_id: NodeId(2),
            metadata: None,
        };

        let _save_future = node1.save_state(save_data);
        let _load_future = node2.load_state(load_data);
    }

    #[test]
    fn node_init_serialization_roundtrip() {
        use crate::workspace::{SerializableWorkspace, Workspace};

        // Create a workspace with a test node
        let mut workspace = Workspace::new();

        // Add a table node directly
        let table_node = crate::nodes::table::TableViewerNode::new_with_cache_dir(
            NodeId(1),
            "test_table".to_string(),
            std::path::PathBuf::from("/tmp"),
        );
        workspace.add_table_node(NodeId(1), table_node);

        // Serialize the workspace
        let serializable = SerializableWorkspace::from(&workspace);
        assert_eq!(serializable.nodes.len(), 1);
        assert_eq!(serializable.nodes[0].node_type, "table");

        // Reconstruct workspace from serializable (this tests the registry)
        let reconstructed = Workspace::from_serializable(serializable);

        // Verify the node was reconstructed
        let nodes = reconstructed.list_nodes();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].1.node_type().to_string(), "table");
        assert_eq!(nodes[0].1.name(), "test_table");
    }
}
