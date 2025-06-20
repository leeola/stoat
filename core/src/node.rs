use crate::{value::Value, Result};
use std::collections::HashMap;

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
