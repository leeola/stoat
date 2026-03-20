//! Trait-per-concern interfaces for IO operations.
//!
//! Local implementations ([`local`]) wrap syscalls directly; future remote
//! implementations will serialize over a network channel.
//!
//! API design principles:
//! - Caller-owned buffers for reuse (`&mut Vec<u8>` reads)
//! - Borrowed paths (`&Path` inputs)
//! - Small `Copy` return types for metadata

pub mod fs;
pub mod local;

pub use fs::{FsDirEntry, FsHost, FsMetadata};
pub use local::LocalFs;
