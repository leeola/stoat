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
use stoat_text::LineEnding;

/// Discover the git repo at `git_root`, list its working-tree changes,
/// and build the [`ReviewFileInput`] vector that
/// [`extract_review_hunks_changeset`] consumes. Returns `None` when no
/// repo is found or the working tree is clean. The first tuple element
/// is the discovered repository workdir, which callers use to resolve the
/// paths the inputs name.
///
/// A moved file diffs against the blob it was moved from, so a pure rename
/// yields an input whose two texts match and therefore no hunks at all.
///
/// Per-file read failures are logged at `warn` level and the file is
/// skipped, so one unreadable file does not lose the whole scan.
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

    // A moved file's blob sits under the path it came from. Asking for the
    // current path instead answers nothing, which reads as a whole-file
    // addition beside the old path's whole-file deletion.
    let head_paths: Vec<&Path> = changed
        .iter()
        .map(|f| f.renamed_from.as_deref().unwrap_or(&f.path))
        .collect();
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

/// A file's disk contents with line terminators normalized to bare `\n`.
///
/// The normalization matches how a buffer holds its text, so a diff run over
/// disk bytes lands the same hunks as one run over the open buffer.
pub(crate) fn read_utf8(fs: &dyn FsHost, path: &Path) -> std::io::Result<String> {
    let mut buf = Vec::new();
    fs.read(path, &mut buf)?;
    let text = String::from_utf8(buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(LineEnding::normalize(&text).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{extract_review_hunks_changeset, scan_working_tree};
    use crate::host::fake::{FakeFs, FakeGit};
    use std::path::Path;
    use stoat_language::LanguageRegistry;

    #[test]
    fn a_crlf_working_tree_over_an_lf_blob_reports_no_hunks() {
        let fs = FakeFs::new();
        let git = FakeGit::new();
        git.add_repo("/repo")
            .with_fs(&fs)
            .modified("a.txt", "a\nb\n", "a\r\nb\r\n");

        let langs = LanguageRegistry::standard();
        let (_workdir, inputs) =
            scan_working_tree(&git, &fs, &langs, Path::new("/repo")).expect("the repo has changes");

        let hunks = extract_review_hunks_changeset(&inputs, 3, None);
        assert_eq!(
            hunks.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![0],
            "disk bytes differing from the blob only in their line terminators \
             carry no change",
        );
    }

    /// A move carries its content to a new address. Reading the base at that
    /// address finds nothing, which reads as a file written from scratch.
    #[test]
    fn a_moved_file_diffs_against_the_blob_it_moved_from() {
        let fs = FakeFs::new();
        let git = FakeGit::new();
        git.add_repo("/repo")
            .with_fs(&fs)
            .renamed("old.txt", "new.txt", "a\nb\n");

        let langs = LanguageRegistry::standard();
        let (_workdir, inputs) =
            scan_working_tree(&git, &fs, &langs, Path::new("/repo")).expect("the repo has changes");

        assert_eq!(
            inputs
                .iter()
                .map(|i| (i.rel_path.as_str(), i.base_text.as_str()))
                .collect::<Vec<_>>(),
            vec![("new.txt", "a\nb\n")],
            "the moved file's base is the old path's blob, listed at the new path",
        );
        assert_eq!(
            extract_review_hunks_changeset(&inputs, 3, None)
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            vec![0],
            "a move edits no line",
        );
    }
}
