//! TUI-free diff API. Re-exports the structural-diff hunk extractor
//! and supporting row/side/hunk types from the in-tree review module
//! so downstream consumers -- the bin layer's `stoat diff` subcommand
//! and the diff-cache RPC -- can compute and consume the same
//! per-file hunks the review pane renders without depending on the
//! TUI rendering path.

use crate::host::{FsHost, GitHost};
pub use crate::review::{
    extract_review_hunks_changeset, MoveProvenance, ReviewFileInput, ReviewHunk, ReviewRow,
    ReviewSide,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_language::LanguageRegistry;

/// Discover the git repo at `git_root`, list its working-tree changes,
/// and build the [`ReviewFileInput`] vector that
/// [`extract_review_hunks_changeset`] consumes. Returns `None` when no
/// repo is found or the working tree is clean. The first tuple element
/// is the discovered repository workdir; callers building a
/// [`crate::review_session::ReviewSession`] use it for
/// [`crate::review_session::ReviewSource::WorkingTree`].
///
/// Per-file read failures are logged at `warn` level and the file is
/// skipped, matching the behavior the TUI review path has shipped with
/// since `open_review` first landed.
pub fn scan_working_tree(
    git: &dyn GitHost,
    fs: &dyn FsHost,
    langs: &LanguageRegistry,
    git_root: &Path,
) -> Option<(PathBuf, Vec<ReviewFileInput>)> {
    let repo = git.discover(git_root)?;
    let workdir = repo.workdir()?;

    let changed = repo.changed_files();
    if changed.is_empty() {
        return None;
    }

    let head_paths: Vec<&Path> = changed.iter().map(|f| f.path.as_path()).collect();
    let head_texts = repo.head_contents(&head_paths);

    let mut inputs: Vec<ReviewFileInput> = Vec::with_capacity(changed.len());
    for (file, base_text) in changed.iter().zip(head_texts) {
        let buffer_text = match read_utf8(fs, &file.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                tracing::warn!(
                    path = %file.path.display(),
                    error = %e,
                    "scan_working_tree: skip file",
                );
                continue;
            },
        };
        let base_text = base_text.unwrap_or_default();
        let lang = langs.for_path(&file.path);
        let rel_path = file
            .path
            .strip_prefix(&workdir)
            .unwrap_or(&file.path)
            .display()
            .to_string();
        inputs.push(ReviewFileInput {
            path: file.path.clone(),
            rel_path,
            language: lang,
            base_text: Arc::new(base_text),
            buffer_text: Arc::new(buffer_text),
        });
    }

    Some((workdir, inputs))
}

fn read_utf8(fs: &dyn FsHost, path: &Path) -> std::io::Result<String> {
    let mut buf = Vec::new();
    fs.read(path, &mut buf)?;
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
