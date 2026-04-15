use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use slotmap::{new_key_type, SlotMap};
use std::{io, path::PathBuf, sync::Arc};

/// Host-provided callback for interactive tool-permission prompts.
///
/// When a [`ClaudeCodeSession`] is built with a registered callback, the
/// underlying wrapper asks the `claude` CLI to route permission prompts
/// over the control protocol (`--permission-prompt-tool-name stdio`).
/// Each incoming `can_use_tool` control request is forwarded here; the
/// returned [`PermissionResult`] becomes the control response.
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

/// Host-provided callback for CLI hook events.
///
/// When a [`ClaudeCodeSession`] is built with `include_hook_events`, or
/// a user registers a [`HookCallback`] via the builder, the CLI's
/// `hook_callback` control requests are routed here instead of being
/// replied with a no-op. The callback receives a [`HookEvent`] with
/// the raw JSON payload and returns a [`HookResponse`] the dispatcher
/// serializes back to the CLI.
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

// =====================================================================
// Shared data types.
//
// These types describe the *shape* of tool calls, plans, and usage
// data. They live here (in `stoat::host`) rather than in
// `agent/claude_code` because both sides of the trait boundary need to
// reference them, and the agent crate already depends on this one.
// =====================================================================

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

// =====================================================================
// Session-state sub-enums
// =====================================================================

/// Grouped transient session-state events. These arrive from the CLI
/// when the appropriate opt-in flags
/// (`CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS`, etc.) are set. Grouping
/// them under one top-level `AgentMessage` variant keeps consumer match
/// statements short.
#[derive(Debug, Clone)]
pub enum SessionStateEvent {
    /// `state` value from a `session_state_changed` frame (`idle` /
    /// `busy` / `compacting` / ...).
    StateChanged { state: String },
    /// Free-form status text (e.g. the CLI emitting "compacting").
    Status { text: String },
    /// Compaction pass just finished.
    CompactBoundary {
        trigger: Option<String>,
        pre_tokens: Option<u64>,
        post_tokens: Option<u64>,
    },
    /// Output from a local-only slash command.
    LocalCommandOutput { text: String },
    /// API call is being retried.
    ApiRetry {
        attempt: Option<u32>,
        reason: Option<String>,
    },
}

/// Subagent / Task lifecycle events.
#[derive(Debug, Clone)]
pub enum TaskEvent {
    Started {
        task_id: Option<String>,
        parent_tool_use_id: Option<String>,
        title: Option<String>,
    },
    Notification {
        task_id: Option<String>,
        text: String,
    },
    Progress {
        task_id: Option<String>,
    },
    Updated {
        task_id: Option<String>,
    },
}

/// Hook-lifecycle events. These are emitted by the CLI when the
/// session is started with `--include-hook-events`. The raw JSON
/// payload is preserved so hosts can inspect per-hook details without
/// this crate needing to model every hook's shape.
#[derive(Debug, Clone)]
pub enum HookLifecycleEvent {
    Started {
        hook_event_name: Option<String>,
        payload_json: String,
    },
    Progress {
        hook_event_name: Option<String>,
        payload_json: String,
    },
    Response {
        hook_event_name: Option<String>,
        payload_json: String,
    },
}

// =====================================================================
// AgentMessage: top-level host-facing event
// =====================================================================

