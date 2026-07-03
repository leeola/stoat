//! Local implementations that call directly into the OS via [`tokio::fs`]
//! and libgit2.

pub mod clipboard;
pub mod git;
pub mod lsp;
pub mod terminal;

pub use clipboard::LocalClipboard;
pub use git::LocalGit;
pub use lsp::LocalLsp;
pub use terminal::LocalTerminalHost;
