//! The checkout confirm, which asks before a blame checkout stashes the
//! working tree's changes.
//!
//! A checkout detaches HEAD, and a tree with uncommitted changes stands in its
//! way. The confirm names the changes and offers to stash them, and the stash
//! comes back when the reader returns. A clean tree checks out with no confirm.

use crate::{
    app::{Stoat, UpdateEffect},
    commit_preview::CommitPreview,
    host::{ChangedFile, CommitInfo, StagedKind},
    keymap::Keymap,
    keymap_state::{self, ActiveModal, Flags, StoatKeymapState},
};
use std::path::{Path, PathBuf};

/// The `modal` predicate value of the confirm, which its key block matches.
const MODAL: &str = "checkout_confirm";

/// A blame checkout's commit and the place of its line, carried from the key
/// press to the walk's start.
pub(crate) struct CheckoutTarget {
    pub(crate) workdir: PathBuf,
    pub(crate) commit: CommitInfo,
    /// The repo-relative path the blamed line's file has in the commit.
    pub(crate) path: PathBuf,
    /// The 0-based line the blamed line has in the commit's version of the
    /// file.
    pub(crate) row: u32,
}

/// A checkout that waits on the reader's word, open over the panes as a modal.
pub(crate) struct CheckoutConfirm {
    pub(crate) target: CheckoutTarget,
    /// The branch HEAD stands on, or `None` for a detached HEAD.
    pub(crate) branch: Option<String>,
    /// HEAD's short sha, which the message names for a detached HEAD.
    pub(crate) head: String,
    /// Each changed file as its two-character status and its repo-relative
    /// path, in the order the repository lists them.
    pub(crate) files: Vec<(String, String)>,
    /// The Commit Preview that the confirm opened over.
    ///
    /// Stoat shows one modal at a time, so the confirm holds the preview while
    /// it shows, paints it under its own box, and hands it back on a cancel.
    pub(crate) behind: Option<CommitPreview>,
    /// The labeled keys of the confirm's key block, as its last row lists them.
    pub(crate) hints: Vec<(String, String)>,
}

/// Open the confirm over `target`, with an open Commit Preview behind it.
///
/// The confirm puts the pane in normal mode, where its keys bind. A modal
/// other than the Commit Preview keeps the confirm closed, since stoat shows
/// one modal at a time, and the checkout does not run.
pub(crate) fn open(
    stoat: &mut Stoat,
    target: CheckoutTarget,
    branch: Option<String>,
    head: String,
    files: Vec<(String, String)>,
) {
    if keymap_state::active_modal(stoat).is_some_and(|modal| modal != ActiveModal::CommitPreview) {
        return;
    }

    let behind = stoat.commit_preview.take();
    stoat.set_focused_mode("normal".to_string());
    stoat.checkout_confirm = Some(CheckoutConfirm {
        target,
        branch,
        head,
        files,
        behind,
        hints: hints(&stoat.keymap),
    });
}

/// Close the confirm with no checkout, and bring back the Commit Preview it
/// opened over.
pub(crate) fn cancel(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(confirm) = stoat.checkout_confirm.take() {
        stoat.commit_preview = confirm.behind;
    }
    UpdateEffect::Redraw
}

/// Each of `files` as its two-character status and its path relative to
/// `workdir`, as `git status --short` lists it.
pub(crate) fn file_rows(files: &[ChangedFile], workdir: &Path) -> Vec<(String, String)> {
    files
        .iter()
        .map(|file| {
            let path = file.path.strip_prefix(workdir).unwrap_or(&file.path);
            (status_code(file).to_owned(), path.display().to_string())
        })
        .collect()
}

/// The two-character status `git status --short` gives `file`.
///
/// The change list does not tell an unstaged deletion from an unstaged edit,
/// so both read ` M`.
fn status_code(file: &ChangedFile) -> &'static str {
    if file.untracked {
        return "??";
    }
    match file.staged_kind {
        Some(StagedKind::Added) => "A ",
        Some(StagedKind::Modified) => "M ",
        Some(StagedKind::Deleted) => "D ",
        Some(StagedKind::Renamed) => "R ",
        None => " M",
    }
}

/// The key and the label of each labeled binding in the confirm's key block,
/// in keymap order.
fn hints(keymap: &Keymap) -> Vec<(String, String)> {
    let state = StoatKeymapState::with_flags("normal", Flags::default()).with_modal(MODAL);
    keymap
        .scoped_bindings(&state, "modal", MODAL)
        .into_iter()
        .filter_map(|(key, _, label)| Some((key, label?.to_owned())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::file_rows;
    use crate::host::{ChangedFile, StagedKind};
    use std::path::{Path, PathBuf};

    /// Each file reads as `git status --short` lists it, with its path
    /// relative to the working tree.
    #[test]
    fn file_rows_read_the_short_status_of_each_change() {
        let file = |path: &str, staged_kind: Option<StagedKind>, untracked: bool| ChangedFile {
            path: PathBuf::from("/repo").join(path),
            staged: staged_kind.is_some(),
            staged_kind,
            untracked,
            renamed_from: None,
        };
        let files = [
            file("a", Some(StagedKind::Added), false),
            file("b", Some(StagedKind::Modified), false),
            file("c", Some(StagedKind::Deleted), false),
            file("d", Some(StagedKind::Renamed), false),
            file("e", None, false),
            file("f", None, true),
        ];
        let want = [
            ("A ", "a"),
            ("M ", "b"),
            ("D ", "c"),
            ("R ", "d"),
            (" M", "e"),
            ("??", "f"),
        ];
        assert_eq!(
            file_rows(&files, Path::new("/repo")),
            want.map(|(code, path)| (code.to_string(), path.to_string())),
        );
    }
}
