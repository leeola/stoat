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
        /// Present when this assistant turn belongs to a subagent
        /// invocation; the id matches the parent `tool_use` id that
        /// spawned the subagent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
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
        /// Optional UUID the host stamped on outbound user frames.
        /// Present on echoes when `replay-user-messages` is enabled;
        /// the adapter uses it to drop the CLI's replay of our own
        /// input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_uuid: Option<String>,
        /// Parent tool_use id when this user message is a subagent
        /// tool result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        /// Session identifier for message correlation
        session_id: String,
        /// Total cost in USD for this conversation
        total_cost_usd: f64,
        /// Aggregate token usage for the turn. Missing on older CLI
        /// releases; callers should treat absence as "unknown", not zero.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Per-model usage breakdown when the session touched more than
        /// one model. Keyed by model id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_usage: Option<HashMap<String, ModelUsage>>,
        /// Reason the model stopped (`end_turn`, `max_tokens`, ...).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
        /// Set when this `Result` terminates a subagent's run.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
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
        /// System message subtype. `Init` carries the full session
        /// context; other subtypes use only a subset of the fields
        /// below.
        subtype: SystemSubtype,
        /// Session identifier. Present on every system frame.
        session_id: String,
        /// Source of API key authentication (init only).
        #[serde(
            rename = "apiKeySource",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        api_key_source: Option<ApiKeySource>,
        /// Current working directory for file operations (init only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// List of available tool names (init only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools: Option<Vec<String>>,
        /// Connected MCP servers (init only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mcp_servers: Option<Vec<McpServer>>,
        /// Active model identifier (init only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Permission mode for tool execution (init only).
        #[serde(
            rename = "permissionMode",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        permission_mode: Option<PermissionMode>,
        /// Session state payload for `session_state_changed`
        /// (e.g. `"idle"`, `"busy"`, `"compacting"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<String>,
        /// Status payload for `status` frames (e.g. `"compacting"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        /// Text payload used by `local_command_output` and some
        /// `status`/`task_notification` frames.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// Reason a compaction boundary fired
        /// (`context_limit_pressure`, `manual`, etc.).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
        /// Pre-compaction token total for `compact_boundary`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pre_tokens: Option<u64>,
        /// Post-compaction token total for `compact_boundary`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        post_tokens: Option<u64>,
        /// Subagent task identifier for `task_*` frames.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        /// Human-readable title for `task_started` and related frames.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Parent tool-use id for subagent `task_*` frames.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
        /// Hook event kind for `hook_started` / `hook_progress` /
        /// `hook_response`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hook_event_name: Option<String>,
        /// File paths that the CLI just persisted (for `files_persisted`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        paths: Option<Vec<String>>,
        /// API retry attempt counter for `api_retry`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<u32>,
        /// API retry reason string for `api_retry`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Any additional subtype-specific fields we have not modelled
        /// explicitly. Preserved so callers can inspect or forward the
        /// raw JSON without schema loss.
        #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
        extra: serde_json::Map<String, serde_json::Value>,
    },

    /// Partial-message streaming event.
    ///
    /// Emitted when the session was started with
    /// `--include-partial-messages`. Carries one raw Anthropic API
    /// streaming event (`message_start`, `content_block_start`,
    /// `content_block_delta`, etc.). The payload is intentionally
    /// retained as opaque JSON; callers interested in a specific event
    /// subtype should use an accessor like [`SdkMessage::as_text_delta`]
    /// rather than matching on the tree directly.
    #[serde(rename = "stream_event")]
    StreamEvent {
        /// Raw Anthropic streaming-event JSON.
        event: serde_json::Value,
        /// Session identifier for message correlation.
        session_id: String,
        /// Parent tool-use id when the event originates from a
        /// sub-agent; absent for primary assistant streams.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
    },

    /// Control-protocol request from the CLI (e.g. a `can_use_tool`
    /// permission prompt or a `hook_callback`). The host replies via a
    /// [`ControlResponse`] on stdin carrying the same `request_id`.
    ///
    /// The inner request payload is kept as raw JSON to keep the wire
    /// model decoupled from the host trait; use accessors like
    /// [`SdkMessage::as_can_use_tool`] to extract strongly-typed views.
    #[serde(rename = "control_request")]
    ControlRequest {
        request_id: String,
        request: serde_json::Value,
    },

    /// Inbound control-protocol response to a control_request that the
    /// host sent earlier (e.g. interrupt, set_model, set_permission_mode).
    /// The CLI replies with a matching `request_id`; the correlator
    /// routes the response back to the awaiting caller.
    #[serde(rename = "control_response")]
    ControlResponse {
        /// Usually a nested `{ subtype: "success" | "error", request_id, ... }`
        /// object matching the wire shape defined by [`ControlResponse`].
        response: serde_json::Value,
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

    /// Extract the session ID from any message type, when present.
    ///
    /// Most messages carry a session ID; [`SdkMessage::ControlRequest`]
    /// does not (control-protocol framing is session-scoped implicitly
    /// via the subprocess), so it returns `""` for that variant.
    pub fn session_id(&self) -> &str {
        match self {
            SdkMessage::Assistant { session_id, .. }
            | SdkMessage::User { session_id, .. }
            | SdkMessage::Result { session_id, .. }
            | SdkMessage::System { session_id, .. }
            | SdkMessage::StreamEvent { session_id, .. } => session_id,
            SdkMessage::ControlRequest { .. } | SdkMessage::ControlResponse { .. } => "",
        }
    }

    /// Returns the message type as a string for logging/debugging.
    pub fn message_type(&self) -> &str {
        match self {
            SdkMessage::Assistant { .. } => "assistant",
            SdkMessage::User { .. } => "user",
            SdkMessage::Result { .. } => "result",
            SdkMessage::System { .. } => "system",
            SdkMessage::StreamEvent { .. } => "stream_event",
            SdkMessage::ControlRequest { .. } => "control_request",
            SdkMessage::ControlResponse { .. } => "control_response",
        }
    }

    /// If this is an inbound `control_response`, return
    /// `(request_id, subtype)` where `subtype` is `"success"` or
    /// `"error"`. Used by the correlator to route responses back to
    /// the caller waiting on the matching request_id.
    pub fn as_control_response(&self) -> Option<(&str, &str, &serde_json::Value)> {
        let SdkMessage::ControlResponse { response } = self else {
            return None;
        };
        let request_id = response.get("request_id")?.as_str()?;
        let subtype = response.get("subtype")?.as_str()?;
        Some((request_id, subtype, response))
    }

    /// If this is a `stream_event` carrying a `content_block_delta`
    /// whose inner `delta.type` is `text_delta`, returns the delta's
    /// text. Returns `None` for every other shape.
    pub fn as_text_delta(&self) -> Option<&str> {
        self.stream_event_delta("text_delta")?
            .get("text")
            .and_then(|v| v.as_str())
    }

    /// If this is a `stream_event` carrying a `content_block_delta`
    /// whose inner `delta.type` is `input_json_delta`, returns the
    /// partial JSON chunk. Used to stream tool-use `input` as it's
    /// produced, before the full assistant message arrives.
    pub fn as_input_json_delta(&self) -> Option<&str> {
        self.stream_event_delta("input_json_delta")?
            .get("partial_json")
            .and_then(|v| v.as_str())
    }

    /// If this is a `stream_event` carrying a `content_block_delta`
    /// whose inner `delta.type` is `thinking_delta`, returns the
    /// streaming thinking text.
    pub fn as_thinking_delta(&self) -> Option<&str> {
        self.stream_event_delta("thinking_delta")?
            .get("thinking")
            .and_then(|v| v.as_str())
    }

    /// If this is a `stream_event` with `type == "content_block_start"`,
    /// returns the inner `content_block` object and its `index`.
    pub fn as_content_block_start(&self) -> Option<(u64, &serde_json::Value)> {
        let event = self.stream_event_of_type("content_block_start")?;
        let index = event.get("index")?.as_u64()?;
        let block = event.get("content_block")?;
        Some((index, block))
    }

    /// If this is a `stream_event` with `type == "content_block_stop"`,
    /// returns the block `index`.
    pub fn as_content_block_stop(&self) -> Option<u64> {
        self.stream_event_of_type("content_block_stop")?
            .get("index")?
            .as_u64()
    }

    /// Returns the `index` field of any `content_block_delta` event,
    /// regardless of the inner delta type.
    pub fn content_block_delta_index(&self) -> Option<u64> {
        self.stream_event_of_type("content_block_delta")?
            .get("index")?
            .as_u64()
    }

    /// If this is a `stream_event` with `type == "message_start"`,
    /// returns the inner `message` object (raw JSON).
    pub fn as_message_start(&self) -> Option<&serde_json::Value> {
        self.stream_event_of_type("message_start")?.get("message")
    }

    /// If this is a `stream_event` with `type == "message_delta"`,
    /// returns the inner `delta` object (raw JSON).
    pub fn as_message_delta(&self) -> Option<&serde_json::Value> {
        self.stream_event_of_type("message_delta")?.get("delta")
    }

    /// Returns true when this is a `stream_event` with `type == "message_stop"`.
    pub fn is_message_stop(&self) -> bool {
        self.stream_event_of_type("message_stop").is_some()
    }

    /// Internal: return the inner `event` object if it matches the
    /// requested top-level `type` string; `None` otherwise.
    fn stream_event_of_type(&self, expected: &str) -> Option<&serde_json::Value> {
        let SdkMessage::StreamEvent { event, .. } = self else {
            return None;
        };
        if event.get("type").and_then(|v| v.as_str())? != expected {
            return None;
        }
        Some(event)
    }

    /// Internal: walk the `content_block_delta` → `delta` branch and
    /// confirm the inner `delta.type`. Returns the `delta` object on match.
    fn stream_event_delta(&self, expected: &str) -> Option<&serde_json::Value> {
        let event = self.stream_event_of_type("content_block_delta")?;
        let delta = event.get("delta")?;
        if delta.get("type").and_then(|v| v.as_str())? != expected {
            return None;
        }
        Some(delta)
    }

    /// If this is a `control_request` with `subtype == "can_use_tool"`,
    /// returns a strongly-typed view of the permission request.
    pub fn as_can_use_tool(&self) -> Option<CanUseToolRequest<'_>> {
        let SdkMessage::ControlRequest {
            request_id,
            request,
        } = self
        else {
            return None;
        };
        if request.get("subtype").and_then(|v| v.as_str())? != "can_use_tool" {
            return None;
        }
        Some(CanUseToolRequest {
            request_id: request_id.as_str(),
            tool_name: request.get("tool_name").and_then(|v| v.as_str())?,
            input: request.get("input")?,
            permission_suggestions: request.get("permission_suggestions"),
            tool_use_id: request.get("tool_use_id").and_then(|v| v.as_str()),
            agent_id: request.get("agent_id").and_then(|v| v.as_str()),
            blocked_path: request.get("blocked_path").and_then(|v| v.as_str()),
        })
    }

    /// If this is a `control_request` with `subtype == "hook_callback"`,
    /// returns a strongly-typed view of the hook invocation.
    pub fn as_hook_callback(&self) -> Option<HookCallbackRequest<'_>> {
        let SdkMessage::ControlRequest {
            request_id,
            request,
        } = self
        else {
            return None;
        };
        if request.get("subtype").and_then(|v| v.as_str())? != "hook_callback" {
            return None;
        }
        Some(HookCallbackRequest {
            request_id: request_id.as_str(),
            callback_id: request.get("callback_id").and_then(|v| v.as_str())?,
            input: request.get("input"),
            tool_use_id: request.get("tool_use_id").and_then(|v| v.as_str()),
        })
    }
}

