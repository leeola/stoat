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
use stoat_language::{Language, LanguageRegistry};

/// Discover the git repo at `git_root`, list its working-tree changes,
/// and build the [`ReviewFileInput`] vector that
/// [`extract_review_hunks_changeset`] consumes. Returns `None` when no
/// repo is found or the working tree is clean. The first tuple element
/// is the discovered repository workdir; callers building a
/// [`crate::review_session::ReviewSession`] use it for
/// [`crate::review_session::ReviewSource::WorkingTree`].
///
/// When `language_override` is `Some`, every input's
/// [`ReviewFileInput::language`] is set to that override regardless of
/// the file's extension. When `None`, the per-file path is resolved
/// against `langs` via [`LanguageRegistry::for_path`] (unknown
/// extensions land as `None`).
///
/// `staged_filter` restricts which changed paths are included by their
/// [`crate::host::ChangedFile::staged`] flag: `Some(true)` keeps only
/// staged paths, `Some(false)` only unstaged paths, `None` keeps every
/// changed path. The per-file diff is always HEAD vs working tree
/// regardless of the filter -- the flag selects which files appear, not
/// what each is diffed against.
///
/// Per-file read failures are logged at `warn` level and the file is
/// skipped, matching the behavior the TUI review path has shipped with
/// since `open_review` first landed.
pub fn scan_working_tree(
    git: &dyn GitHost,
    fs: &dyn FsHost,
    langs: &LanguageRegistry,
    git_root: &Path,
    language_override: Option<Arc<Language>>,
    staged_filter: Option<bool>,
) -> Option<(PathBuf, Vec<ReviewFileInput>)> {
    let repo = git.discover(git_root)?;
    let workdir = repo.workdir()?;

    let changed = repo.changed_files();
    if changed.is_empty() {
        return None;
    }

    let mut inputs: Vec<ReviewFileInput> = Vec::with_capacity(changed.len());
    for file in &changed {
        if staged_filter.is_some_and(|want| file.staged != want) {
            continue;
        }
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
        let base_text = repo.head_content(&file.path).unwrap_or_default();
        let lang = language_override
            .clone()
            .or_else(|| langs.for_path(&file.path));
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
