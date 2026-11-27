//! Trait-per-concern interfaces for IO operations.
//!
//! Local implementations wrap syscalls directly; future remote
//! implementations will serialize over a network channel.
//!
//! API design principles:
//! - Caller-owned buffers for reuse (`&mut Vec<u8>` reads)
//! - Borrowed paths (`&Path` inputs)
//! - Small `Copy` return types for metadata

#[cfg(test)]
pub use clipboard::FakeClipboard;
pub use clipboard::{
    clipboard_copy, osc52_should_emit, ClipboardHost, ClipboardKind, LocalClipboard, NoopClipboard,
};
pub use git::{
    BlameCommit, BlameRun, ChangedFile, CherryPickOutcome, CommitFileChange, CommitFileChangeKind,
    CommitInfo, ConflictedFile, FileBlame, GitApplyError, GitHost, GitRepo, LocalGit, RebaseError,
    RebaseTodo, RebaseTodoOp, RewriteResult, StagedKind,
};
#[cfg(test)]
pub use git::{FakeGit, FakeGitRepo, FakeRepoBuilder};
#[cfg(test)]
pub use lsp::{
    change_params, completion_params, definition_params, document_highlight_params, hover_params,
    inlay_hint_params, open_params, reference_params, workspace_symbol_params, FakeLsp,
};
pub use lsp::{
    LanguageServerFeature, LocalLsp, LspHost, LspNotification, LspTranscript, NoopLsp,
    OffsetEncoding,
};
pub use stoat_host::{
    EnvHost, FsDirEntry, FsEventKind, FsHost, FsMetadata, FsWatchEvent, FsWatchHost, LocalEnv,
    LocalFs, LocalFsWatcher, LocalShell, NoopFsWatcher, ShellHost, ShellOutput, WatchToken,
};
#[cfg(test)]
pub use stoat_host::{FakeEnv, FakeFs, FakeFsOp, FakeFsWatcher, FakeShell};
pub use stoat_host_clipboard as clipboard;
pub use stoat_host_git as git;
pub use stoat_host_lsp as lsp;
pub use stoat_host_terminal as terminal;
#[cfg(test)]
pub use terminal::{FakeTerminalHost, FakeTerminalSession};
pub use terminal::{LocalTerminalHost, SpawnArgs, TerminalHost, TerminalSession};
