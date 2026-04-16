//! Hook-callback interface and data types.
//!
//! When a [`super::ClaudeCodeSession`] is built with `include_hook_events`,
//! or a user registers a [`HookCallback`] via the builder, the CLI's
//! `hook_callback` control requests are routed to the registered
//! callback and its [`HookResponse`] is serialized back to the CLI.

use async_trait::async_trait;

/// Host-provided callback for CLI hook events.
///
/// The event kind + payload are passed as `&str` so this trait (and
/// the consumer crate) stays free of any serde dependency.
#[async_trait]
pub trait HookCallback: Send + Sync {
    async fn handle_hook(&self, event: HookEvent<'_>) -> HookResponse;
}

/// Kind of hook the CLI is firing. Mirrors the event names defined by
/// the Claude Code SDK. Unknown names fall through to `Unknown` so a
/// new event doesn't break dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookKind {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    Stop,
    SubagentStop,
    SessionStart,
    SessionEnd,
    Notification,
    PreCompact,
    Unknown(String),
}

impl HookKind {
    /// Parse the CLI's hook event name into a [`HookKind`].
    pub fn from_name(name: &str) -> Self {
        match name {
            "PreToolUse" => HookKind::PreToolUse,
            "PostToolUse" => HookKind::PostToolUse,
            "UserPromptSubmit" => HookKind::UserPromptSubmit,
            "Stop" => HookKind::Stop,
            "SubagentStop" => HookKind::SubagentStop,
            "SessionStart" => HookKind::SessionStart,
            "SessionEnd" => HookKind::SessionEnd,
            "Notification" => HookKind::Notification,
            "PreCompact" => HookKind::PreCompact,
            other => HookKind::Unknown(other.to_string()),
        }
    }

    pub fn as_name(&self) -> &str {
        match self {
            HookKind::PreToolUse => "PreToolUse",
            HookKind::PostToolUse => "PostToolUse",
            HookKind::UserPromptSubmit => "UserPromptSubmit",
            HookKind::Stop => "Stop",
            HookKind::SubagentStop => "SubagentStop",
            HookKind::SessionStart => "SessionStart",
            HookKind::SessionEnd => "SessionEnd",
            HookKind::Notification => "Notification",
            HookKind::PreCompact => "PreCompact",
            HookKind::Unknown(s) => s.as_str(),
        }
    }
}

/// A hook invocation delivered to [`HookCallback::handle_hook`].
#[derive(Debug, Clone, Copy)]
pub struct HookEvent<'a> {
    pub kind_name: &'a str,
    /// Raw JSON string of the hook's `input` object. Callers that
    /// need structured access should parse this themselves.
    pub payload_json: &'a str,
    /// Matching tool-use id when the hook is tool-scoped
    /// (`PreToolUse`, `PostToolUse`).
    pub tool_use_id: Option<&'a str>,
    /// Opaque hook identifier the CLI assigned. Useful for correlating
    /// a registered hook to its fired event.
    pub callback_id: &'a str,
}

impl<'a> HookEvent<'a> {
    pub fn kind(&self) -> HookKind {
        HookKind::from_name(self.kind_name)
    }
}

/// Reply a [`HookCallback`] returns for a hook invocation.
#[derive(Debug, Clone, Default)]
pub struct HookResponse {
    /// Whether the CLI should continue the current action. `true`
    /// (default) lets execution proceed; `false` halts the action
    /// with `decision` providing the reason.
    pub r#continue: bool,
    /// Optional decision overlay. `None` falls back to the default
    /// per-hook behaviour.
    pub decision: Option<HookDecision>,
    /// Opaque hook-specific output (arbitrary JSON) forwarded to the
    /// CLI. Used by e.g. `UserPromptSubmit` to inject context.
    pub hook_specific_output_json: Option<String>,
}

impl HookResponse {
    /// Default: allow the action to continue with no overlay.
    pub fn r#continue() -> Self {
        Self {
            r#continue: true,
            decision: None,
            hook_specific_output_json: None,
        }
    }

    /// Shortcut: block with a reason.
    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            r#continue: false,
            decision: Some(HookDecision::Block {
                reason: reason.into(),
            }),
            hook_specific_output_json: None,
        }
    }
}

/// Decision a hook can return to override default CLI behaviour.
#[derive(Debug, Clone)]
pub enum HookDecision {
    /// Approve the action (optionally with an advisory reason).
    Allow { reason: Option<String> },
    /// Block the action; the reason is surfaced to the CLI.
    Block { reason: String },
    /// Modify the action's input (the CLI re-executes with the new
    /// input). `updated_input_json` is the JSON-string representation
    /// of the replacement input object.
    Modify { updated_input_json: String },
}
