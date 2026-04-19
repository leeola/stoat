use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Status of a diff region at the line level. Rendered by the display
/// layer; included here because both the working-tree git path and the
/// in-memory diff map produce values of this type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiffStatus {
    #[default]
    Unchanged,
    Added,
    Modified,
    /// Line participates in a [`crate::diff_map::DiffHunkStatus::Moved`]
    /// hunk: byte-for-byte equal content that relocated to or from
    /// another position (possibly across files in the same changeset).
    Moved,
}

/// One changed path in a repository's working tree or index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    /// Absolute path on disk. Consumers that need a path relative to the
    /// workdir must strip it themselves; `GitHost` deliberately does not
    /// carry a distinguished workdir so the same type can describe files
    /// from different repos.
    pub path: PathBuf,
    /// True when the change is present in the index (staged), false when
    /// it only exists in the working tree.
    pub staged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitApplyError {
    /// Backend (libgit2, ssh, fake) surfaced a failure. Message is
    /// human-readable and free of secrets.
    Backend(String),
}

/// Metadata for a single commit, populated by [`GitRepo::log_commits`].
///
/// Pre-computed fields (`short_sha`) exist so the UI can paint each row
/// without reformatting on every redraw; the log view repaints at every
/// keystroke while the user scrolls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub sha: String,
    pub short_sha: String,
    /// First line of the commit message, trimmed. May be empty for
    /// commits without a message (pathological, but observed in the wild).
    pub summary: String,
    pub author_name: String,
    pub author_email: String,
    /// Author time as unix epoch seconds. Consumers format for display.
    pub time: i64,
    pub parent_count: u32,
}

/// How a single path changed between a commit and its parent, as
/// surfaced by [`GitRepo::commit_file_changes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitFileChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChange,
}

/// One path touched by a commit, plus its line-count summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitFileChange {
    pub rel_path: PathBuf,
    pub kind: CommitFileChangeKind,
    pub additions: u32,
    pub deletions: u32,
}

/// Discovers repositories. Kept separate from [`GitRepo`] so the host
/// can be a cheap cloneable value (`Arc<dyn GitHost>`) while repository
/// handles carry per-repo state.
pub trait GitHost: Send + Sync {
    /// Walk up from `path` looking for a repository root. Returns a
    /// shared handle on success.
    fn discover(&self, path: &Path) -> Option<Arc<dyn GitRepo>>;
}

/// Read/write interface against a single repository. Operations are
/// synchronous because git2 is synchronous and the handlers that call
/// this trait are driven by the keyboard event loop; no realistic async
/// git API exists that would benefit consumers here.
pub trait GitRepo: Send + Sync {
    fn workdir(&self) -> Option<PathBuf>;
    fn changed_files(&self) -> Vec<ChangedFile>;
    /// Read the UTF-8 content of `path` as it appears in HEAD. Returns
    /// `None` for orphan branches, paths not in HEAD, or binary blobs.
    fn head_content(&self, path: &Path) -> Option<String>;
    /// Apply a unified-diff patch to the index. Most callers will want
    /// to drive this through the review-apply flow rather than directly.
    fn apply_to_index(&self, patch: &str) -> Result<(), GitApplyError>;

    /// Read the full tree at `sha` as a map of repo-relative path to
    /// UTF-8 content. Returns `None` when the sha is unknown or any
    /// entry is not valid UTF-8. Used by commit and commit-range review.
    fn commit_tree(&self, sha: &str) -> Option<BTreeMap<PathBuf, String>>;
    /// Sha of the first parent of `sha`, or `None` for a root commit or
    /// an unknown sha. Merge commits surface only the first parent;
    /// `CommitRange` review should be used for multi-parent walks.
    fn parent_sha(&self, sha: &str) -> Option<String>;

    /// Walk first-parent history starting immediately after `after`
    /// (exclusive; `None` starts at HEAD) and return up to `limit`
    /// commits, newest first. Used to paginate the commit-list view:
    /// the caller requests just enough rows to fill the viewport plus
    /// a small prefetch window, then walks on demand as the user
    /// scrolls. Empty on orphan branches or when `after` is unknown.
    fn log_commits(&self, after: Option<&str>, limit: usize) -> Vec<CommitInfo>;

    /// Per-file summary of what changed between `sha` and its first
    /// parent (empty tree for a root commit). Lighter than building a
    /// full review: the left pane of the commit list renders these
    /// stats while the heavier hunk-level preview loads in the
    /// background. Empty when the sha is unknown.
    fn commit_file_changes(&self, sha: &str) -> Vec<CommitFileChange>;
}
