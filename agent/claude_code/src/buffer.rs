use crate::messages::{AssistantMessage, MessageContent, SdkMessage};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub struct MessageBuffer {
    messages: Arc<Mutex<Vec<BufferedMessage>>>,
    update_tx: broadcast::Sender<BufferUpdate>,
}

#[derive(Debug, Clone)]
pub struct BufferedMessage {
    pub id: String,
    pub session_id: String,
    pub timestamp: std::time::SystemTime,
    pub message_type: MessageType,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageType {
    User,
    Assistant,
    System,
    Result,
}

#[derive(Debug, Clone)]
pub enum BufferUpdate {
    MessageAdded(BufferedMessage),
    MessageModified { id: String, content: String },
    BufferCleared,
}

impl Default for MessageBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageBuffer {
    pub fn new() -> Self {
        let (update_tx, _) = broadcast::channel(100);
        Self {
            messages: Arc::new(Mutex::new(Vec::new())),
            update_tx,
        }
    }

    pub fn add_message(&self, msg: SdkMessage) {
        let buffered = match &msg {
            SdkMessage::User {
                message,
                session_id,
            } => {
                // Convert UserContent to string for buffering
                let content_str = match &message.content {
                    crate::messages::UserContent::Text(s) => s.clone(),
                    crate::messages::UserContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|b| match b {
                            crate::messages::UserContentBlock::Text { text } => text.clone(),
                            crate::messages::UserContentBlock::ToolResult {
                                tool_use_id,
                                content,
                            } => {
                                format!("[Tool result {tool_use_id}]: {content}")
                            },
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                BufferedMessage {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: session_id.clone(),
                    timestamp: std::time::SystemTime::now(),
                    message_type: MessageType::User,
                    content: content_str,
                }
            },

            SdkMessage::Assistant {
                message,
                session_id,
            } => BufferedMessage {
                id: uuid::Uuid::new_v4().to_string(),
                session_id: session_id.clone(),
                timestamp: std::time::SystemTime::now(),
                message_type: MessageType::Assistant,
                content: extract_text_content(message),
            },

            SdkMessage::System {
                subtype,
                cwd,
                session_id,
                ..
            } => BufferedMessage {
                id: uuid::Uuid::new_v4().to_string(),
                session_id: session_id.clone(),
                timestamp: std::time::SystemTime::now(),
                message_type: MessageType::System,
                content: format!("System {subtype:?}: Working directory: {cwd}"),
            },

            SdkMessage::Result {
                subtype,
                result,
                session_id,
                ..
            } => BufferedMessage {
                id: uuid::Uuid::new_v4().to_string(),
                session_id: session_id.clone(),
                timestamp: std::time::SystemTime::now(),
                message_type: MessageType::Result,
                content: format!(
                    "Result ({:?}): {}",
                    subtype,
                    result.as_deref().unwrap_or("No result")
                ),
            },
        };

        let mut messages = self.messages.lock().expect("Message buffer lock poisoned");
        messages.push(buffered.clone());

        // Notify subscribers
        let _ = self.update_tx.send(BufferUpdate::MessageAdded(buffered));
    }

    pub fn get_messages(&self) -> Vec<BufferedMessage> {
        self.messages
            .lock()
            .expect("Message buffer lock poisoned")
            .clone()
    }

    pub fn get_messages_by_session(&self, session_id: &str) -> Vec<BufferedMessage> {
        self.messages
            .lock()
            .expect("Message buffer lock poisoned")
            .iter()
            .filter(|m| m.session_id == session_id)
            .cloned()
            .collect()
    }

    pub fn clear(&self) {
        self.messages
            .lock()
            .expect("Message buffer lock poisoned")
            .clear();
        let _ = self.update_tx.send(BufferUpdate::BufferCleared);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BufferUpdate> {
        self.update_tx.subscribe()
    }

    pub fn get_rendered_text(&self) -> String {
        let messages = self.messages.lock().expect("Message buffer lock poisoned");
        let mut output = String::new();

        for msg in messages.iter() {
            let prefix = match msg.message_type {
                MessageType::User => "USER",
                MessageType::Assistant => "ASSISTANT",
                MessageType::System => "SYSTEM",
                MessageType::Result => "RESULT",
            };

            output.push_str(&format!("\n[{}] {}\n", prefix, msg.content));
            output.push_str("---\n");
        }

        output
    }
}

fn extract_text_content(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            MessageContent::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
