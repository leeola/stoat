//! Local implementations that call directly into the OS via [`tokio::fs`].

pub mod fs;

pub use fs::LocalFs;
