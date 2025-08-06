//! Claude Code Stream-JSON Protocol Messages
//!
//! This module defines the message types for the Claude Code stream-json protocol,
//! which enables bidirectional communication between a host application and the Claude CLI.
//!
//! # Protocol Overview
//!
//! The stream-json protocol allows multiple message exchanges within a single Claude process
//! lifetime. Messages are newline-delimited JSON objects that flow in both directions:
//!
//! - **Inbound** (Host to Claude): User messages containing prompts or tool results
//! - **Outbound** (Claude to Host): System, Assistant, User (tool results), and Result messages
//!
//! # Message Lifecycle
//!
//! Every conversation follows a predictable lifecycle pattern:
//!
//! 1. **Initialization Phase**
//!    - `System(Init)` message establishes session context
//!    - Contains available tools, working directory, model, and permissions
//!    - Always the first message in any session
//!
//! 2. **Conversation Loop**
//!    - `User` messages provide input (human prompts)
//!    - `Assistant` messages contain Claude's responses
//!    - Tool usage creates automatic User -> Assistant -> User cycles
//!    - Multiple turns can occur until completion
//!
//! 3. **Tool Execution Cycles**
//!    - `Assistant(ToolUse)` invokes a tool
//!    - Runtime automatically injects `User(ToolResult)` with results
//!    - Assistant continues processing with tool output
//!
//! 4. **Termination**
//!    - `Result` message ends the conversation
//!    - Contains metrics: duration, cost, turn count
//!    - Indicates success or error condition
//!
//! # Example Flows
//!
//! ## Simple Text Exchange
//! ```text
//! System(Init)
//!   -> User("What is 2+2?")
//!   -> Assistant("2+2 equals 4")
//!   -> Result(Success)
//! ```
//!
//! ## Tool Usage Flow
//! ```text
//! System(Init)
//!   -> User("Create a file named test.txt")
//!   -> Assistant("I'll create that file for you")
//!   -> Assistant(ToolUse: Write)
//!   -> User(ToolResult: "File created successfully")
//!   -> Assistant("I've created test.txt successfully")
//!   -> Result(Success)
//! ```
//!
//! ## Multi-Turn with Multiple Tools
//! ```text
//! System(Init)
//!   -> User("Analyze all Python files")
//!   -> Assistant("Let me search for Python files")
//!   -> Assistant(ToolUse: Glob "**/*.py")
//!   -> User(ToolResult: ["main.py", "test.py"])
//!   -> Assistant(ToolUse: Read "main.py")
//!   -> User(ToolResult: "...file contents...")
//!   -> Assistant("I found 2 Python files. The main.py file contains...")
//!   -> Result(Success)
//! ```
//!
//! # Process Lifecycle
//!
//! The Claude process behavior depends on the mode:
//!
//! - **Interactive Mode**: Process stays alive between exchanges
//! - **Single Exchange**: Process exits after Result message
//! - **Idle Exit**: Process may exit after prolonged inactivity
//!
//! The host must handle process restarts gracefully, maintaining message
//! continuity across process boundaries when using session resumption.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Root message type for the Claude Code stream-json protocol.
///
/// All communication between the host and Claude uses this enum. Messages
/// flow bidirectionally through stdin/stdout pipes in JSON format.
///
/// # Lifecycle Roles
///
/// - [`System`](SdkMessage::System): Initialization - first message establishing session
/// - [`User`](SdkMessage::User): Input - provides prompts or tool results to Claude
/// - [`Assistant`](SdkMessage::Assistant): Output - Claude's responses and tool invocations
/// - [`Result`](SdkMessage::Result): Termination - final message with session metrics
///
/// # Serialization
///
/// Messages use tagged JSON with a `type` field:
/// ```json
/// {"type": "assistant", "message": {...}, "session_id": "..."}
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SdkMessage {
    /// Claude's response messages.
    ///
    /// # Lifecycle
    ///
    /// Assistant messages appear during the conversation phase and can:
    /// 1. Provide text responses to user prompts
    /// 2. Invoke tools (triggers automatic `User(ToolResult)`)
    /// 3. Continue processing after receiving tool results
    ///
    /// Multiple assistant messages may appear in sequence, especially
    /// when using tools or providing multi-part responses.
    ///
    /// # Example Sequence
    /// ```text
    /// User("analyze this file")
    ///   -> Assistant("Let me read that file first")  // Explanation
    ///   -> Assistant(ToolUse: Read)                   // Tool invocation
    ///   -> User(ToolResult)                           // Automatic injection
    ///   -> Assistant("The file contains...")          // Final response
    /// ```
    Assistant {
        /// The assistant's message content (text and/or tool uses)
        message: AssistantMessage,
        /// Session identifier for message correlation
        session_id: String,
    },

    /// Input messages to Claude.
    ///
    /// # Lifecycle
    ///
    /// User messages serve two distinct purposes:
    ///
    /// 1. **Human Input**: Direct prompts from the user starting a new exchange
    /// 2. **Tool Results**: Automatically injected after `Assistant(ToolUse)`
    ///
    /// Tool results are generated by the runtime, not the human user. They
    /// always follow an assistant message containing tool uses and provide
    /// the execution results back to Claude.
    ///
    /// # Example
    /// ```text
    /// User("What files are here?")        // Human input
    /// User(ToolResult: ["a.txt", "b.rs"]) // Automatic after ToolUse
    /// ```
    User {
        /// The user's message content (text or tool results)
        message: UserMessage,
        /// Session identifier for message correlation
        session_id: String,
    },

    /// Terminal message ending a conversation.
    ///
    /// # Lifecycle
    ///
    /// Always the final message in a conversation. Contains session metrics
    /// and indicates whether the conversation completed successfully or
    /// encountered an error.
    ///
    /// After a Result message:
    /// - The conversation is complete
    /// - No further messages should be sent
    /// - The process may exit (depending on mode)
    ///
    /// # Subtypes
    ///
    /// - `Success`: Normal completion
    /// - `ErrorMaxTurns`: Hit configured turn limit
    /// - `ErrorDuringExecution`: Runtime or processing error
    Result {
        /// Type of result (success or error variant)
        subtype: ResultSubtype,
        /// Total wall-clock time in milliseconds
        duration_ms: u64,
        /// Time spent in API calls in milliseconds
        duration_api_ms: u64,
        /// Whether this result represents an error condition
        is_error: bool,
        /// Number of conversation turns completed
        num_turns: u32,
        /// Final result text (usually assistant's last response)
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        /// Session identifier for message correlation
        session_id: String,
        /// Total cost in USD for this conversation
        total_cost_usd: f64,
    },

    /// Initialization message establishing session context.
    ///
    /// # Lifecycle
    ///
    /// Always the first message in any session. Sent by Claude immediately
    /// after process startup to indicate readiness and report configuration.
    ///
    /// Contains critical session context:
    /// - Working directory for file operations
    /// - Available tools Claude can use
    /// - Model and permission configuration
    /// - MCP server connections
    ///
    /// The host should wait for this message before sending any user input.
    ///
    /// # Example
    /// ```json
    /// {
    ///   "type": "system",
    ///   "subtype": "init",
    ///   "cwd": "/home/user/project",
    ///   "tools": ["Read", "Write", "Bash"],
    ///   "model": "claude-3-opus",
    ///   "permissionMode": "default"
    /// }
    /// ```
    System {
        /// System message subtype (currently only Init)
        subtype: SystemSubtype,
        /// Source of API key authentication
        #[serde(rename = "apiKeySource")]
        api_key_source: ApiKeySource,
        /// Current working directory for file operations
        cwd: String,
        /// Session identifier for message correlation
        session_id: String,
        /// List of available tool names
        tools: Vec<String>,
        /// Connected MCP (Model Context Protocol) servers
        mcp_servers: Vec<McpServer>,
        /// Active model identifier
        model: String,
        /// Permission mode for tool execution
        #[serde(rename = "permissionMode")]
        permission_mode: PermissionMode,
    },
}

