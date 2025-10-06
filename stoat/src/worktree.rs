//! Worktree abstraction for file discovery.
//!
//! This module provides a Worktree/Snapshot pattern similar to Zed's architecture,
//! enabling instant file finder access through pre-computed file lists.
//!
//! # Architecture
//!
//! The worktree module follows Zed's design pattern:
//! - [`Worktree`] manages the file tree state and performs discovery
//! - [`Snapshot`] provides immutable access to the file list
//! - [`Entry`] represents individual file/directory metadata
//!
//! # Usage
//!
//! ```ignore
//! let worktree = Worktree::new(PathBuf::from("."));
//! let snapshot = worktree.snapshot();
//! let files = snapshot.files(false); // Fast read access, excluding ignored files
//! ```
//!
//! # Future Extension
//!
//! This architecture supports adding background file watching similar to Zed:
//! - Background scanner can run continuously
//! - Updates [`Snapshot`] atomically when FS changes detected
//! - All readers see consistent state via snapshot cloning

mod ignore;

use self::ignore::IgnoreStack;
use crate::rel_path::RelPath;
use ::ignore::gitignore::{gitconfig_excludes_path, Gitignore, GitignoreBuilder};
use fuzzy::CharBag;
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
};
use sum_tree::{Dimension, Item, KeyedItem, SumTree, Summary};

/// Worktree manages the file tree for a directory.
///
/// Similar to Zed's `LocalWorktree`, this struct owns the file discovery state
/// and provides access via immutable [`Snapshot`]s. Currently performs a single
/// initial scan on construction, but the architecture supports adding background
/// file watching in the future.
///
/// # Design
///
/// The worktree maintains a [`Snapshot`] of all discovered files. In Zed's
/// implementation, background scanner tasks continuously update this snapshot
/// based on filesystem events. For now, we perform one-time initialization.
///
/// # Usage in Stoat
///
/// The [`Worktree`] is stored in [`crate::Stoat`] as `Arc<Mutex<Worktree>>`,
/// allowing multiple clones of Stoat to share the same file list. The file finder
/// ([`crate::actions::shell::open_file_finder`]) reads from the snapshot for
/// instant file access.
///
/// # Related
///
/// See also:
/// - [`Snapshot`] - immutable file tree data
/// - [`Entry`] - individual file metadata
/// - [`crate::actions::shell::open_file_finder`] - primary consumer
pub struct Worktree {
    /// Current snapshot of the file tree
    snapshot: Snapshot,
    /// Root directory being tracked
    root: PathBuf,
}

/// Immutable snapshot of files in the worktree.
///
/// Similar to Zed's `Snapshot`, this provides read-only access to the file tree.
/// Snapshots are cheap to clone and provide a consistent view of the file tree
/// at a point in time. This enables background updates (in the future) without
/// blocking readers.
///
/// # Design
///
/// Uses [`SumTree<Entry>`] for efficient O(log n) lookups and updates. The tree
/// is indexed by [`PathKey`] and summarized by [`EntrySummary`], enabling fast
/// queries for total file count, non-ignored file count, etc.
///
/// # Usage
///
/// Obtained via [`Worktree::snapshot()`] and used by file finder for fuzzy
/// matching and filtering.
#[derive(Clone)]
pub struct Snapshot {
    /// All discovered entries in a B+ tree, indexed by path
    entries_by_path: SumTree<Entry>,
    /// Root directory of this snapshot
    root: PathBuf,
    /// CharBag for the root path (used to generate entry char_bags)
    root_char_bag: CharBag,
}

/// Metadata for a single file or directory entry.
///
/// Represents a file or directory discovered during worktree scanning. Each entry
/// tracks git ignore status, a char bag for fuzzy matching, and file size.
///
/// # SumTree Integration
///
/// Implements [`Item`] and [`KeyedItem`] for storage in [`SumTree`]. The summary
/// type is [`EntrySummary`] which aggregates file counts and supports efficient
/// querying.
///
/// # Fields
///
/// - `path` - Relative path from worktree root (used as key)
/// - `is_dir` - Whether this is a directory
/// - `is_ignored` - Whether gitignore rules exclude this entry
/// - `char_bag` - Lowercased characters from path (for fuzzy matching)
/// - `size` - File size in bytes (0 for directories)
#[derive(Clone, Debug)]
pub struct Entry {
    /// Path relative to worktree root
    pub path: Arc<RelPath>,
    /// Whether this entry is a directory
    pub is_dir: bool,
    /// Whether this entry matches gitignore rules
    pub is_ignored: bool,
    /// Character bag for fuzzy matching (lowercased path characters)
    pub char_bag: CharBag,
    /// File size in bytes (0 for directories)
    pub size: u64,
}

