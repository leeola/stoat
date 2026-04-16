//! Permission-callback interface and outcome types.
//!
//! When a [`super::ClaudeCodeSession`] is built with a registered
//! [`PermissionCallback`], the underlying wrapper asks the `claude` CLI
//! to route permission prompts over the control protocol
//! (`--permission-prompt-tool-name stdio`). Each incoming `can_use_tool`
//! control request is forwarded here; the returned [`PermissionResult`]
//! becomes the control response.

use super::types::{ToolCallContent, ToolCallLocation, ToolKind};
use async_trait::async_trait;

/// Host-provided callback for interactive tool-permission prompts.
///
/// JSON payloads are passed as `&str` so this trait (and the `stoat`
/// crate) stays free of a `serde_json` dependency. Callbacks that need
/// structured access should parse the strings themselves.
#[async_trait]
pub trait PermissionCallback: Send + Sync {
    async fn can_use_tool(
        &self,
        tool_name: &str,
        input_json: &str,
        context: ToolPermissionContext<'_>,
    ) -> PermissionResult;
}

/// Context passed to a [`PermissionCallback::can_use_tool`] invocation.
///
/// Mirrors the fields in the Python SDK's `ToolPermissionContext`, plus
/// the classifier-derived metadata (`tool_kind`, `tool_title`,
/// `tool_content`, `tool_locations`) the dispatcher runs before
/// invoking the callback. Having the classifier output available here
/// lets hosts render a rich permission prompt (icon, title, inline
/// diff) without re-parsing `input_json`.
///
/// `suggestions_json` is the raw `permission_suggestions` array as a
/// JSON string, or `None` when absent. Tool fields default to empty
/// when the classifier has not yet run (e.g. in tests that synthesise
/// a context directly).
#[derive(Debug, Clone)]
pub struct ToolPermissionContext<'a> {
    pub suggestions_json: Option<&'a str>,
    pub tool_use_id: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub blocked_path: Option<&'a str>,
    pub tool_kind: ToolKind,
    pub tool_title: String,
    pub tool_content: Vec<ToolCallContent>,
    pub tool_locations: Vec<ToolCallLocation>,
}

impl<'a> ToolPermissionContext<'a> {
    /// Minimal constructor for hosts that have not run the classifier.
    pub fn bare() -> Self {
        Self {
            suggestions_json: None,
            tool_use_id: None,
            agent_id: None,
            blocked_path: None,
            tool_kind: ToolKind::Other,
            tool_title: String::new(),
            tool_content: Vec::new(),
            tool_locations: Vec::new(),
        }
    }
}

/// Scope of an `Allow` outcome. Lets a host store per-scope approvals
/// rather than re-prompting for every tool call:
/// `Once` applies only to this specific tool invocation; `Session`
/// remembers for the lifetime of the CC session; `Always` persists
/// into the workspace / user settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionScope {
    /// Allow this single invocation only.
    Once,
    /// Allow until the current session ends.
    Session,
    /// Allow permanently (persisted to the user/project's settings).
    Always,
}

/// Where a [`PermissionSuggestion`] should apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDestination {
    /// Applies for the current session only.
    Session,
    /// Persisted to the project's `.claude/settings.json`.
    Project,
    /// Persisted to the user's `~/.claude/settings.json`.
    User,
}

/// Action a [`PermissionRule`] prescribes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionBehavior {
    Allow,
    Deny,
    Ask,
}

/// A rule matched against future tool calls. A tool call matches when
/// its `name` equals `tool_name` (or `tool_name` is `None` to match all
/// tools) and, if present, its input satisfies `input_pattern` (an
/// opaque glob/regex string interpreted by the CLI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRule {
    pub tool_name: Option<String>,
    pub input_pattern: Option<String>,
}

/// Suggestion attached to an `Allow` outcome. The CLI applies these
/// rules for the remainder of the scope specified by
/// [`PermissionDestination`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionSuggestion {
    /// Switch the current permission mode.
    SetMode {
        mode: String,
        destination: PermissionDestination,
    },
    /// Add rules that auto-approve or auto-deny matching tool calls.
    AddRules {
        rules: Vec<PermissionRule>,
        behavior: PermissionBehavior,
        destination: PermissionDestination,
    },
}

/// Outcome of a [`PermissionCallback::can_use_tool`] invocation.
#[derive(Debug, Clone)]
pub enum PermissionResult {
    /// Permit the tool to execute. `scope` controls how long the
    /// approval applies; `updated_input_json` optionally replaces the
    /// input the CLI proposed (as a JSON object string);
    /// `updated_permissions` installs suggestions to broaden future
    /// approvals (e.g. allow-always rules).
    Allow {
        scope: PermissionScope,
        updated_input_json: Option<String>,
        updated_permissions: Vec<PermissionSuggestion>,
    },
    /// Block the tool invocation. `message` is surfaced to Claude; if
    /// `interrupt` is true, the agent run is aborted entirely.
    Deny { message: String, interrupt: bool },
    /// User dismissed the prompt without approving or denying. The
    /// CLI treats this as "tool not executed, no further run".
    Cancel,
}

impl PermissionResult {
    /// Convenience constructor: `Allow` with [`PermissionScope::Once`]
    /// and no updated input or permission suggestions. Preserved from
    /// the earlier trait shape so existing callers keep compiling.
    pub fn allow() -> Self {
        PermissionResult::Allow {
            scope: PermissionScope::Once,
            updated_input_json: None,
            updated_permissions: Vec::new(),
        }
    }

    /// Convenience constructor: `Allow` with explicit scope.
    pub fn allow_with_scope(scope: PermissionScope) -> Self {
        PermissionResult::Allow {
            scope,
            updated_input_json: None,
            updated_permissions: Vec::new(),
        }
    }

    /// Convenience constructor: simple `Deny` without interrupt.
    pub fn deny(message: impl Into<String>) -> Self {
        PermissionResult::Deny {
            message: message.into(),
            interrupt: false,
        }
    }

    /// Convenience constructor: `Cancel`.
    pub fn cancel() -> Self {
        PermissionResult::Cancel
    }
}
