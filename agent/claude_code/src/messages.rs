//! Claude Code Stream-JSON Protocol Messages
//!
//! Defines message types for bidirectional communication between a host application
//! and the Claude CLI via newline-delimited JSON through stdin/stdout.
//!
//! # Message Lifecycle
//!
//! 1. `System(Init)` establishes session context (first message)
//! 2. `User` messages provide prompts; `Assistant` messages contain responses
//! 3. Tool usage creates `Assistant(ToolUse)` -> `User(ToolResult)` cycles
//! 4. `Result` terminates the conversation with metrics

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Root message type for the Claude Code stream-json protocol.
///
/// Uses tagged JSON with a `type` field for serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SdkMessage {
    Assistant {
        message: AssistantMessage,
        session_id: String,
    },

    User {
        message: UserMessage,
        session_id: String,
    },

    /// Terminal message ending a conversation with session metrics.
    Result {
        subtype: ResultSubtype,
        duration_ms: u64,
        duration_api_ms: u64,
        is_error: bool,
        num_turns: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        session_id: String,
        total_cost_usd: f64,
    },

    /// Initialization message sent on process startup.
    System {
        subtype: SystemSubtype,
        #[serde(rename = "apiKeySource")]
        api_key_source: ApiKeySource,
        cwd: String,
        session_id: String,
        tools: Vec<String>,
        mcp_servers: Vec<McpServer>,
        model: String,
        #[serde(rename = "permissionMode")]
        permission_mode: PermissionMode,
    },
}

impl SdkMessage {
    pub fn is_terminal(&self) -> bool {
        matches!(self, SdkMessage::Result { .. })
    }

    pub fn session_id(&self) -> &str {
        match self {
            SdkMessage::Assistant { session_id, .. }
            | SdkMessage::User { session_id, .. }
            | SdkMessage::Result { session_id, .. }
            | SdkMessage::System { session_id, .. } => session_id,
        }
    }

    pub fn message_type(&self) -> &str {
        match self {
            SdkMessage::Assistant { .. } => "assistant",
            SdkMessage::User { .. } => "user",
            SdkMessage::Result { .. } => "result",
            SdkMessage::System { .. } => "system",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResultSubtype {
    Success,
    ErrorMaxTurns,
    ErrorDuringExecution,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SystemSubtype {
    Init,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
    Plan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ApiKeySource {
    None,
    Environment,
    Config,
    Argument,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    pub status: McpServerStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpServerStatus {
    Connected,
    Disconnected,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// Assistant message containing text responses and/or tool invocations.
///
/// Content blocks are ordered and may mix text with tool uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub role: Role,
    pub content: Vec<MessageContent>,
}

impl AssistantMessage {
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![MessageContent::Text { text: text.into() }],
        }
    }

    /// Concatenates all text content blocks, ignoring tool uses.
    pub fn get_text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                MessageContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn get_tool_uses(&self) -> Vec<ToolUse> {
        self.content
            .iter()
            .filter_map(|c| match c {
                MessageContent::ToolUse { id, name, input } => Some(ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    pub fn is_tool_only(&self) -> bool {
        !self.content.is_empty()
            && self
                .content
                .iter()
                .all(|c| matches!(c, MessageContent::ToolUse { .. }))
    }

    pub fn has_tool_uses(&self) -> bool {
        self.content
            .iter()
            .any(|c| matches!(c, MessageContent::ToolUse { .. }))
    }
}

#[derive(Debug, Clone)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: HashMap<String, serde_json::Value>,
}

/// User message with two formats depending on origin:
/// - Human input: simple text string
/// - Tool results: array of content blocks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub role: Role,
    #[serde(with = "user_content_serde")]
    pub content: UserContent,
}

impl UserMessage {
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: UserContent::Text(text.into()),
        }
    }

    pub fn from_tool_result(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: UserContent::Blocks(vec![UserContentBlock::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: content.into(),
            }]),
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match &self.content {
            UserContent::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn is_tool_result(&self) -> bool {
        matches!(&self.content, UserContent::Blocks(_))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<UserContentBlock>),
}

impl UserContent {
    pub fn as_text(&self) -> String {
        match self {
            UserContent::Text(s) => s.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| match b {
                    UserContentBlock::Text { text } => text.clone(),
                    UserContentBlock::ToolResult {
                        tool_use_id,
                        content,
                    } => format!("[Tool Result {tool_use_id}]: {content}"),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum UserContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

mod user_content_serde {
    use super::*;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(content: &UserContent, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match content {
            UserContent::Text(s) => s.serialize(serializer),
            UserContent::Blocks(blocks) => blocks.serialize(serializer),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<UserContent, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Helper {
            Text(String),
            Blocks(Vec<UserContentBlock>),
        }

        let helper = Helper::deserialize(deserializer)?;
        Ok(match helper {
            Helper::Text(s) => UserContent::Text(s),
            Helper::Blocks(blocks) => UserContent::Blocks(blocks),
        })
    }
}

/// Content blocks within assistant messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContent {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: HashMap<String, serde_json::Value>,
    },
}

impl MessageContent {
    pub fn is_text(&self) -> bool {
        matches!(self, MessageContent::Text { .. })
    }

    pub fn is_tool_use(&self) -> bool {
        matches!(self, MessageContent::ToolUse { .. })
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            MessageContent::Text { text } => Some(text.as_str()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_type_detection() {
        let result = SdkMessage::Result {
            subtype: ResultSubtype::Success,
            duration_ms: 1000,
            duration_api_ms: 800,
            is_error: false,
            num_turns: 1,
            result: Some("Done".to_string()),
            session_id: "test".to_string(),
            total_cost_usd: 0.001,
        };

        assert!(result.is_terminal());
        assert_eq!(result.session_id(), "test");
        assert_eq!(result.message_type(), "result");
    }

    #[test]
    fn assistant_message_helpers() {
        let msg = AssistantMessage {
            role: Role::Assistant,
            content: vec![
                MessageContent::Text {
                    text: "Let me help.".to_string(),
                },
                MessageContent::ToolUse {
                    id: "tool_123".to_string(),
                    name: "Read".to_string(),
                    input: HashMap::new(),
                },
                MessageContent::Text {
                    text: "Done!".to_string(),
                },
            ],
        };

        assert_eq!(msg.get_text_content(), "Let me help.\nDone!");
        assert_eq!(msg.get_tool_uses().len(), 1);
        assert!(!msg.is_tool_only());
        assert!(msg.has_tool_uses());
    }

    #[test]
    fn user_message_constructors() {
        let text_msg = UserMessage::from_text("Hello");
        assert_eq!(text_msg.as_text(), Some("Hello"));
        assert!(!text_msg.is_tool_result());

        let tool_msg = UserMessage::from_tool_result("tool_123", "Success");
        assert!(tool_msg.is_tool_result());
        assert_eq!(tool_msg.as_text(), None);
    }
}
