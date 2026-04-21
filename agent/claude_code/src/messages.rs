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
//!
//! # Organization
//!
//! Types are grouped into submodules by concept; every public type is
//! re-exported at this module root so existing
//! `crate::messages::TypeName` paths continue to resolve:
//!
//! - [`sdk`]: the root [`SdkMessage`] enum and its stream-event accessors.
//! - [`content`]: assistant/user message bodies and their content blocks.
//! - [`control`]: inbound [`CanUseToolRequest`]/[`HookCallbackRequest`] views plus the outbound
//!   [`ControlResponse`].
//! - [`result`]: terminal-result subtype, token usage, stop reasons.
//! - [`system`]: system-frame subtype and session-level enums (permissions, API key source, MCP
//!   servers, role tag).

pub mod content;
pub mod control;
pub mod result;
pub mod sdk;
pub mod system;

pub use content::{
    AssistantMessage, MessageContent, ToolResultContent, ToolUse, UserContent, UserContentBlock,
    UserMessage,
};
pub use control::{CanUseToolRequest, ControlResponse, ControlResponseBody, HookCallbackRequest};
pub use result::{ModelUsage, ResultSubtype, StopReason, Usage};
pub use sdk::SdkMessage;
pub use system::{
    ApiKeySource, McpServer, McpServerStatus, PermissionMode, Role, SettingSource, SystemSubtype,
};

#[cfg(test)]
mod tests;
