//! Trait-per-concern interfaces for IO operations.
//!
//! Local implementations ([`local`]) wrap syscalls directly; future remote
//! implementations will serialize over a network channel.
//!
//! API design principles:
//! - Caller-owned buffers for reuse (`&mut Vec<u8>` reads)
//! - Borrowed paths (`&Path` inputs)
//! - Small `Copy` return types for metadata

pub mod claude_code;
#[cfg(test)]
pub mod fake;
pub mod fs;
pub mod git;
pub mod local;
pub mod lsp;
pub mod terminal;

pub use claude_code::{
    AgentMessage, ClaudeCodeHost, ClaudeCodeSession, ClaudeCodeSessions, ClaudeNotification,
    ClaudeSessionId, ClaudeSessionSummary, HookCallback, HookDecision, HookEvent, HookKind,
    HookLifecycleEvent, HookResponse, ModeInfo, ModelInfo, PermissionBehavior, PermissionCallback,
    PermissionDestination, PermissionResult, PermissionRule, PermissionScope, PermissionSuggestion,
    PlanEntry, PlanEntryStatus, SessionStateEvent, TaskEvent, TerminalMeta, TokenUsage,
    ToolCallContent, ToolCallLocation, ToolCallStatus, ToolKind, ToolPermissionContext,
};
#[cfg(test)]
pub use fake::{
    change_params, completion_params, definition_params, document_highlight_params, hover_params,
    inlay_hint_params, open_params, reference_params,
    terminal::{inject_done, inject_output, FakeTerminal},
    workspace_symbol_params, FakeClaudeCode, FakeClaudeCodeHost, FakeFs, FakeGit, FakeLsp,
    FakeRepoBuilder,
};
pub use fs::{FsDirEntry, FsHost, FsMetadata};
pub use git::{ChangedFile, DiffStatus, GitApplyError, GitHost, GitRepo};
pub use local::{LocalFs, LocalGit};
pub use lsp::{LspHost, LspNotification};
pub use terminal::TerminalHost;
