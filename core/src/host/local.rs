//! Local implementations that call directly into the OS via [`tokio::fs`].

pub mod clipboard;
pub mod lsp;
pub mod terminal;

pub use clipboard::LocalClipboard;
pub use lsp::{LocalLsp, LspTranscript};
pub use terminal::LocalTerminalHost;
