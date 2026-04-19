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

    /// Replace HEAD's tree (and optionally its message) with the given
    /// values, creating a new commit and updating HEAD to point at it.
    /// Parents, author, and committer carry over; signatures are
    /// stripped; hooks are not invoked. Returns the new HEAD sha.
    ///
    /// Fails when HEAD is orphan, the commit cannot be built, or the
    /// backend rejects the write.
    fn amend_head(
        &self,
        tree: &BTreeMap<PathBuf, String>,
        message: Option<&str>,
    ) -> Result<String, GitApplyError>;

    /// Replace the commit at `sha` with one carrying `tree` (and
    /// optionally a new `message`), then cherry-pick each entry in
    /// `descendants` onto the rewritten commit in order (oldest first,
    /// newest last). Returns the new HEAD sha plus a mapping from each
    /// old sha (the target `sha` and every descendant) to its new sha.
    ///
    /// Descendants should be the commits strictly between `sha` and
    /// the current HEAD, oldest first. A cherry-pick conflict at any
    /// step aborts the operation with `GitApplyError::Backend`; the
    /// repo is left untouched.
    fn rewrite_commit(
        &self,
        sha: &str,
        tree: &BTreeMap<PathBuf, String>,
        message: Option<&str>,
        descendants: &[String],
    ) -> Result<RewriteResult, GitApplyError>;

    /// Execute a rebase plan. `onto` is the base commit the plan
    /// stacks on top of; `todo` is the oldest-first list of operations
    /// to apply. Returns the new HEAD sha on success.
    ///
    /// Implementations must make the whole plan atomic: a conflict at
    /// any step aborts the rebase with `RebaseError::Conflict { at_sha }`
    /// and leaves the repo untouched.
    fn run_rebase(&self, onto: &str, todo: &[RebaseTodo]) -> Result<String, RebaseError>;

    /// Attempt to apply the changes introduced by `source_sha` on top
    /// of `onto_sha`. Returns the resulting tree plus the source's
    /// metadata on a clean 3-way merge, or a list of conflicted files
    /// with per-stage content on conflict. Does not create any commits
    /// or update any refs; the caller is responsible for driving the
    /// rebase state machine.
    fn cherry_pick_tree(
        &self,
        source_sha: &str,
        onto_sha: &str,
    ) -> Result<CherryPickOutcome, GitApplyError>;

    /// Create a commit with the given parent, tree, message, and
    /// author identity. Committer is set to the configured identity
    /// (or the same as author when not configured). Returns the new
    /// commit's sha; does not update HEAD.
    fn create_commit(
        &self,
        parent_sha: Option<&str>,
        tree: &BTreeMap<PathBuf, String>,
        message: &str,
        author_name: &str,
        author_email: &str,
    ) -> Result<String, GitApplyError>;

    /// Point HEAD at `sha` (detached update; does not move any branch
    /// refs). Used by the rebase stepper after the plan completes.
    fn update_head(&self, sha: &str) -> Result<(), GitApplyError>;
}

/// Result of a [`GitRepo::rewrite_commit`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteResult {
    /// Sha of the new HEAD after the rewrite + cherry-pick chain.
    pub new_head: String,
    /// Map from the original sha (target + each descendant) to the
    /// new sha it became. Callers that were pointing at an original
    /// sha (e.g. a review session's `ReviewSource::Commit`) read this
    /// to relocate.
    pub mapping: std::collections::HashMap<String, String>,
}

/// Single entry in a rebase plan, mirroring the commands accepted by
/// `git rebase -i` (minus reword/edit in v1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebaseTodo {
    pub op: RebaseTodoOp,
    /// Sha of the commit this entry refers to in the pre-rebase
    /// history.
    pub sha: String,
    /// Message from the pre-rebase commit, carried on the entry so
    /// implementations (and the fake) can synthesize combined messages
    /// without re-reading the original commit.
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebaseTodoOp {
    Pick,
    Squash,
    Fixup,
    Drop,
    /// Apply the commit, then pause so the user can supply a new
    /// message. Implementations that cannot pause (fake test paths,
    /// `run_rebase`) should treat this as `Pick` and the caller is
    /// responsible for driving a stepper that handles the pause.
    Reword,
    /// Apply the commit, then pause so the user can modify the
    /// resulting commit (via review-mode hunk removal, etc.) before
    /// continuing.
    Edit,
}

/// Result of applying one commit's diff onto another via 3-way merge.
/// Surfaces either a clean tree ready to be committed, or a set of
/// conflicted files with each stage's content for the UI to resolve.
#[derive(Clone, Debug)]
pub enum CherryPickOutcome {
    Clean {
        tree: BTreeMap<PathBuf, String>,
        /// Commit message carried from the source commit.
        message: String,
        author_name: String,
        author_email: String,
        author_time: i64,
    },
    Conflict {
        files: Vec<ConflictedFile>,
    },
}

/// Per-file 3-way merge state when a cherry-pick produces conflicts.
#[derive(Clone, Debug)]
pub struct ConflictedFile {
    pub path: PathBuf,
    /// Content at the common ancestor. `None` when the file did not
    /// exist at that point (pure addition on one side).
    pub ancestor: Option<String>,
    /// Content on the "ours" side (the rebase-so-far HEAD).
    pub ours: Option<String>,
    /// Content on the "theirs" side (the commit being applied).
    pub theirs: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebaseError {
    Backend(String),
    Conflict { at_sha: String },
    DirtyWorktree,
}
