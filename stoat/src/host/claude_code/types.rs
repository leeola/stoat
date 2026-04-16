//! Shared data types used across the Claude Code host API.
//!
//! These describe the *shape* of tool calls, plans, and usage data. They
//! live here (in `stoat::host`) rather than in `agent/claude_code`
//! because both sides of the trait boundary need to reference them, and
//! the agent crate already depends on this one.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Classifies a tool call for display purposes. Populated by the
/// agent crate's classifier.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Execute,
    Search,
    Fetch,
    Think,
    SwitchMode,
    Other,
}

/// Lifecycle state of a tool call.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// Display content attached to a tool call. A tool call can carry
/// multiple content entries rendered in order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallContent {
    Text {
        text: String,
    },
    Diff {
        path: PathBuf,
        old_text: Option<String>,
        new_text: String,
    },
    Terminal {
        terminal_id: String,
    },
    Image {
        data: String,
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
    Resource {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    ResourceLink {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

/// File/line location associated with a tool call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallLocation {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// Terminal lifecycle metadata surfaced alongside Bash tool updates
/// when the client supports terminal output streaming.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TerminalMeta {
    pub terminal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
}

/// Plan/TodoWrite checklist entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanEntry {
    pub content: String,
    pub status: PlanEntryStatus,
    /// `"medium"` for TodoWrite-derived plans.
    pub priority: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
}

/// Display metadata for one of the session's permission modes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Display metadata for one of the session's available models.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Token usage (input / output / cached) for a single turn or
/// accumulated across a session. A host-side mirror of the wire-level
/// Usage struct in the agent crate.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}
