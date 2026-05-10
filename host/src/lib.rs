//! Foundational `*Host` traits + production / fake impls shared
//! across stoat crates. Crates that need to route IO through a
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
pub mod shell;
pub mod watch;

pub use env::{EnvHost, LocalEnv};
pub use fake::{FakeEnv, FakeFs, FakeFsOp, FakeFsWatcher, FakeShell, FakeShellInvocation};
pub use fs::{FsDirEntry, FsHost, FsMetadata, LocalFs};
pub use shell::{LocalShell, ShellHost, ShellOutput};
pub use watch::{
    FsEventKind, FsWatchEvent, FsWatchHost, LocalFsWatcher, NoopFsWatcher, WatchToken,
};
