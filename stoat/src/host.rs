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
pub mod local;
pub mod lsp;

pub use claude_code::{AgentMessage, ClaudeCodeHost};
#[cfg(test)]
pub use fake::{
    change_params, completion_params, definition_params, document_highlight_params, hover_params,
    inlay_hint_params, open_params, reference_params, workspace_symbol_params, FakeClaudeCode,
    FakeLsp,
};
pub use fs::{FsDirEntry, FsHost, FsMetadata};
pub use local::LocalFs;
pub use lsp::{LspHost, LspNotification};
