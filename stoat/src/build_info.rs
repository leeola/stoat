// Build information captured at compile time.
//
//! Build-time information about the stoat binary.
//!
//! This module provides access to git commit information that was captured
//! during the build process via the build script. The information includes
//! the commit hash and whether the build had uncommitted changes (dirty).

/// Build information captured at compile time.
///
/// Contains git commit information that was captured when the binary was built.
/// This information is embedded into the binary at compile time by the build script.
#[derive(Debug, Clone, Copy)]
pub struct BuildInfo {
    /// Short git commit hash (7 characters).
    ///
    /// This is the abbreviated hash of the git commit that was checked out
    /// when the binary was built. If the build was not in a git repository,
    /// this will be "unknown".
    pub commit_hash: &'static str,

    /// Whether the build had uncommitted changes.
    ///
    /// `true` if there were uncommitted changes (either staged or unstaged)
    /// in the working tree when the binary was built. `false` if the working
    /// tree was clean. Always `false` if not built in a git repository.
    pub dirty: bool,
}

impl BuildInfo {
    /// Get a display string for the build info.
    ///
    /// Returns a formatted string suitable for display in UIs or logs.
    /// Examples:
    /// - Clean build: "abc1234"
    /// - Dirty build: "abc1234 (dirty)"
    /// - Non-git build: "unknown"
    pub fn display_string(&self) -> String {
        if self.dirty {
            format!("{} (dirty)", self.commit_hash)
        } else {
            self.commit_hash.to_string()
        }
    }
}

/// Get build information for the current binary.
///
/// Returns a [`BuildInfo`] struct containing the git commit hash and dirty status
/// that were captured at build time. This information is embedded in the binary
/// by the build script.
///
/// # Example
///
/// ```
/// use stoat::build_info::build_info;
///
/// let info = build_info();
/// println!("Built from commit: {}", info.display_string());
/// ```
pub fn build_info() -> BuildInfo {
    BuildInfo {
        commit_hash: env!("STOAT_COMMIT_HASH"),
        dirty: env!("STOAT_COMMIT_DIRTY") == "true",
    }
}
