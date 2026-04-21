//! Assistant/user message bodies and content blocks.
//!
//! Groups [`AssistantMessage`], [`UserMessage`], and the content-block
//! families that flow through them ([`MessageContent`] for assistant
//! blocks, [`UserContentBlock`] for user blocks including tool results).

use crate::messages::{
    result::{StopReason, Usage},
    system::Role,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Assistant message containing Claude's response.
///
/// Assistant messages can contain multiple content blocks, mixing
/// text responses with tool invocations. The content array order
/// is significant and represents Claude's intended execution sequence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// Role identifier (always "assistant")
    pub role: Role,
    /// Ordered array of content blocks
    pub content: Vec<MessageContent>,
    /// Model id the API attributed this turn to. Not always present on
    /// older CLI releases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Token usage for this turn, when reported by the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Message id from the Anthropic API, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Reason the turn ended, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// Stop sequence that triggered end-of-turn, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
}

impl AssistantMessage {
    /// Create a text-only assistant message.
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![MessageContent::Text { text: text.into() }],
            model: None,
            usage: None,
            id: None,
            stop_reason: None,
            stop_sequence: None,
        }
    }

    /// Extract all text content, concatenated with newlines.
    ///
    /// Ignores tool use blocks, returning only human-readable text.
    /// Useful for displaying responses to users who don't need to
    /// see tool invocation details.
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

    /// Extract all tool use blocks for execution.
    ///
    /// Returns references to all ToolUse content blocks in order.
    /// The runtime should execute these and inject results as
    /// User messages before Claude continues.
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

    /// Check if this message contains only tool uses (no text).
    ///
    /// Pure tool messages are common when Claude needs to gather
    /// information before providing a text response.
    pub fn is_tool_only(&self) -> bool {
        !self.content.is_empty()
            && self
                .content
                .iter()
                .all(|c| matches!(c, MessageContent::ToolUse { .. }))
    }

    /// Check if this message contains any tool uses.
    pub fn has_tool_uses(&self) -> bool {
        self.content
            .iter()
            .any(|c| matches!(c, MessageContent::ToolUse { .. }))
    }
}

/// Tool use information extracted from assistant messages.
#[derive(Debug, Clone)]
pub struct ToolUse {
    /// Unique identifier for this tool invocation
    pub id: String,
    /// Name of the tool to execute
    pub name: String,
    /// Tool-specific input parameters
    pub input: HashMap<String, serde_json::Value>,
}

/// User message containing input to Claude.
///
/// User messages have two distinct formats depending on origin:
/// - Human input: Simple text string
/// - Tool results: Array of content blocks with results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    /// Role identifier (always "user")
    pub role: Role,
    /// Message content (text or structured blocks)
    #[serde(with = "user_content_serde")]
    pub content: UserContent,
}

impl UserMessage {
    /// Create a simple text message from the user.
    ///
    /// Use this for human-generated prompts.
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: UserContent::Text(text.into()),
        }
    }

    /// Create a tool result message.
    ///
    /// Use this for automatic tool result injection after
    /// assistant tool use messages.
    pub fn from_tool_result(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: UserContent::Blocks(vec![UserContentBlock::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: ToolResultContent::Text(content.into()),
                is_error: None,
            }]),
        }
    }

    /// Try to extract text content if this is a simple text message.
    ///
    /// Returns None for tool result messages.
    pub fn as_text(&self) -> Option<&str> {
        match &self.content {
            UserContent::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Check if this is a tool result message.
    pub fn is_tool_result(&self) -> bool {
        matches!(&self.content, UserContent::Blocks(_))
    }
}

/// Content format for user messages.
///
/// The format depends on the message origin:
/// - Text: Human-generated prompts
/// - Blocks: Tool results and structured content
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    /// Simple text prompt from human user
    Text(String),
    /// Structured content blocks (typically tool results)
    Blocks(Vec<UserContentBlock>),
}

impl UserContent {
    /// Extract as plain text if possible.
    ///
    /// For block content, concatenates all text blocks and
    /// tool results into a single string.
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
                        ..
                    } => format!("[Tool Result {tool_use_id}]: {}", content.as_text()),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// Structured content block within user messages.
///
/// These blocks appear in tool result messages to provide
/// execution results back to Claude.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum UserContentBlock {
    /// Text content block
    #[serde(rename = "text")]
    Text {
        /// Text content
        text: String,
    },

    /// Tool execution result block.
    ///
    /// Generated automatically by the runtime after executing
    /// a tool that Claude requested via `MessageContent::ToolUse`.
    #[serde(rename = "tool_result")]
    ToolResult {
        /// ID matching the original ToolUse request
        tool_use_id: String,
        /// Execution result. Wire format accepts either a plain string or
        /// a structured array of content blocks; [`ToolResultContent`]
        /// preserves both and exposes them as one string on demand.
        content: ToolResultContent,
        /// Whether the tool execution resulted in an error. Optional per
        /// the Anthropic API schema; absent implies `false`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

/// Body of a [`UserContentBlock::ToolResult`]. The wire format accepts
/// both a plain string and an array of content blocks; this enum
/// preserves the shape rather than flattening on deserialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<serde_json::Value>),
}

impl ToolResultContent {
    /// Collapse to a single displayable string. Plain text passes
    /// through unchanged; structured blocks are serialized to compact
    /// JSON so callers can still surface their content as a string.
    pub fn as_text(&self) -> String {
        match self {
            ToolResultContent::Text(s) => s.clone(),
            ToolResultContent::Blocks(blocks) => serde_json::to_string(blocks).unwrap_or_default(),
        }
    }
}

