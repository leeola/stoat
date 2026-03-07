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

    pub fn get_thinking_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| c.as_thinking())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn has_thinking(&self) -> bool {
        self.content.iter().any(|c| c.is_thinking())
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
///
/// Uses custom serialize/deserialize to gracefully handle unknown content block
/// types from the Claude API without failing deserialization.
#[derive(Debug, Clone)]
pub enum MessageContent {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: HashMap<String, serde_json::Value>,
    },
    Thinking {
        thinking: String,
    },
    RedactedThinking,
    ServerToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ServerToolResult {
        tool_use_id: String,
        content: serde_json::Value,
    },
    Unknown {
        content_type: String,
    },
}

impl Serialize for MessageContent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        match self {
            MessageContent::Text { text } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "text")?;
                map.serialize_entry("text", text)?;
                map.end()
            },
            MessageContent::ToolUse { id, name, input } => {
                let mut map = serializer.serialize_map(Some(4))?;
                map.serialize_entry("type", "tool_use")?;
                map.serialize_entry("id", id)?;
                map.serialize_entry("name", name)?;
                map.serialize_entry("input", input)?;
                map.end()
            },
            MessageContent::Thinking { thinking } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "thinking")?;
                map.serialize_entry("thinking", thinking)?;
                map.end()
            },
            MessageContent::RedactedThinking => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("type", "redacted_thinking")?;
                map.end()
            },
            MessageContent::ServerToolUse { id, name, input } => {
                let mut map = serializer.serialize_map(Some(4))?;
                map.serialize_entry("type", "server_tool_use")?;
                map.serialize_entry("id", id)?;
                map.serialize_entry("name", name)?;
                map.serialize_entry("input", input)?;
                map.end()
            },
            MessageContent::ServerToolResult {
                tool_use_id,
                content,
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "server_tool_result")?;
                map.serialize_entry("tool_use_id", tool_use_id)?;
                map.serialize_entry("content", content)?;
                map.end()
            },
            MessageContent::Unknown { content_type } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("type", content_type)?;
                map.end()
            },
        }
    }
}

impl<'de> Deserialize<'de> for MessageContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let content_type = value
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        match content_type.as_str() {
            "text" => {
                let text = value
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(MessageContent::Text { text })
            },
            "tool_use" => {
                let id = value
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = value
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input: HashMap<String, serde_json::Value> = value
                    .get("input")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                Ok(MessageContent::ToolUse { id, name, input })
            },
            "thinking" => {
                let thinking = value
                    .get("thinking")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(MessageContent::Thinking { thinking })
            },
            "redacted_thinking" => Ok(MessageContent::RedactedThinking),
            "server_tool_use" => {
                let id = value
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = value
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = value
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                Ok(MessageContent::ServerToolUse { id, name, input })
            },
            "server_tool_result" => {
                let tool_use_id = value
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let content = value
                    .get("content")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                Ok(MessageContent::ServerToolResult {
                    tool_use_id,
                    content,
                })
            },
            _ => Ok(MessageContent::Unknown { content_type }),
        }
    }
}

impl MessageContent {
    pub fn is_text(&self) -> bool {
        matches!(self, MessageContent::Text { .. })
    }

    pub fn is_tool_use(&self) -> bool {
        matches!(self, MessageContent::ToolUse { .. })
    }

    pub fn is_thinking(&self) -> bool {
        matches!(self, MessageContent::Thinking { .. })
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            MessageContent::Text { text } => Some(text.as_str()),
            _ => None,
        }
    }

    pub fn as_thinking(&self) -> Option<&str> {
        match self {
            MessageContent::Thinking { thinking } => Some(thinking.as_str()),
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
    fn assistant_thinking_helpers() {
        let msg = AssistantMessage {
            role: Role::Assistant,
            content: vec![
                MessageContent::Thinking {
                    thinking: "Let me reason...".to_string(),
                },
                MessageContent::Text {
                    text: "Here's my answer.".to_string(),
                },
            ],
        };

        assert!(msg.has_thinking());
        assert_eq!(msg.get_thinking_content(), "Let me reason...");
        assert_eq!(msg.get_text_content(), "Here's my answer.");
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

    #[test]
    fn deserialize_text_content() {
        let json = r#"{"type": "text", "text": "hello"}"#;
        let content: MessageContent = serde_json::from_str(json).unwrap();
        assert_eq!(content.as_text(), Some("hello"));
    }

    #[test]
    fn deserialize_tool_use_content() {
        let json = r#"{"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "/foo"}}"#;
        let content: MessageContent = serde_json::from_str(json).unwrap();
        assert!(content.is_tool_use());
        match &content {
            MessageContent::ToolUse { id, name, input } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "Read");
                assert_eq!(input.get("path").unwrap(), "/foo");
            },
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn deserialize_thinking_content() {
        let json = r#"{"type": "thinking", "thinking": "hmm..."}"#;
        let content: MessageContent = serde_json::from_str(json).unwrap();
        assert_eq!(content.as_thinking(), Some("hmm..."));
    }

    #[test]
    fn deserialize_redacted_thinking() {
        let json = r#"{"type": "redacted_thinking"}"#;
        let content: MessageContent = serde_json::from_str(json).unwrap();
        assert!(matches!(content, MessageContent::RedactedThinking));
    }

    #[test]
    fn deserialize_server_tool_use() {
        let json = r#"{"type": "server_tool_use", "id": "s1", "name": "web_search", "input": {"query": "rust"}}"#;
        let content: MessageContent = serde_json::from_str(json).unwrap();
        match &content {
            MessageContent::ServerToolUse { id, name, .. } => {
                assert_eq!(id, "s1");
                assert_eq!(name, "web_search");
            },
            _ => panic!("expected ServerToolUse"),
        }
    }

    #[test]
    fn deserialize_unknown_content_type() {
        let json = r#"{"type": "future_thing", "data": 42}"#;
        let content: MessageContent = serde_json::from_str(json).unwrap();
        match &content {
            MessageContent::Unknown { content_type } => {
                assert_eq!(content_type, "future_thing");
            },
            _ => panic!("expected Unknown"),
        }
    }

    #[test]
    fn deserialize_assistant_with_mixed_content() {
        let json = r#"{
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "reasoning..."},
                    {"type": "text", "text": "answer"},
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}},
                    {"type": "weird_new_type", "foo": "bar"}
                ]
            },
            "session_id": "s1"
        }"#;
        let msg: SdkMessage = serde_json::from_str(json).unwrap();
        match &msg {
            SdkMessage::Assistant { message, .. } => {
                assert_eq!(message.content.len(), 4);
                assert!(message.has_thinking());
                assert!(message.has_tool_uses());
                assert_eq!(message.get_text_content(), "answer");
                assert!(matches!(
                    &message.content[3],
                    MessageContent::Unknown { content_type } if content_type == "weird_new_type"
                ));
            },
            _ => panic!("expected Assistant"),
        }
    }
}
