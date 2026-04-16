//! [`AgentMessage`]: the top-level host-facing event emitted by a
//! Claude Code session for every decoded assistant, tool, system, or
//! error frame.

use super::{
    events::{HookLifecycleEvent, SessionStateEvent, TaskEvent},
    types::{
        PlanEntry, TerminalMeta, TokenUsage, ToolCallContent, ToolCallLocation, ToolCallStatus,
        ToolKind,
    },
};
use std::path::PathBuf;

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