#[derive(Debug, Clone)]
pub enum AgentMessage {
    /// Initial `system(init)` frame decoded into the session context.
    Init {
        session_id: String,
        model: String,
        tools: Vec<String>,
    },
    Text {
        text: String,
    },
    /// Incremental text delta surfaced when the session is configured
    /// with `--include-partial-messages`. A normal `Text` message always
    /// follows once the stream block completes, so consumers can choose
    /// to display deltas live or ignore them and wait for the finalized
    /// `Text` message.
    PartialText {
        text: String,
    },
    Thinking {
        text: String,
        signature: String,
    },
    /// Tool invocation enriched with classifier output. `input` retains
    /// the original JSON-stringified tool input so callers that need
    /// raw access are not forced to re-serialise. `kind`, `title`,
    /// `content`, and `locations` carry the display decoration.
    ToolUse {
        id: String,
        name: String,
        input: String,
        kind: ToolKind,
        title: String,
        content: Vec<ToolCallContent>,
        locations: Vec<ToolCallLocation>,
    },
    /// Tool result. `content` is the raw text body; `status` says
    /// whether the execution succeeded or failed.
    ToolResult {
        id: String,
        content: String,
        status: ToolCallStatus,
        kind: ToolKind,
        terminal_meta: Option<TerminalMeta>,
    },
    /// Refinement of a previously-emitted tool call. Produced after a
    /// PostToolUse hook (for richer Edit diffs) or after streaming
    /// `input_json_delta` chunks resolve the tool arguments. The UI
    /// should merge this into the existing tool call by id.
    ToolUpdate {
        id: String,
        content: Vec<ToolCallContent>,
        status: ToolCallStatus,
    },
    /// Partial `tool_use.input` chunk from the streaming channel.
    /// Consumers that render tool calls live can concatenate these
    /// chunks before the full assistant message arrives.
    PartialToolInput {
        id: String,
        json_delta: String,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: String,
    },
    ServerToolResult {
        id: String,
        content: String,
    },
    /// Plan/checklist snapshot extracted from a TodoWrite tool call.
    /// Fully replaces any prior plan; consumers should not merge.
    Plan {
        entries: Vec<PlanEntry>,
    },
    /// Token usage snapshot. `accumulated` is the running per-session
    /// total; `last` is the delta attributable to the most recent turn.
    Usage {
        accumulated: TokenUsage,
        last: TokenUsage,
    },
    /// Permission mode changed mid-session (e.g. via a `set_mode`
    /// permission suggestion or an EnterPlanMode hook).
    ModeChanged {
        mode: String,
    },
    /// Active model switched mid-session.
    ModelChanged {
        model: String,
    },
    /// Files that the CLI just persisted.
    FilesPersisted {
        paths: Vec<PathBuf>,
    },
    /// Elicitation dialog (interactive prompt to the host) completed.
    ElicitationComplete {
        id: String,
        outcome_json: String,
    },
    /// Authentication is required to continue (e.g. subscription
    /// expired). Hosts should surface a login flow.
    AuthRequired {
        reason: String,
    },
    /// Grouped session-state events (state transitions, compact
    /// boundaries, retries, local-command output).
    SessionState(SessionStateEvent),
    /// Grouped subagent/task lifecycle events.
    TaskEvent(TaskEvent),
    /// Grouped hook lifecycle events.
    Hook(HookLifecycleEvent),
    Result {
        cost_usd: f64,
        duration_ms: u64,
        num_turns: u32,
    },
    Error {
        message: String,
    },
    /// Content block with an unrecognized `type` tag. Carries the raw
    /// JSON so consumers can surface or persist it without this crate
    /// needing to understand the schema.
    Unknown {
        raw: String,
    },
}

/// Per-session I/O handle for an active Claude Code conversation.
#[async_trait]
pub trait ClaudeCodeSession: Send + Sync {
    async fn send(&self, content: &str) -> io::Result<()>;
    async fn recv(&self) -> Option<AgentMessage>;
    fn is_alive(&self) -> bool;
    async fn shutdown(&self) -> io::Result<()>;

    /// Interrupt the current turn via the CLI's control protocol. The
    /// default implementation returns an error so existing fake
    /// sessions (which don't model control traffic) don't have to
    /// implement it. Real sessions override.
    async fn interrupt(&self) -> io::Result<()> {
        Err(io::Error::other(
            "interrupt not supported by this ClaudeCodeSession implementation",
        ))
    }

    /// Request a model switch mid-session. Default `Err`.
    async fn set_model(&self, _model_id: &str) -> io::Result<()> {
        Err(io::Error::other(
            "set_model not supported by this ClaudeCodeSession implementation",
        ))
    }

    /// Request a permission-mode switch mid-session. Default `Err`.
    async fn set_permission_mode(&self, _mode: &str) -> io::Result<()> {
        Err(io::Error::other(
            "set_permission_mode not supported by this ClaudeCodeSession implementation",
        ))
    }
}

