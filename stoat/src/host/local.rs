//! Local implementations that call directly into the OS via [`tokio::fs`]
//! and libgit2.

pub mod clipboard;
pub mod git;

pub use clipboard::LocalClipboard;
pub use git::LocalGit;