/// Summary type for [`Entry`] in the [`SumTree`].
///
/// Aggregates statistics about entries in a subtree, enabling efficient queries
/// like "how many non-ignored files are there?" without traversing all entries.
///
/// # Fields
///
/// - `max_path` - Maximum path in this subtree (for binary search)
/// - `count` - Total entries (files + dirs)
/// - `non_ignored_count` - Entries not matching gitignore
/// - `file_count` - Total files (excluding dirs)
/// - `non_ignored_file_count` - Non-ignored files only
#[derive(Clone, Debug)]
pub struct EntrySummary {
    max_path: Arc<RelPath>,
    count: usize,
    non_ignored_count: usize,
    file_count: usize,
    non_ignored_file_count: usize,
}

/// Key type for seeking in the entries [`SumTree`].
///
/// Wraps an `Arc<Path>` and implements [`Dimension`] so the tree can be indexed
/// by path. This allows efficient lookups like "find the entry for path X".
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PathKey(pub Arc<RelPath>);

impl Default for PathKey {
    fn default() -> Self {
        Self(RelPath::empty().into())
    }
}

impl Default for EntrySummary {
    fn default() -> Self {
        Self {
            max_path: RelPath::empty().into(),
            count: 0,
            non_ignored_count: 0,
            file_count: 0,
            non_ignored_file_count: 0,
        }
    }
}

// SumTree trait implementations

impl Item for Entry {
    type Summary = EntrySummary;

    fn summary(&self, _cx: ()) -> Self::Summary {
        let non_ignored_count = if self.is_ignored { 0 } else { 1 };
        let file_count = if self.is_dir { 0 } else { 1 };
        let non_ignored_file_count = if self.is_dir || self.is_ignored { 0 } else { 1 };

        EntrySummary {
            max_path: self.path.clone(),
            count: 1,
            non_ignored_count,
            file_count,
            non_ignored_file_count,
        }
    }
}

impl KeyedItem for Entry {
    type Key = PathKey;

    fn key(&self) -> Self::Key {
        PathKey(self.path.clone())
    }
}

impl Summary for EntrySummary {
    type Context<'a> = ();

    fn zero<'a>(_cx: Self::Context<'a>) -> Self {
        Default::default()
    }

    fn add_summary<'a>(&mut self, other: &Self, _cx: Self::Context<'a>) {
        self.max_path = other.max_path.clone();
        self.count += other.count;
        self.non_ignored_count += other.non_ignored_count;
        self.file_count += other.file_count;
        self.non_ignored_file_count += other.non_ignored_file_count;
    }
}

impl<'a> Dimension<'a, EntrySummary> for PathKey {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a EntrySummary, _cx: ()) {
        self.0 = summary.max_path.clone();
    }
}

impl Worktree {
    /// Create a new worktree by discovering files in the given root directory.
    ///
    /// Performs synchronous I/O to walk the directory tree. This should be called
    /// during initialization, not in performance-critical paths. In Zed, this is
    /// done by background scanner tasks, but we do it synchronously on creation.
    ///
    /// # Arguments
    ///
    /// * `root` - Root directory to scan
    ///
    /// # Gitignore Processing
    ///
    /// Respects gitignore rules in this order (matching git's precedence):
    /// 1. Global gitignore from git config (`core.excludesfile`)
    /// 2. Repository `.git/info/exclude` files
    /// 3. Directory `.gitignore` files (parent to child)
    ///
    /// Each directory's `.gitignore` extends parent rules, matching git's behavior.
    ///
    /// # Filters
    ///
    /// Automatically excludes:
    /// - Files matching gitignore rules
    /// - Files larger than 10MB
    /// - Directories deeper than 10 levels
    ///
    /// # Related
    ///
    /// This is called from [`crate::Stoat::new()`] during editor initialization.
    pub fn new(root: PathBuf) -> Self {
        let mut entries = Vec::new();

        // Load global gitignore from git config (core.excludesfile)
        let ignore_stack = if let Some(global_path) = gitconfig_excludes_path() {
            if let Ok(gitignore) = build_gitignore(&global_path) {
                IgnoreStack::global(Arc::new(gitignore))
            } else {
                IgnoreStack::none()
            }
        } else {
            IgnoreStack::none()
        };

        let root_char_bag = CharBag::default();

        Self::walk_directory(
            &root,
            Path::new(""),
            &mut entries,
            ignore_stack,
            root_char_bag,
            0,
        );

        // Build SumTree from collected entries
        let mut tree = SumTree::new(());
        for entry in entries {
            tree.insert_or_replace(entry, ());
        }

        let snapshot = Snapshot {
            entries_by_path: tree,
            root: root.clone(),
            root_char_bag,
        };

        Self { snapshot, root }
    }