// Custom serialization to handle both string and array formats for UserContent
mod user_content_serde {
    use super::{UserContent, UserContentBlock};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

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
/// Assistant messages contain an ordered array of these blocks,
/// which can be text responses, tool invocations, or blocks whose
/// `type` tag is unrecognized by this crate. The order is significant
/// and represents Claude's intended sequence.
///
/// Deserialization is schema-loose: any content block whose `type` tag
/// does not match a known variant (or whose fields fail to parse for
/// a known tag) is captured as [`MessageContent::Unknown`] carrying the
/// raw JSON, so consumers can surface or persist unrecognized blocks
/// without the entire assistant message being dropped.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContent {
    /// Text response content.
    ///
    /// Human-readable text that should be displayed to the user.
    /// May contain markdown formatting.
    Text {
        /// Text content (may include markdown)
        text: String,
    },

    /// Tool invocation request.
    ///
    /// Indicates Claude wants to execute a tool. The runtime should:
    /// 1. Execute the specified tool with provided input
    /// 2. Inject a `User(ToolResult)` message with results
    /// 3. Continue processing Claude's response
    ///
    /// # Example
    /// ```json
    /// {
    ///   "type": "tool_use",
    ///   "id": "toolu_abc123",
    ///   "name": "Read",
    ///   "input": {"file_path": "/tmp/data.txt"}
    /// }
    /// ```
    ToolUse {
        /// Unique identifier for this tool invocation.
        /// Must be included in the corresponding ToolResult.
        id: String,
        /// Name of the tool to execute
        name: String,
        /// Tool-specific input parameters as JSON object
        input: HashMap<String, serde_json::Value>,
    },

    /// Extended thinking block. Emitted when extended thinking is
    /// enabled; `signature` preserves the cryptographic integrity tag
    /// the API attaches to each thinking block.
    Thinking { thinking: String, signature: String },

    /// Redacted thinking block. Carries only the encrypted `data`
    /// field; the plaintext thinking is withheld by the API.
    RedactedThinking { data: String },

    /// Server-side tool invocation (e.g. `web_search`, `code_execution`).
    /// Same shape as [`ToolUse`](MessageContent::ToolUse) but executed
    /// server-side and paired with a [`ServerToolResult`] rather than a
    /// client-injected tool result.
    ServerToolUse {
        id: String,
        name: String,
        input: HashMap<String, serde_json::Value>,
    },

    /// Result of a server-side tool invocation.
    ServerToolResult {
        tool_use_id: String,
        content: serde_json::Value,
    },

    /// Image content block. The `source` sub-object is retained as raw
    /// JSON because the Anthropic API supports several encodings
    /// (`base64`, `url`, `file`) whose shape has evolved over time.
    /// Callers that need structured access should destructure the
    /// underlying `source.type` discriminant themselves.
    Image { source: serde_json::Value },

    /// Content block with an unrecognized `type` tag, or a recognized tag
    /// whose fields did not match the expected shape. Carries the raw
    /// JSON so the information is preserved rather than silently dropped.
    #[serde(skip)]
    Unknown(serde_json::Value),
}

impl<'de> Deserialize<'de> for MessageContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Helper enum mirrors the known-variant shape. Falling through to
        // `Unknown` preserves the original JSON for any block we cannot
        // parse (new content types, malformed known types).
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Known {
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
                signature: String,
            },
            RedactedThinking {
                data: String,
            },
            ServerToolUse {
                id: String,
                name: String,
                input: HashMap<String, serde_json::Value>,
            },
            ServerToolResult {
                tool_use_id: String,
                content: serde_json::Value,
            },
            Image {
                source: serde_json::Value,
            },
        }

        let value = serde_json::Value::deserialize(deserializer)?;
        match serde_json::from_value::<Known>(value.clone()) {
            Ok(Known::Text { text }) => Ok(MessageContent::Text { text }),
            Ok(Known::ToolUse { id, name, input }) => {
                Ok(MessageContent::ToolUse { id, name, input })
            },
            Ok(Known::Thinking {
                thinking,
                signature,
            }) => Ok(MessageContent::Thinking {
                thinking,
                signature,
            }),
            Ok(Known::RedactedThinking { data }) => Ok(MessageContent::RedactedThinking { data }),
            Ok(Known::ServerToolUse { id, name, input }) => {
                Ok(MessageContent::ServerToolUse { id, name, input })
            },
            Ok(Known::ServerToolResult {
                tool_use_id,
                content,
            }) => Ok(MessageContent::ServerToolResult {
                tool_use_id,
                content,
            }),
            Ok(Known::Image { source }) => Ok(MessageContent::Image { source }),
            Err(_) => Ok(MessageContent::Unknown(value)),
        }
    }
}

impl MessageContent {
    /// Check if this is a text content block.
    pub fn is_text(&self) -> bool {
        matches!(self, MessageContent::Text { .. })
    }

    /// Check if this is a tool use block.
    pub fn is_tool_use(&self) -> bool {
        matches!(self, MessageContent::ToolUse { .. })
    }

    /// Check if this is an unrecognized content block.
    pub fn is_unknown(&self) -> bool {
        matches!(self, MessageContent::Unknown(_))
    }

    /// Extract text content if this is a text block.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            MessageContent::Text { text } => Some(text.as_str()),
            _ => None,
        }
    }
}