/// Strongly-typed view of a `can_use_tool` control request, borrowed
/// from the underlying [`SdkMessage::ControlRequest`]'s raw JSON.
#[derive(Debug, Clone, Copy)]
pub struct CanUseToolRequest<'a> {
    pub request_id: &'a str,
    pub tool_name: &'a str,
    pub input: &'a serde_json::Value,
    pub permission_suggestions: Option<&'a serde_json::Value>,
    pub tool_use_id: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub blocked_path: Option<&'a str>,
}

/// Strongly-typed view of a `hook_callback` control request.
#[derive(Debug, Clone, Copy)]
pub struct HookCallbackRequest<'a> {
    pub request_id: &'a str,
    pub callback_id: &'a str,
    pub input: Option<&'a serde_json::Value>,
    pub tool_use_id: Option<&'a str>,
}

/// Outbound control-protocol response. Serialized once and written to
/// child stdin; never inbound. The top-level `type` is
/// `"control_response"`; the inner `response` carries a success or
/// error subtype plus the original `request_id`.
#[derive(Debug, Clone, Serialize)]
pub struct ControlResponse {
    #[serde(rename = "type")]
    kind: &'static str,
    pub response: ControlResponseBody,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ControlResponseBody {
    Success {
        request_id: String,
        response: serde_json::Value,
    },
    Error {
        request_id: String,
        error: String,
    },
}

impl ControlResponse {
    pub fn success(request_id: impl Into<String>, response: serde_json::Value) -> Self {
        Self {
            kind: "control_response",
            response: ControlResponseBody::Success {
                request_id: request_id.into(),
                response,
            },
        }
    }

