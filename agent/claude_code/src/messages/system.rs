//! System-level enums and settings: subtypes, permission modes, API key source,
//! MCP server descriptors, and the conversation [`Role`] tag.

use serde::{Deserialize, Serialize};

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