    /// Get an immutable snapshot of the current file tree.
    ///
    /// Returns a cheap clone of the current snapshot. In Zed's implementation,
    /// this allows background updates to occur while readers access a consistent
    /// view. For now, the snapshot never changes after initialization.
    ///
    /// # Usage
    ///
    /// ```ignore
    /// let snapshot = worktree.snapshot();
    /// let files = snapshot.files(false); // Exclude ignored files
    /// ```
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }

    /// Get the root directory of this worktree.
    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Manually refresh the file list by re-scanning the filesystem.
    ///
    /// This is synchronous and will block. In Zed, refreshes happen automatically
    /// via background scanner responding to FS events. We provide this for manual
    /// updates if needed.
    ///
    /// # Future
    ///
    /// When file watching is added, this could be triggered by FS events or
    /// called manually to force a refresh.
    #[allow(dead_code)]
    pub fn refresh(&mut self) {
        let mut entries = Vec::new();
        let ignore_stack = IgnoreStack::none();
        let root_char_bag = CharBag::default();

        Self::walk_directory(
            &self.root,
            Path::new(""),
            &mut entries,
            ignore_stack,
            root_char_bag,
            0,
        );

        // Rebuild SumTree
        let mut tree = SumTree::new(());
        for entry in entries {
            tree.insert_or_replace(entry, ());
        }

        self.snapshot = Snapshot {
            entries_by_path: tree,
            root: self.root.clone(),
            root_char_bag,
        };
    }

    /// Recursively walk a directory and collect entries with gitignore support.
    ///
    /// Processes `.gitignore` files during traversal, building an [`IgnoreStack`]
    /// that respects parent/child gitignore rules like git does.
    ///
    /// # Arguments
    ///
    /// * `abs_dir` - Absolute path to directory being scanned
    /// * `rel_path` - Relative path from worktree root
    /// * `entries` - Accumulator for discovered [`Entry`] structs
    /// * `ignore_stack` - Current gitignore rule stack
    /// * `root_char_bag` - CharBag for root path
    /// * `depth` - Current recursion depth (for limiting depth)
    ///
    /// # Gitignore Processing
    ///
    /// When a `.gitignore` file is found:
    /// 1. Parse it using the `ignore` crate
    /// 2. Append to ignore_stack for this directory and descendants
    /// 3. Check all subsequent files against the updated stack
    ///
    /// # Implementation Note
    ///
    /// Unlike Zed's async BackgroundScanner, this is synchronous. Zed processes
    /// directories concurrently, handles symlinks, tracks git repositories, etc.
    /// We focus on the core: gitignore support + file discovery.
    fn walk_directory(
        abs_dir: &Path,
        rel_path: &Path,
        entries: &mut Vec<Entry>,
        mut ignore_stack: Arc<IgnoreStack>,
        root_char_bag: CharBag,
        depth: usize,
    ) {
        // Limit recursion depth to avoid infinite loops
        if depth > 10 {
            return;
        }

        let Ok(dir_entries) = std::fs::read_dir(abs_dir) else {
            return;
        };

        // Collect entries so we can process .gitignore first
        let mut children: Vec<_> = dir_entries.flatten().collect();

        // Sort to ensure .gitignore is processed before other files
        children.sort_by_key(|e| e.file_name());

        // First pass: look for .gitignore and .git/info/exclude
        let mut found_gitignore = false;
        let mut found_git = false;

        for entry in &children {
            if entry.file_name() == ".gitignore" {
                let gitignore_path = entry.path();
                if let Ok(gitignore) = build_gitignore(&gitignore_path) {
                    ignore_stack = ignore_stack.append(abs_dir.into(), Arc::new(gitignore));
                }
                found_gitignore = true;
            } else if entry.file_name() == ".git" && entry.path().is_dir() {
                // Load .git/info/exclude for repository-specific excludes
                let exclude_path = entry.path().join("info").join("exclude");
                if exclude_path.exists() {
                    if let Ok(gitignore) = build_gitignore(&exclude_path) {
                        ignore_stack = ignore_stack.append(abs_dir.into(), Arc::new(gitignore));
                    }
                }
                found_git = true;
            }

            if found_gitignore && found_git {
                break;
            }
        }

        // Second pass: process all entries with updated ignore_stack
        for entry in children {
            let abs_path = entry.path();
            let Some(name) = abs_path.file_name() else {
                continue;
            };

            let rel_entry_path = rel_path.join(name);

            let Ok(metadata) = entry.metadata() else {
                continue;
            };

            let is_dir = metadata.is_dir();

            // Check if this path is ignored
            let is_ignored = ignore_stack.is_abs_path_ignored(&abs_path, is_dir);

            if is_dir {
                // Skip recursion into ignored directories (e.g., .git, node_modules)
                if !is_ignored {
                    Self::walk_directory(
                        &abs_path,
                        &rel_entry_path,
                        entries,
                        ignore_stack.clone(),
                        root_char_bag,
                        depth + 1,
                    );
                }
            } else {
                // Skip very large files (> 10MB)
                if metadata.len() > 10 * 1024 * 1024 {
                    continue;
                }

                // Create Entry for this file
                let mut char_bag = root_char_bag;
                char_bag.extend(
                    rel_entry_path
                        .to_string_lossy()
                        .chars()
                        .map(|c| c.to_ascii_lowercase()),
                );

                // Convert Path to RelPath
                let rel_path = match RelPath::from_path(&rel_entry_path) {
                    Ok(Cow::Borrowed(path)) => path.into(),
                    Ok(Cow::Owned(path)) => path.into(),
                    Err(_) => continue, // Skip paths that can't be converted
                };

                entries.push(Entry {
                    path: rel_path,
                    is_dir: false,
                    is_ignored,
                    char_bag,
                    size: metadata.len(),
                });
            }
        }
    }
}