    pub fn error(request_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            kind: "control_response",
            response: ControlResponseBody::Error {
                request_id: request_id.into(),
                error: error.into(),
            },
        }
    }
}

/// Result message subtypes indicating completion status.
///
/// These appear in the final `Result` message to indicate how
/// the conversation ended and whether it was successful.
///
/// Unrecognized subtype strings are captured in [`ResultSubtype::Unknown`]
/// so the parser does not reject messages when the CLI introduces a new
/// error category.
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

    /// Conversation hit a configured USD budget ceiling (`--max-budget-usd`).
    ErrorMaxBudgetUsd,

    /// Structured-output retries hit their configured cap.
    ErrorMaxStructuredOutputRetries,

    /// An error occurred during execution.
    ///
    /// This indicates a runtime error such as:
    /// - Tool execution failure
    /// - API communication error
    /// - Invalid message format
    /// - Process crash or timeout
    ErrorDuringExecution,

    /// Unrecognized subtype. Preserves the raw string so callers can
    /// surface or log the value without the message being dropped.
    #[serde(untagged)]
    Unknown(String),
}

/// Token usage accounting for a single assistant turn.
///
/// Emitted on `Assistant` messages and aggregated on `Result`. Cache
/// fields are optional because the API only populates them when prompt
/// caching is active on the turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

