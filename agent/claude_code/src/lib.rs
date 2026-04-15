pub mod auth;
pub mod claude_code;
mod host_adapter;
pub mod launcher;
pub mod messages;
pub mod model_prefs;
pub mod prompt;
pub mod settings;
pub mod slash;
pub mod tools;
pub mod utils;

pub use claude_code::{ClaudeCode, ClaudeCodeBuilder, SessionConfig};
pub use launcher::ClaudeCodeLauncher;
pub use messages::{
    AssistantMessage, McpServer, MessageContent, ModelUsage, PermissionMode, ResultSubtype,
    SdkMessage, StopReason, SystemSubtype, Usage, UserContent, UserContentBlock, UserMessage,
};
// Shared tool/plan/usage types live in `stoat::host`; re-export them
// through this crate for callers who only want a single import path.
pub use stoat::host::{
    ModeInfo, ModelInfo, PlanEntry, PlanEntryStatus, TerminalMeta, TokenUsage, ToolCallContent,
    ToolCallLocation, ToolCallStatus, ToolKind,
};
// Tool classifier + the agent-side classifier-output types.
pub use tools::{ToolInfo, ToolUpdate, ToolUseSnapshot};
