//! Session-state, task, and hook lifecycle event enums surfaced as
//! variants of [`super::AgentMessage`].

/// Grouped transient session-state events. These arrive from the CLI
/// when the appropriate opt-in flags
/// (`CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS`, etc.) are set. Grouping
/// them under one top-level [`super::AgentMessage`] variant keeps
/// consumer match statements short.
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
