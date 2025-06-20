use crate::{
    plugin::{NodeLoadData, NodeLoadResult, NodeSaveData, NodeSaveResult},
    value::Value,
    Result,
};
use std::{collections::HashMap, future::Future};

pub trait Node: Send + Sync + std::fmt::Debug {
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

    /// Allow downcasting to concrete types for type-specific operations
    /// TODO: Remove this ASAP - bad implementation. Type-specific setup should be handled
    /// through proper trait methods or configuration, not downcasting.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

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
    Data, /* Main data flow - circles
           * Config,   // TODO: Configuration parameters - squares
           * Control,  // TODO: Triggers/events - diamonds */
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        plugin::{NodeLoadData, NodeSaveData},
        value::Value,
    };
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

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
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
}
