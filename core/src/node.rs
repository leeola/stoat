use crate::{value::Value, Result};
use std::collections::HashMap;

pub trait Node: Send + Sync {
    fn id(&self) -> NodeId;
    fn node_type(&self) -> NodeType;
    fn name(&self) -> &str;

    /// Execute this node with the given inputs, returning output values by port name
    fn execute(&mut self, inputs: &HashMap<String, Value>) -> Result<HashMap<String, Value>>;

    /// Get input port definitions
    fn input_ports(&self) -> Vec<Port>;

    /// Get output port definitions  
    fn output_ports(&self) -> Vec<Port>;
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
pub enum NodeType {
    #[cfg(feature = "csv")]
    CsvSource,
    #[cfg(feature = "json")]
    JsonSource,
}