/// Per-model usage breakdown emitted on the terminal `Result` message
/// when the session touched more than one model (e.g. primary + sub-agent).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// Reason the model stopped producing the current turn. Mirrors the
/// Anthropic API's `stop_reason` values plus a fallback for unknown strings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    PauseTurn,
    Refusal,
    /// Session was cancelled mid-turn.
    Cancelled,
    #[serde(untagged)]
    Unknown(String),
}

/// System message subtypes.
///
/// The CLI emits a variety of system messages during a session. `Init`
/// is the only one required by the protocol contract (it's always the
/// first frame on stdout); the rest describe transient events that a
/// client may opt in to via flags like `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS`
/// or `--include-hook-events`.
///
/// Unknown subtype strings fall through to [`SystemSubtype::Unknown`] so
/// a new CLI release cannot break stdout parsing just by emitting a new
/// subtype name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SystemSubtype {
    /// Initialization message sent on process startup.
    ///
    /// Indicates Claude is ready to receive messages and reports
    /// the session configuration including available tools and model.
    Init,
    /// Lightweight status update (e.g. "compacting").
    Status,
    /// The CLI just finished a context-compaction pass. Accumulated
    /// usage should reset at this boundary.
    CompactBoundary,
    /// Output from a local-only slash command (e.g. `/context`).
    LocalCommandOutput,
    /// Session transitioned between busy/idle/compacting states.
    SessionStateChanged,
    /// A hook event fired (pre/post tool use, stop, etc.).
    HookStarted,
    /// Progress signal for a long-running hook.
    HookProgress,
    /// Hook completed with a response body.
    HookResponse,
    /// Files were persisted by an editor tool.
    FilesPersisted,
    /// A subagent task started.
    TaskStarted,
    /// A subagent posted a user-facing notification.
    TaskNotification,
    /// Progress update from a running subagent task.
    TaskProgress,
    /// Subagent task state updated.
    TaskUpdated,
    /// Elicitation flow completed (interactive dialog with the client).
    ElicitationComplete,
    /// API call is being retried.
    ApiRetry,
    /// Subtype string not recognised by this crate; preserves raw value.
    #[serde(untagged)]
    Unknown(String),
}

/// Source of settings (`.claude/settings.json`) loaded alongside the
/// session. Mirrors the CLI's `--setting-sources` comma-separated list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingSource {
    User,
    Project,
    Local,
}