/// Session manager that creates new [`ClaudeCodeSession`] instances.
///
/// Production uses a launcher that spawns Claude CLI subprocesses.
/// Tests use a fake that returns pre-configured sessions.
#[async_trait]
pub trait ClaudeCodeHost: Send + Sync {
    async fn new_session(&self) -> io::Result<Box<dyn ClaudeCodeSession>>;

    /// Resume a prior session by id. Default implementation forwards
    /// to [`new_session`] so hosts that don't persist sessions still
    /// satisfy the trait.
    async fn resume_session(&self, _session_id: &str) -> io::Result<Box<dyn ClaudeCodeSession>> {
        self.new_session().await
    }

    /// Load a prior session and replay its history. Default
    /// implementation forwards to [`new_session`].
    async fn load_session(&self, _session_id: &str) -> io::Result<Box<dyn ClaudeCodeSession>> {
        self.new_session().await
    }

    /// Fork an existing session under a new id. Default forwards to
    /// [`new_session`] (i.e. treats fork as "new session, no shared
    /// history").
    async fn fork_session(
        &self,
        _parent_session_id: &str,
    ) -> io::Result<Box<dyn ClaudeCodeSession>> {
        self.new_session().await
    }

    /// List sessions known to this host. Default: empty list.
    async fn list_sessions(&self) -> io::Result<Vec<ClaudeSessionSummary>> {
        Ok(Vec::new())
    }
}

/// Summary of a session surfaced by [`ClaudeCodeHost::list_sessions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeSessionSummary {
    pub session_id: String,
    pub cwd: String,
    pub title: String,
    pub updated_at: String,
}

new_key_type! {
    pub struct ClaudeSessionId;
}

pub enum ClaudeNotification {
    CreateRequested {
        session_id: ClaudeSessionId,
    },
    SessionReady {
        session_id: ClaudeSessionId,
        session: Box<dyn ClaudeCodeSession>,
    },
    SessionError {
        session_id: ClaudeSessionId,
        error: String,
    },
    Message {
        session_id: ClaudeSessionId,
        message: AgentMessage,
    },
}

/// Owns zero or more active Claude Code sessions.
pub struct ClaudeCodeSessions {
    sessions: SlotMap<ClaudeSessionId, Option<Arc<dyn ClaudeCodeSession>>>,
}

impl ClaudeCodeSessions {
    pub fn new() -> Self {
        Self {
            sessions: SlotMap::with_key(),
        }
    }

    pub fn reserve_slot(&mut self) -> ClaudeSessionId {
        self.sessions.insert(None)
    }

    pub fn fill_slot(&mut self, id: ClaudeSessionId, session: Arc<dyn ClaudeCodeSession>) {
        if let Some(slot) = self.sessions.get_mut(id) {
            *slot = Some(session);
        }
    }

    pub fn get(&self, id: ClaudeSessionId) -> Option<&Arc<dyn ClaudeCodeSession>> {
        self.sessions.get(id).and_then(|s| s.as_ref())
    }

    pub fn remove(&mut self, id: ClaudeSessionId) -> Option<Arc<dyn ClaudeCodeSession>> {
        self.sessions.remove(id).flatten()
    }

    pub fn ids(&self) -> impl Iterator<Item = ClaudeSessionId> + '_ {
        self.sessions.keys()
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

impl Default for ClaudeCodeSessions {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::{FakeClaudeCode, FakeClaudeCodeHost};
    use async_trait::async_trait;

    #[test]
    fn reserve_fill_get_remove() {
        let mut sessions = ClaudeCodeSessions::new();

        let id_a = sessions.reserve_slot();
        let id_b = sessions.reserve_slot();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.get(id_a).is_none(), "unfilled slot returns None");

        let fake_a: Arc<dyn ClaudeCodeSession> = Arc::new(FakeClaudeCode::new());
        let fake_b: Arc<dyn ClaudeCodeSession> = Arc::new(FakeClaudeCode::new());
        sessions.fill_slot(id_a, fake_a);
        sessions.fill_slot(id_b, fake_b);

