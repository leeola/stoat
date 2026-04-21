//! libgit2 tree-construction and blob-reading helpers shared across
//! [`super::LocalGitRepo`]'s mutating methods (`amend_head`,
//! `rewrite_commit`, `create_commit`) and the rebase submodule.

use git2::Repository;
use std::{collections::BTreeMap, path::PathBuf};

/// Materialize `tree` as a git tree object and return its oid. Handles
/// nested paths (`sub/b.rs`) by constructing a staging index and
/// writing it out; libgit2 builds the intermediate trees automatically.
pub(super) fn build_tree_from_map(
    repo: &Repository,
    tree: &BTreeMap<PathBuf, String>,
) -> Result<git2::Oid, git2::Error> {
    // `Index::new()` produces a bare index that `add_frombuffer`
    // refuses because it cannot create the backing blob. Write each
    // blob explicitly via the repo, then point each index entry at
    // its oid.
    let mut index = git2::Index::new()?;
    for (path, content) in tree {
        let blob_oid = repo.blob(content.as_bytes())?;
        let entry = git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            file_size: content.len() as u32,
            id: blob_oid,
            flags: 0,
            flags_extended: 0,
            path: path.to_string_lossy().as_bytes().to_vec(),
        };
        index.add(&entry)?;
    }
    index.write_tree_to(repo)
}

/// Read a blob by oid into a UTF-8 string; returns None on lookup
/// failure or non-UTF-8 content.
pub(super) fn read_blob(repo: &Repository, oid: git2::Oid) -> Option<String> {
    let blob = repo.find_blob(oid).ok()?;
    std::str::from_utf8(blob.content()).ok().map(String::from)
}