impl SettingSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SettingSource::User => "user",
            SettingSource::Project => "project",
            SettingSource::Local => "local",
        }
    }
}

/// Permission modes controlling tool execution behavior.
///
/// These modes determine how Claude handles tool execution and
/// what capabilities are available during the conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Model classifier approves/denies tool use; no user confirmation.
    Auto,

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

    /// Never prompt the user; auto-deny anything not pre-approved.
    DontAsk,

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
#[serde(rename_all = "kebab-case")]
pub enum McpServerStatus {
    Connected,
    Disconnected,
    NeedsAuth,
    #[serde(untagged)]
    Other(String),
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
            usage: None,
            model_usage: None,
            stop_reason: None,
            parent_tool_use_id: None,
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
            model: None,
            usage: None,
            id: None,
            stop_reason: None,
            stop_sequence: None,
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

    #[test]
    fn message_content_known_text_parses() {
        let json = r#"{"type":"text","text":"hi"}"#;
        let parsed: MessageContent = serde_json::from_str(json).unwrap();
        match parsed {
            MessageContent::Text { text } => assert_eq!(text, "hi"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn message_content_known_tool_use_parses() {
        let json = r#"{"type":"tool_use","id":"abc","name":"Read","input":{"path":"/tmp"}}"#;
        let parsed: MessageContent = serde_json::from_str(json).unwrap();
        match parsed {
            MessageContent::ToolUse { id, name, input } => {
                assert_eq!(id, "abc");
                assert_eq!(name, "Read");
                assert_eq!(
                    input.get("path"),
                    Some(&serde_json::Value::String("/tmp".to_string()))
                );
            },
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn message_content_image_parses() {
        let json = r#"{"type":"image","source":{"kind":"base64","data":"xyz"}}"#;
        let parsed: MessageContent = serde_json::from_str(json).unwrap();
        match parsed {
            MessageContent::Image { source } => {
                assert_eq!(source.get("data").and_then(|d| d.as_str()), Some("xyz"));
                assert_eq!(source.get("kind").and_then(|k| k.as_str()), Some("base64"));
            },
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn message_content_unknown_tag_preserved() {
        let json = r#"{"type":"audio","source":{"data":"xyz"}}"#;
        let parsed: MessageContent = serde_json::from_str(json).unwrap();
        match parsed {
            MessageContent::Unknown(value) => {
                assert_eq!(value.get("type").and_then(|v| v.as_str()), Some("audio"));
                assert_eq!(
                    value
                        .get("source")
                        .and_then(|s| s.get("data"))
                        .and_then(|d| d.as_str()),
                    Some("xyz")
                );
            },
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn message_content_malformed_tool_use_falls_back() {
        let json = r#"{"type":"tool_use","id":"x"}"#;
        let parsed: MessageContent = serde_json::from_str(json).unwrap();
        assert!(
            parsed.is_unknown(),
            "expected Unknown for malformed tool_use, got {parsed:?}"
        );
    }

    #[test]
    fn sdk_assistant_with_unknown_content_still_parses() {
        let json = r#"{
            "type": "assistant",
            "session_id": "sess-1",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "server_tool_use", "id": "srv_1", "tool": "web_search"},
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"p": "/x"}}
                ]
            }
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        let SdkMessage::Assistant { message, .. } = parsed else {
            panic!("expected Assistant variant");
        };
        assert_eq!(message.content.len(), 3);
        assert!(message.content[0].is_text());
        assert!(message.content[1].is_unknown());
        assert!(message.content[2].is_tool_use());
    }

    #[test]
    fn message_content_thinking_parses() {
        let json = r#"{"type":"thinking","thinking":"let me think","signature":"sig-abc"}"#;
        let parsed: MessageContent = serde_json::from_str(json).unwrap();
        match parsed {
            MessageContent::Thinking {
                thinking,
                signature,
            } => {
                assert_eq!(thinking, "let me think");
                assert_eq!(signature, "sig-abc");
            },
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn message_content_redacted_thinking_parses() {
        let json = r#"{"type":"redacted_thinking","data":"encrypted-blob"}"#;
        let parsed: MessageContent = serde_json::from_str(json).unwrap();
        match parsed {
            MessageContent::RedactedThinking { data } => assert_eq!(data, "encrypted-blob"),
            other => panic!("expected RedactedThinking, got {other:?}"),
        }
    }

    #[test]
    fn message_content_server_tool_use_parses() {
        let json = r#"{"type":"server_tool_use","id":"srv_1","name":"web_search","input":{"query":"rust"}}"#;
        let parsed: MessageContent = serde_json::from_str(json).unwrap();
        match parsed {
            MessageContent::ServerToolUse { id, name, input } => {
                assert_eq!(id, "srv_1");
                assert_eq!(name, "web_search");
                assert_eq!(input.get("query"), Some(&serde_json::json!("rust")));
            },
            other => panic!("expected ServerToolUse, got {other:?}"),
        }
    }

    #[test]
    fn message_content_server_tool_result_parses() {
        let json = r#"{"type":"server_tool_result","tool_use_id":"srv_1","content":"found"}"#;
        let parsed: MessageContent = serde_json::from_str(json).unwrap();
        match parsed {
            MessageContent::ServerToolResult {
                tool_use_id,
                content,
            } => {
                assert_eq!(tool_use_id, "srv_1");
                assert_eq!(content, serde_json::json!("found"));
            },
            other => panic!("expected ServerToolResult, got {other:?}"),
        }
    }

    #[test]
    fn user_content_block_tool_result_accepts_plain_string() {
        let json = r#"{"type":"tool_result","tool_use_id":"t1","content":"ok"}"#;
        let parsed: UserContentBlock = serde_json::from_str(json).unwrap();
        match parsed {
            UserContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "t1");
                assert_eq!(content.as_text(), "ok");
                assert_eq!(is_error, None);
            },
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn user_content_block_tool_result_accepts_structured_blocks() {
        let json = r#"{
            "type":"tool_result",
            "tool_use_id":"t1",
            "content":[{"type":"text","text":"line1"},{"type":"text","text":"line2"}],
            "is_error": true
        }"#;
        let parsed: UserContentBlock = serde_json::from_str(json).unwrap();
        match parsed {
            UserContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "t1");
                assert_eq!(is_error, Some(true));
                let flattened = content.as_text();
                assert!(flattened.contains("line1"));
                assert!(flattened.contains("line2"));
            },
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn user_content_block_tool_result_is_error_absent_defaults_none() {
        let json = r#"{"type":"tool_result","tool_use_id":"t1","content":"ok"}"#;
        let parsed: UserContentBlock = serde_json::from_str(json).unwrap();
        match parsed {
            UserContentBlock::ToolResult { is_error, .. } => assert_eq!(is_error, None),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn stream_event_parses_and_exposes_text_delta() {
        let json = r#"{
            "type": "stream_event",
            "session_id": "sess-s",
            "event": {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "hello "}
            }
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.message_type(), "stream_event");
        assert_eq!(parsed.session_id(), "sess-s");
        assert_eq!(parsed.as_text_delta(), Some("hello "));
    }

    #[test]
    fn stream_event_non_text_delta_returns_none() {
        let json = r#"{
            "type": "stream_event",
            "session_id": "sess-s",
            "event": {
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": "{\"x\":"}
            }
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.as_text_delta(), None);
    }

    #[test]
    fn stream_event_message_start_returns_none() {
        let json = r#"{
            "type": "stream_event",
            "session_id": "sess-s",
            "event": {"type": "message_start", "message": {}}
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.as_text_delta(), None);
    }

    #[test]
    fn non_stream_event_text_delta_returns_none() {
        let msg = SdkMessage::Result {
            subtype: ResultSubtype::Success,
            duration_ms: 1,
            duration_api_ms: 1,
            is_error: false,
            num_turns: 1,
            result: None,
            session_id: "sess".into(),
            total_cost_usd: 0.0,
            usage: None,
            model_usage: None,
            stop_reason: None,
            parent_tool_use_id: None,
        };
        assert_eq!(msg.as_text_delta(), None);
    }

    #[test]
    fn control_request_can_use_tool_parses_and_extracts() {
        let json = r#"{
            "type": "control_request",
            "request_id": "req_7",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "input": {"command": "ls /"},
                "tool_use_id": "toolu_1"
            }
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.message_type(), "control_request");
        let req = parsed
            .as_can_use_tool()
            .expect("expected CanUseToolRequest view");
        assert_eq!(req.request_id, "req_7");
        assert_eq!(req.tool_name, "Bash");
        assert_eq!(req.tool_use_id, Some("toolu_1"));
        assert_eq!(req.input["command"], serde_json::json!("ls /"));
        assert!(parsed.as_hook_callback().is_none());
    }

    #[test]
    fn control_request_hook_callback_parses_and_extracts() {
        let json = r#"{
            "type": "control_request",
            "request_id": "req_8",
            "request": {
                "subtype": "hook_callback",
                "callback_id": "cb_0",
                "input": {"foo": "bar"},
                "tool_use_id": "toolu_2"
            }
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        let req = parsed
            .as_hook_callback()
            .expect("expected HookCallbackRequest view");
        assert_eq!(req.request_id, "req_8");
        assert_eq!(req.callback_id, "cb_0");
        assert_eq!(req.tool_use_id, Some("toolu_2"));
        assert!(parsed.as_can_use_tool().is_none());
    }

    #[test]
    fn control_response_success_serializes_to_expected_shape() {
        let resp = ControlResponse::success(
            "req_7",
            serde_json::json!({"behavior": "allow", "updatedInput": {"command": "ls /"}}),
        );
        let encoded = serde_json::to_value(&resp).unwrap();
        assert_eq!(encoded["type"], "control_response");
        assert_eq!(encoded["response"]["subtype"], "success");
        assert_eq!(encoded["response"]["request_id"], "req_7");
        assert_eq!(encoded["response"]["response"]["behavior"], "allow");
        assert_eq!(
            encoded["response"]["response"]["updatedInput"]["command"],
            "ls /"
        );
    }

    #[test]
    fn control_response_error_serializes_to_expected_shape() {
        let resp = ControlResponse::error("req_7", "no callback");
        let encoded = serde_json::to_value(&resp).unwrap();
        assert_eq!(encoded["type"], "control_response");
        assert_eq!(encoded["response"]["subtype"], "error");
        assert_eq!(encoded["response"]["request_id"], "req_7");
        assert_eq!(encoded["response"]["error"], "no callback");
    }

    #[test]
    fn system_subtype_falls_back_to_unknown_variant() {
        let parsed: SystemSubtype = serde_json::from_str("\"frobnication\"").unwrap();
        assert_eq!(parsed, SystemSubtype::Unknown("frobnication".into()));
        // Known values still map correctly.
        let init: SystemSubtype = serde_json::from_str("\"init\"").unwrap();
        assert_eq!(init, SystemSubtype::Init);
        let compact: SystemSubtype = serde_json::from_str("\"compact_boundary\"").unwrap();
        assert_eq!(compact, SystemSubtype::CompactBoundary);
    }

    #[test]
    fn result_subtype_falls_back_to_unknown_variant() {
        let parsed: ResultSubtype = serde_json::from_str("\"error_something_new\"").unwrap();
        assert_eq!(parsed, ResultSubtype::Unknown("error_something_new".into()));
        let max_budget: ResultSubtype = serde_json::from_str("\"error_max_budget_usd\"").unwrap();
        assert_eq!(max_budget, ResultSubtype::ErrorMaxBudgetUsd);
    }

    #[test]
    fn system_non_init_subtype_parses_with_missing_init_fields() {
        // session_state_changed has no cwd/tools/model/permission_mode,
        // only session_id and a `state` payload.
        let json = r#"{
            "type": "system",
            "subtype": "session_state_changed",
            "session_id": "sess-9",
            "state": "idle"
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        match parsed {
            SdkMessage::System {
                subtype,
                session_id,
                cwd,
                tools,
                model,
                state,
                ..
            } => {
                assert_eq!(subtype, SystemSubtype::SessionStateChanged);
                assert_eq!(session_id, "sess-9");
                assert!(cwd.is_none());
                assert!(tools.is_none());
                assert!(model.is_none());
                assert_eq!(state.as_deref(), Some("idle"));
            },
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn system_unknown_subtype_parses_and_preserves_extra_fields() {
        let json = r#"{
            "type": "system",
            "subtype": "brand_new_event",
            "session_id": "sess-10",
            "payload": {"foo": 42},
            "other_field": "value"
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        match parsed {
            SdkMessage::System {
                subtype,
                session_id,
                extra,
                ..
            } => {
                assert_eq!(subtype, SystemSubtype::Unknown("brand_new_event".into()));
                assert_eq!(session_id, "sess-10");
                assert!(extra.contains_key("payload"));
                assert!(extra.contains_key("other_field"));
            },
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn system_compact_boundary_parses_with_token_fields() {
        let json = r#"{
            "type": "system",
            "subtype": "compact_boundary",
            "session_id": "sess-11",
            "trigger": "context_limit_pressure",
            "pre_tokens": 50000,
            "post_tokens": 10000
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        match parsed {
            SdkMessage::System {
                subtype,
                trigger,
                pre_tokens,
                post_tokens,
                ..
            } => {
                assert_eq!(subtype, SystemSubtype::CompactBoundary);
                assert_eq!(trigger.as_deref(), Some("context_limit_pressure"));
                assert_eq!(pre_tokens, Some(50000));
                assert_eq!(post_tokens, Some(10000));
            },
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn assistant_message_parses_with_usage_and_model() {
        let json = r#"{
            "type": "assistant",
            "session_id": "sess-12",
            "message": {
                "role": "assistant",
                "id": "msg_01",
                "model": "claude-sonnet-4-5",
                "content": [{"type":"text","text":"hello"}],
                "usage": {
                    "input_tokens": 42,
                    "output_tokens": 7,
                    "cache_read_input_tokens": 100
                },
                "stop_reason": "end_turn"
            }
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        match parsed {
            SdkMessage::Assistant { message, .. } => {
                assert_eq!(message.model.as_deref(), Some("claude-sonnet-4-5"));
                assert_eq!(message.id.as_deref(), Some("msg_01"));
                assert_eq!(message.stop_reason, Some(StopReason::EndTurn));
                let usage = message.usage.expect("usage present");
                assert_eq!(usage.input_tokens, 42);
                assert_eq!(usage.output_tokens, 7);
                assert_eq!(usage.cache_read_input_tokens, Some(100));
            },
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn result_parses_with_usage_and_stop_reason() {
        let json = r#"{
            "type": "result",
            "subtype": "success",
            "session_id": "sess-13",
            "duration_ms": 1000,
            "duration_api_ms": 800,
            "is_error": false,
            "num_turns": 2,
            "total_cost_usd": 0.01,
            "usage": {"input_tokens": 12, "output_tokens": 3},
            "stop_reason": "end_turn"
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        match parsed {
            SdkMessage::Result {
                usage, stop_reason, ..
            } => {
                let usage = usage.expect("usage present");
                assert_eq!(usage.input_tokens, 12);
                assert_eq!(usage.output_tokens, 3);
                assert_eq!(stop_reason, Some(StopReason::EndTurn));
            },
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn stream_event_input_json_delta_accessor() {
        let json = r#"{
            "type": "stream_event",
            "session_id": "sess-s",
            "event": {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{\"cmd\":"}
            }
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.as_input_json_delta(), Some("{\"cmd\":"));
        assert_eq!(parsed.as_text_delta(), None);
    }

    #[test]
    fn stream_event_thinking_delta_accessor() {
        let json = r#"{
            "type": "stream_event",
            "session_id": "sess-s",
            "event": {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "hmm"}
            }
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.as_thinking_delta(), Some("hmm"));
    }

    #[test]
    fn stream_event_content_block_start_accessor() {
        let json = r#"{
            "type": "stream_event",
            "session_id": "sess-s",
            "event": {
                "type": "content_block_start",
                "index": 3,
                "content_block": {"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}
            }
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        let (index, block) = parsed.as_content_block_start().expect("should extract");
        assert_eq!(index, 3);
        assert_eq!(block.get("name").and_then(|v| v.as_str()), Some("Bash"));
    }

    #[test]
    fn stream_event_content_block_stop_accessor() {
        let json = r#"{
            "type": "stream_event",
            "session_id": "sess-s",
            "event": {"type": "content_block_stop", "index": 2}
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.as_content_block_stop(), Some(2));
    }

    #[test]
    fn stream_event_message_stop_accessor() {
        let json = r#"{
            "type": "stream_event",
            "session_id": "sess-s",
            "event": {"type": "message_stop"}
        }"#;
        let parsed: SdkMessage = serde_json::from_str(json).unwrap();
        assert!(parsed.is_message_stop());
    }
}