impl SdkMessage {
    /// Returns true if this message terminates the conversation.
    ///
    /// Only `Result` messages are terminal. After receiving a terminal
    /// message, no further messages should be sent in this conversation.
    pub fn is_terminal(&self) -> bool {
        matches!(self, SdkMessage::Result { .. })
    }

    /// Extract the session ID from any message type.
    ///
    /// All messages contain a session ID for correlation across
    /// multiple turns and potential process restarts.
    pub fn session_id(&self) -> &str {
        match self {
            SdkMessage::Assistant { session_id, .. }
            | SdkMessage::User { session_id, .. }
            | SdkMessage::Result { session_id, .. }
            | SdkMessage::System { session_id, .. } => session_id,
        }
    }

    /// Returns the message type as a string for logging/debugging.
    pub fn message_type(&self) -> &str {
        match self {
            SdkMessage::Assistant { .. } => "assistant",
            SdkMessage::User { .. } => "user",
            SdkMessage::Result { .. } => "result",
            SdkMessage::System { .. } => "system",
        }
    }
}

/// Result message subtypes indicating completion status.
///
/// These appear in the final `Result` message to indicate how
/// the conversation ended and whether it was successful.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResultSubtype {
    /// Normal successful completion.
    ///
    /// The conversation completed without errors. Claude provided
    /// all requested responses and any tool executions succeeded.
    Success,

    /// Conversation hit the maximum turn limit.
    ///
    /// The configured `max_turns` was reached before the conversation
    /// naturally completed. This is a safety mechanism to prevent
    /// infinite loops in tool usage.
    ErrorMaxTurns,

    /// An error occurred during execution.
    ///
    /// This indicates a runtime error such as:
    /// - Tool execution failure
    /// - API communication error
    /// - Invalid message format
    /// - Process crash or timeout
    ErrorDuringExecution,
}

