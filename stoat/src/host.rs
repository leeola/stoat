//! Trait-per-concern interfaces for IO operations.
//!
//! Local implementations ([`local`]) wrap syscalls directly; future remote
//! implementations will serialize over a network channel.
//!
//! API design principles:
//! - Caller-owned buffers for reuse (`&mut Vec<u8>` reads)
//! - Borrowed paths (`&Path` inputs)
//! - Small `Copy` return types for metadata

pub mod clipboard;
#[cfg(test)]
pub mod fake;
pub mod git;
pub mod local;
pub mod lsp;
pub mod terminal;

pub use clipboard::{osc52_should_emit, ClipboardHost, NoopClipboard};
#[cfg(test)]
pub use fake::{
    change_params, completion_params, definition_params, document_highlight_params, hover_params,
    inlay_hint_params, open_params, reference_params,
    terminal::{inject_done, inject_output, FakeTerminal},
    workspace_symbol_params, FakeClipboard, FakeEnv, FakeFs, FakeFsOp, FakeFsWatcher, FakeGit,
    FakeLsp, FakeRepoBuilder,
};
pub use git::{
    ChangedFile, CherryPickOutcome, CommitFileChange, CommitFileChangeKind, CommitInfo,
    ConflictedFile, DiffStatus, GitApplyError, GitHost, GitRepo, RebaseError, RebaseTodo,
    RebaseTodoOp, RewriteResult,
};
pub use local::{LocalClipboard, LocalGit};
pub use lsp::{LanguageServerFeature, LspHost, LspNotification, NoopLsp, OffsetEncoding};
#[cfg(test)]
pub use stoat_host::FakeShell;
pub use stoat_host::{
    EnvHost, FsDirEntry, FsEventKind, FsHost, FsMetadata, FsWatchEvent, FsWatchHost, LocalEnv,
    LocalFs, LocalFsWatcher, LocalShell, NoopFsWatcher, ShellHost, ShellOutput, WatchToken,
};
pub use terminal::TerminalHost;
