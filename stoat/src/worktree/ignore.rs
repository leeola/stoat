//! Gitignore rule stacking for worktree file discovery.
//!
//! This module provides [`IgnoreStack`], a recursive data structure that maintains
//! a hierarchy of gitignore rules as directories are traversed. Each level of the
//! stack corresponds to a directory containing a `.gitignore` file.
//!
//! # Architecture
//!
//! The stack pattern matches how git processes ignore rules:
//! - Parent directory rules apply to all children
//! - Child `.gitignore` files extend (not replace) parent rules
//! - Later rules can override earlier rules (via whitelist patterns)
//!
//! # Usage
//!
//! ```ignore
//! let stack = IgnoreStack::none();
//!
//! // When entering a directory with .gitignore:
//! let gitignore = build_gitignore(".gitignore")?;
//! let stack = stack.append(dir_path, Arc::new(gitignore));
//!
//! // Check if file should be ignored:
//! if stack.is_abs_path_ignored(&file_path, false) {
//!     // Skip this file
//! }
//! ```
//!
//! # Related
//!
//! See also:
//! - [`crate::worktree::Worktree::walk_directory`] - uses IgnoreStack during file discovery
//! - [`ignore::gitignore::Gitignore`] - underlying gitignore parser from `ignore` crate

use ignore::gitignore::Gitignore;
use std::{ffi::OsStr, path::Path, sync::Arc};

/// Recursive stack of gitignore rules.
///
/// Implements a linked-list-like structure where each node contains:
/// - A [`Gitignore`] rule set
/// - The absolute base path where those rules apply
/// - A reference to the parent stack (outer directory rules)
///
/// # Variants
///
/// - [`None`](IgnoreStack::None) - No ignore rules (base case)
/// - [`Global`](IgnoreStack::Global) - Global gitignore from git config
/// - [`All`](IgnoreStack::All) - Ignore everything (used for special cases)
/// - [`Some`](IgnoreStack::Some) - Contains rules + parent stack
///
/// # Design
///
/// Ported from Zed's `worktree/src/ignore.rs`. Uses `Arc` for cheap cloning
/// and structural sharing - when recursing into subdirectories, the child
/// stack holds an `Arc` to the parent, avoiding duplication.
#[derive(Debug)]
pub enum IgnoreStack {
    /// No ignore rules - nothing is ignored
    None,
    /// Global gitignore rules from git config (core.excludesfile)
    Global {
        /// Compiled global gitignore rules
        ignore: Arc<Gitignore>,
    },
    /// A level in the stack with gitignore rules
    Some {
        /// Absolute path to the directory containing the .gitignore file
        abs_base_path: Arc<Path>,
        /// Compiled gitignore rules for this directory
        ignore: Arc<Gitignore>,
        /// Parent directory's ignore rules (recursive)
        parent: Arc<IgnoreStack>,
    },
    /// Special case: ignore everything
    All,
}

impl IgnoreStack {
    /// Create an empty ignore stack (no rules).
    ///
    /// Use this as the starting point for file discovery. As `.gitignore` files
    /// are encountered, call [`append`](Self::append) to build up the stack.
    pub fn none() -> Arc<Self> {
        Arc::new(Self::None)
    }

    /// Create an ignore stack with global gitignore rules.
    ///
    /// Global rules are loaded from git's `core.excludesfile` config setting,
    /// typically `~/.config/git/ignore`. These rules apply to all repositories.
    ///
    /// # Arguments
    ///
    /// * `ignore` - Compiled global gitignore rules
    ///
    /// # Usage
    ///
    /// Called during worktree initialization:
    ///
    /// ```ignore
    /// if let Some(global_path) = gitconfig_excludes_path() {
    ///     if let Ok(gitignore) = build_gitignore(&global_path) {
    ///         ignore_stack = IgnoreStack::global(Arc::new(gitignore));
    ///     }
    /// }
    /// ```
    pub fn global(ignore: Arc<Gitignore>) -> Arc<Self> {
        Arc::new(Self::Global { ignore })
    }

    /// Create a stack that ignores everything.
    ///
    /// Rarely used - mainly for special cases where you want to exclude
    /// an entire directory tree.
    #[allow(dead_code)]
    pub fn all() -> Arc<Self> {
        Arc::new(Self::All)
    }

    /// Append a new gitignore rule set to the stack.
    ///
    /// Creates a new stack level that extends the current rules. The new level
    /// will check its rules first, then delegate to parent rules if no match.
    ///
    /// # Arguments
    ///
    /// * `abs_base_path` - Absolute path to directory containing `.gitignore`
    /// * `ignore` - Compiled gitignore rules from this directory
    ///
    /// # Returns
    ///
    /// New stack with the added rules. If called on [`IgnoreStack::All`],
    /// returns the same `All` stack (can't extend "ignore everything").
    ///
    /// # Usage
    ///
    /// Called during directory traversal when a `.gitignore` file is found:
    ///
    /// ```ignore
    /// if child_name == ".gitignore" {
    ///     let gitignore = build_gitignore(&child_abs_path)?;
    ///     ignore_stack = ignore_stack.append(
    ///         dir_abs_path.clone(),
    ///         Arc::new(gitignore)
    ///     );
    /// }
    /// ```
    pub fn append(self: Arc<Self>, abs_base_path: Arc<Path>, ignore: Arc<Gitignore>) -> Arc<Self> {
        match self.as_ref() {
            IgnoreStack::All => self,
            _ => Arc::new(Self::Some {
                abs_base_path,
                ignore,
                parent: self,
            }),
        }
    }

    /// Check if a path should be ignored according to the stacked rules.
    ///
    /// Recursively checks rules from innermost (current directory) to outermost
    /// (root directory), matching git's behavior.
    ///
    /// # Arguments
    ///
    /// * `abs_path` - Absolute path to check
    /// * `is_dir` - Whether the path is a directory (affects pattern matching)
    ///
    /// # Returns
    ///
    /// `true` if the path should be ignored, `false` if it should be included.
    ///
    /// # Special Cases
    ///
    /// - `.git` directories are always ignored (even without gitignore rules)
    /// - Whitelist patterns (starting with `!`) can un-ignore previously ignored paths
    ///
    /// # Implementation
    ///
    /// Uses the `ignore` crate's [`Gitignore::matched`] which returns:
    /// - `Match::None` - No rule matched, check parent
    /// - `Match::Ignore` - Matched ignore rule, exclude this path
    /// - `Match::Whitelist` - Matched `!pattern`, include this path
    pub fn is_abs_path_ignored(&self, abs_path: &Path, is_dir: bool) -> bool {
        // Special case: .git directories are always ignored
        if is_dir && abs_path.file_name() == Some(OsStr::new(".git")) {
            return true;
        }

        match self {
            Self::None => false,
            Self::All => true,
            Self::Global { ignore } => {
                // Global gitignore uses absolute paths
                match ignore.matched(abs_path, is_dir) {
                    ignore::Match::None => false,
                    ignore::Match::Ignore(_) => true,
                    ignore::Match::Whitelist(_) => false,
                }
            },
            Self::Some {
                abs_base_path,
                ignore,
                parent,
            } => {
                // Strip base path to get relative path for matching
                let relative_path = abs_path.strip_prefix(abs_base_path).unwrap();

                match ignore.matched(relative_path, is_dir) {
                    ignore::Match::None => {
                        // No match at this level, check parent rules
                        parent.is_abs_path_ignored(abs_path, is_dir)
                    },
                    ignore::Match::Ignore(_) => true,
                    ignore::Match::Whitelist(_) => false,
                }
            },
        }
    }
}
