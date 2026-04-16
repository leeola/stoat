//! Local implementations that call directly into the OS via [`tokio::fs`]
//! and libgit2.

pub mod fs;
pub mod git;

pub use fs::LocalFs;
pub use git::LocalGit;
