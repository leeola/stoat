use crate::{
    node::{Node, NodeId, NodeInit, NodePresentation, NodeSockets, NodeStatus, NodeType, Port},
    value::Value,
    Result,
};
use std::collections::HashMap;

/// A node representing a user message from conversation history
///
/// These nodes are created automatically when users submit messages in the
/// agentic chat interface. They form the backbone of the conversation graph,
/// with each user message becoming a node that can later be linked to the
/// resources and actions the agent performed in response.
#[derive(Debug)]
pub struct UserMessageNode {
    id: NodeId,
    name: String,
    message_content: String,
    timestamp: std::time::SystemTime,
}

impl UserMessageNode {
    pub fn new(id: NodeId, name: String, message_content: String) -> Self {
        Self {
            id,
            name,
            message_content,
            timestamp: std::time::SystemTime::now(),
        }
    }

    /// Get the message content
    pub fn content(&self) -> &str {
        &self.message_content
    }

    /// Get the timestamp when this message was created
    pub fn timestamp(&self) -> std::time::SystemTime {
        self.timestamp
    }

    /// Get a truncated version of the message for display
    pub fn truncated_content(&self, max_len: usize) -> String {
        if self.message_content.len() <= max_len {
            self.message_content.clone()
        } else {
            format!("{}...", &self.message_content[..max_len])
        }
    }
}

impl Node for UserMessageNode {
    fn id(&self) -> NodeId {
        self.id
    }

    fn node_type(&self) -> NodeType {
        NodeType::UserMessage
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn execute(&mut self, _inputs: &HashMap<String, Value>) -> Result<HashMap<String, Value>> {
        // User message nodes output their content when executed
        let mut outputs = HashMap::new();
        outputs.insert(
            "message".to_string(),
            Value::String(self.message_content.clone().into()),
        );
        outputs.insert(
            "timestamp".to_string(),
            Value::String(format!("{:?}", self.timestamp).into()),
        );
        Ok(outputs)
    }

    fn input_ports(&self) -> Vec<Port> {
        // No inputs for user message nodes
        vec![]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![
            Port::new("message", "The user's message content"),
            Port::new("timestamp", "When the message was sent"),
        ]
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
            "message_content".to_string(),
            Value::String(self.message_content.clone().into()),
        );
        config.insert(
            "timestamp".to_string(),
            Value::String(format!("{:?}", self.timestamp).into()),
        );
        config
    }
}

/// Initialization struct for creating UserMessageNode instances
#[derive(Debug)]
pub struct UserMessageNodeInit;

impl NodeInit for UserMessageNodeInit {
    fn init(&self, id: NodeId, name: String, config: Value) -> Result<Box<dyn Node>> {
        // Extract message content from config
        let message_content = match config {
            Value::Map(map) => map
                .0
                .get("message_content")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.to_string()),
                    _ => None,
                })
                .unwrap_or_default(),
            Value::String(s) => s.to_string(),
            _ => String::new(),
        };

        Ok(Box::new(UserMessageNode::new(id, name, message_content)))
    }

    fn name(&self) -> &'static str {
        "user_message"
    }
}
