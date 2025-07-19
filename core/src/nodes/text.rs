use crate::{
    node::{Node, NodeId, NodeInit, NodePresentation, NodeSockets, NodeStatus, NodeType, Port},
    value::Value,
    Result,
};
use std::collections::HashMap;

/// A simple text node that stores and displays text content
#[derive(Debug)]
pub struct TextNode {
    id: NodeId,
    name: String,
    content: String,
}

impl TextNode {
    pub fn new(id: NodeId, name: String, content: String) -> Self {
        Self { id, name, content }
    }

    /// Get the text content
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Set the text content
    pub fn set_content(&mut self, content: String) {
        self.content = content;
    }
}

impl Node for TextNode {
    fn id(&self) -> NodeId {
        self.id
    }

    fn node_type(&self) -> NodeType {
        NodeType::Text
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn execute(&mut self, _inputs: &HashMap<String, Value>) -> Result<HashMap<String, Value>> {
        // Text nodes don't process inputs, they just display content
        let mut outputs = HashMap::new();
        outputs.insert(
            "text".to_string(),
            Value::String(self.content.clone().into()),
        );
        Ok(outputs)
    }

    fn input_ports(&self) -> Vec<Port> {
        // No inputs for now
        vec![]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("text", "The text content")]
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

    fn get_config_values(&self) -> HashMap<String, Value> {
        let mut config = HashMap::new();
        config.insert(
            "content".to_string(),
            Value::String(self.content.clone().into()),
        );
        config
    }
}

/// Initialization struct for creating TextNode instances
#[derive(Debug)]
pub struct TextNodeInit;

impl NodeInit for TextNodeInit {
    fn init(&self, id: NodeId, name: String, config: Value) -> Result<Box<dyn Node>> {
        // Extract content from config
        let content = match config {
            Value::Map(map) => map
                .0
                .get("content")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.to_string()),
                    _ => None,
                })
                .unwrap_or_default(),
            Value::String(s) => s.to_string(),
            _ => String::new(),
        };

        Ok(Box::new(TextNode::new(id, name, content)))
    }

    fn name(&self) -> &'static str {
        "text"
    }
}
