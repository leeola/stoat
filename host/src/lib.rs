//! Foundational `*Host` traits + production / fake impls shared by
//! `stoat` and `viewport`. Crates that need to route IO through a
//! host abstraction (instead of calling `std::env` / `std::fs`
//! directly) depend on this crate.
//!
//! API design principles:
//! - Caller-owned buffers for reuse (`&mut Vec<u8>` reads)
//! - Borrowed paths (`&Path` inputs)
//! - Small `Copy` return types for metadata

pub mod env;
pub mod fake;
pub mod fs;

pub use env::{EnvHost, LocalEnv};
pub use fake::{FakeEnv, FakeFs, FakeFsOp};
pub use fs::{FsDirEntry, FsHost, FsMetadata, LocalFs};