        assert!(sessions.get(id_a).is_some());
        assert!(sessions.get(id_b).is_some());

        let removed = sessions.remove(id_a).expect("session present");
        assert!(removed.is_alive());
        assert!(sessions.get(id_a).is_none());
        assert_eq!(sessions.len(), 1);

        let remaining: Vec<_> = sessions.ids().collect();
        assert_eq!(remaining, vec![id_b]);
    }

    // ---- Default trait method behavior ----

    struct MinimalSession;
    #[async_trait]
    impl ClaudeCodeSession for MinimalSession {
        async fn send(&self, _content: &str) -> io::Result<()> {
            Ok(())
        }
        async fn recv(&self) -> Option<AgentMessage> {
            None
        }
        fn is_alive(&self) -> bool {
            true
        }
        async fn shutdown(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn claude_code_session_defaults_return_err() {
        let s = MinimalSession;
        assert!(s.interrupt().await.is_err());
        assert!(s.set_model("opus").await.is_err());
        assert!(s.set_permission_mode("plan").await.is_err());
    }

    #[tokio::test]
    async fn claude_code_host_default_fallbacks_forward_to_new_session() {
        let host = FakeClaudeCodeHost::new();
        host.push_session(FakeClaudeCode::new());
        host.push_session(FakeClaudeCode::new());
        host.push_session(FakeClaudeCode::new());
        let r = host.resume_session("id").await;
        assert!(r.is_ok());
        let l = host.load_session("id").await;
        assert!(l.is_ok());
        let f = host.fork_session("parent").await;
        assert!(f.is_ok());
    }

    #[tokio::test]
    async fn claude_code_host_list_sessions_default_is_empty() {
        let host = FakeClaudeCodeHost::new();
        let out = host.list_sessions().await.unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn hook_kind_round_trips_known_names() {
        let names = [
            "PreToolUse",
            "PostToolUse",
            "UserPromptSubmit",
            "Stop",
            "SubagentStop",
            "SessionStart",
            "SessionEnd",
            "Notification",
            "PreCompact",
        ];
        for name in names {
            let kind = HookKind::from_name(name);
            assert_eq!(kind.as_name(), name);
        }
        let unknown = HookKind::from_name("FutureHook");
        assert_eq!(unknown.as_name(), "FutureHook");
        assert!(matches!(unknown, HookKind::Unknown(_)));
    }

    #[test]
    fn hook_response_helpers_round_trip() {
        let cont = HookResponse::r#continue();
        assert!(cont.r#continue);
        assert!(cont.decision.is_none());

        let blocked = HookResponse::block("because");
        assert!(!blocked.r#continue);
        match blocked.decision.as_ref().unwrap() {
            HookDecision::Block { reason } => assert_eq!(reason, "because"),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn permission_result_convenience_constructors() {
        let a = PermissionResult::allow();
        assert!(matches!(
            a,
            PermissionResult::Allow {
                scope: PermissionScope::Once,
                ..
            }
        ));
        let a2 = PermissionResult::allow_with_scope(PermissionScope::Always);
        assert!(matches!(
            a2,
            PermissionResult::Allow {
                scope: PermissionScope::Always,
                ..
            }
        ));
        let d = PermissionResult::deny("no");
        match d {
            PermissionResult::Deny { message, interrupt } => {
                assert_eq!(message, "no");
                assert!(!interrupt);
            },
            other => panic!("got {other:?}"),
        }
        assert!(matches!(
            PermissionResult::cancel(),
            PermissionResult::Cancel
        ));
    }

    #[test]
    fn tool_permission_context_bare_has_sensible_defaults() {
        let ctx = ToolPermissionContext::bare();
        assert!(ctx.suggestions_json.is_none());
        assert!(ctx.tool_use_id.is_none());
        assert!(ctx.tool_title.is_empty());
        assert!(matches!(ctx.tool_kind, ToolKind::Other));
        assert!(ctx.tool_content.is_empty());
        assert!(ctx.tool_locations.is_empty());
    }
}