/// Build a Gitignore from a .gitignore file path.
///
/// Reads the file, parses gitignore patterns, and returns a compiled [`Gitignore`].
///
/// # Arguments
///
/// * `path` - Absolute path to the `.gitignore` file
///
/// # Errors
///
/// Returns error if file cannot be read or gitignore patterns are invalid.
fn build_gitignore(path: &Path) -> anyhow::Result<Gitignore> {
    let contents = std::fs::read_to_string(path)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("/"));
    let mut builder = GitignoreBuilder::new(parent);

    for line in contents.lines() {
        builder.add_line(Some(path.into()), line)?;
    }

    Ok(builder.build()?)
}

impl Snapshot {
    /// Get file paths from the snapshot, optionally filtering out ignored files.
    ///
    /// Iterates through the [`SumTree`] and collects file paths. The `include_ignored`
    /// parameter controls whether gitignored files are included in the result.
    ///
    /// # Arguments
    ///
    /// * `include_ignored` - If `false`, excludes files matching `.gitignore` rules
    ///
    /// # Returns
    ///
    /// Vector of paths to files (not directories), sorted alphabetically.
    ///
    /// # Usage
    ///
    /// ```ignore
    /// let files = snapshot.files(false); // Exclude gitignored files
    /// let all_files = snapshot.files(true); // Include everything
    /// ```
    ///
    /// # Performance
    ///
    /// This allocates a new `Vec` each call. For hot paths, consider iterating
    /// the tree directly or caching the result.
    pub fn files(&self, include_ignored: bool) -> Vec<PathBuf> {
        self.entries_by_path
            .iter()
            .filter(|entry| !entry.is_dir && (include_ignored || !entry.is_ignored))
            .map(|entry| PathBuf::from(entry.path.to_string()))
            .collect()
    }

    /// Get all file entries (not just paths) for fuzzy matching.
    ///
    /// Returns clones of Entry objects which contain the path, char_bag, and other metadata
    /// needed for efficient fuzzy matching.
    pub fn entries(&self, include_ignored: bool) -> Vec<Entry> {
        self.entries_by_path
            .iter()
            .filter(|entry| !entry.is_dir && (include_ignored || !entry.is_ignored))
            .cloned()
            .collect()
    }

    /// Get the root directory of this snapshot.
    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_discovers_rust_files() {
        let worktree = Worktree::new(PathBuf::from("src"));
        let snapshot = worktree.snapshot();
        let files = snapshot.files(false); // Exclude ignored files

        assert!(!files.is_empty(), "Should discover files in src/");

        // Should find lib.rs
        assert!(
            files.iter().any(|p| p.ends_with("lib.rs")),
            "Should find lib.rs"
        );
    }

    #[test]
    fn snapshot_ignores_hidden() {
        let worktree = Worktree::new(PathBuf::from("."));
        let snapshot = worktree.snapshot();
        let files = snapshot.files(false); // Exclude ignored files

        // Should not include .git directory files (gitignored by IgnoreStack)
        assert!(
            !files.iter().any(|p| p.to_string_lossy().contains(".git/")),
            "Should not include .git directory"
        );
    }

    #[test]
    fn worktree_refresh_updates_list() {
        let mut worktree = Worktree::new(PathBuf::from("src"));
        let initial_count = worktree.snapshot().files(false).len();

        // Refresh should work (may or may not change count depending on FS state)
        worktree.refresh();
        let after_count = worktree.snapshot().files(false).len();

        // Should still have files
        assert!(after_count > 0, "Should still have files after refresh");

        // For a stable directory, count should be same or similar
        let diff = (initial_count as i32 - after_count as i32).abs();
        assert!(
            diff < 100,
            "File count shouldn't drastically change on refresh"
        );
    }
}
