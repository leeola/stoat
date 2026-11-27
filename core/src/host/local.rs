//! Local implementations that call directly into the OS via [`tokio::fs`].

pub mod clipboard;

pub use clipboard::LocalClipboard;