/// System message subtypes.
///
/// Currently only `Init` is defined, but the protocol allows
/// for future system message types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SystemSubtype {
    /// Initialization message sent on process startup.
    ///
    /// Indicates Claude is ready to receive messages and reports
    /// the session configuration including available tools and model.
    Init,
}

/// Permission modes controlling tool execution behavior.
///
/// These modes determine how Claude handles tool execution and
/// what capabilities are available during the conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Standard permission mode with normal confirmation flow.
    ///
    /// Tools require appropriate permissions and may prompt for
    /// confirmation depending on the operation risk level.
    Default,

    /// Automatically accept file edits without confirmation.
    ///
    /// Edit and Write tools will execute without user confirmation.
    /// Other potentially dangerous operations still require approval.
    AcceptEdits,

    /// Bypass all permission checks (dangerous!).
    ///
    /// All tools execute without any confirmation. Should only be
    /// used in fully controlled environments with trusted input.
    BypassPermissions,

    /// Planning mode - describe actions without executing.
    ///
    /// Claude will describe what it would do but won't actually
    /// execute any tools. Useful for reviewing changes before applying.
    Plan,
}

/// Source of API key for Claude authentication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ApiKeySource {
    /// No API key provided (using defaults or local mode)
    None,
    /// API key from environment variable (ANTHROPIC_API_KEY)
    Environment,
    /// API key from configuration file
    Config,
    /// API key provided via command line argument
    Argument,
}

/// MCP (Model Context Protocol) server connection information.
///
/// MCP servers provide additional context and capabilities to Claude
/// through external tool providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServer {
    /// Server identifier name
    pub name: String,
    /// Connection status
    pub status: McpServerStatus,
}

/// MCP server connection status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpServerStatus {
    /// Successfully connected to MCP server
    Connected,
    /// Connection failed or disconnected
    Disconnected,
    /// Connection error with details
    Error(String),
}

/// Message sender role in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Human user or tool result sender
    User,
    /// Claude assistant
    Assistant,
}

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
}

impl AssistantMessage {
    /// Create a text-only assistant message.
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![MessageContent::Text { text: text.into() }],
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
                content: content.into(),
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
                    } => format!("[Tool Result {}]: {}", tool_use_id, content),
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
        /// Execution result as string
        content: String,
    },
}

// Custom serialization to handle both string and array formats for UserContent
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
/// Assistant messages contain an ordered array of these blocks,
/// which can be text responses or tool invocations. The order
/// is significant and represents Claude's intended sequence.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// Extract text content if this is a text block.
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
    fn test_message_type_detection() {
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
    fn test_assistant_message_helpers() {
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
    fn test_user_message_constructors() {
        let text_msg = UserMessage::from_text("Hello");
        assert_eq!(text_msg.as_text(), Some("Hello"));
        assert!(!text_msg.is_tool_result());

        let tool_msg = UserMessage::from_tool_result("tool_123", "Success");
        assert!(tool_msg.is_tool_result());
        assert_eq!(tool_msg.as_text(), None);
    }
}
