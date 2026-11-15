//! The Commit Preview, a read-only overlay that shows one commit's header over
//! its patch against its first parent.
//!
//! `Enter` on a committed line of a blamed pane opens it. The patch builds on
//! the blocking pool from git objects alone, so the working tree, HEAD, and the
//! buffers stay as they were while it shows.

use crate::{
    action_handlers::commits::{self, PreviewHighlights},
    app::{Stoat, UpdateEffect},
    commit_list::PreviewLoad,
    host::BlameCommit,
    keymap::Keymap,
    keymap_state::{Flags, StoatKeymapState},
    lsp::pending::Pending,
    review_session::DiffDocument,
};
use std::sync::Arc;
use stoat_scheduler::Task;

/// The `modal` predicate value of the preview, which its key block matches.
const MODAL: &str = "commit_preview";

/// A commit's header over its patch, open over the panes as a modal.
///
/// The preview holds its commit rather than reading the cursor's line, so the
/// blame keys it binds act on the commit it shows.
pub(crate) struct CommitPreview {
    pub(crate) commit: BlameCommit,
    /// The index in the commit's parents of the parent the meta line names.
    pub(crate) parent: usize,
    /// The patch, `None` while it builds and `Some(None)` for a commit with no
    /// diff to show.
    pub(crate) document: Option<Option<Arc<DiffDocument>>>,
    /// The commit's file count and its added and removed lines, once the build
    /// lands.
    pub(crate) summary: Option<(usize, u32, u32)>,
    /// The rows the patch scrolls down by.
    ///
    /// A step moves it freely and the paint clamps it, since only the built
    /// patch knows how many rows it has.
    pub(crate) scroll: usize,
    /// The labeled keys of the preview's key block, as its footer lists them.
    pub(crate) hints: Vec<(String, String)>,
    load: Pending<PreviewLoad>,
}

/// Open the preview on `commit` and start the build of its patch.
///
/// The preview puts the pane in normal mode, where the preview's keys bind.
/// Outside a repository with a working tree it opens nothing.
pub(crate) fn open(stoat: &mut Stoat, commit: BlameCommit) {
    let Some(task) = start_load(stoat, &commit.info.sha) else {
        return;
    };
    let mut load = Pending::default();
    load.arm(task);

    stoat.set_focused_mode("normal".to_string());
    stoat.commit_preview = Some(CommitPreview {
        commit,
        parent: 0,
        document: None,
        summary: None,
        scroll: 0,
        hints: footer_hints(&stoat.keymap),
        load,
    });
}

/// Build the open preview's patch again.
///
/// A theme switch needs this, because the build bakes the theme's syntax
/// colors into the patch. The preview keeps its old patch when no repository
/// answers.
pub(crate) fn reload(stoat: &mut Stoat) {
    let Some(sha) = stoat
        .commit_preview
        .as_ref()
        .map(|preview| preview.commit.info.sha.clone())
    else {
        return;
    };
    let Some(task) = start_load(stoat, &sha) else {
        return;
    };

    if let Some(preview) = stoat.commit_preview.as_mut() {
        preview.document = None;
        preview.load.arm(task);
    }
}

/// File the open preview's patch and summary once their build lands, and
/// answer whether they landed.
pub(crate) fn pump(stoat: &mut Stoat) -> bool {
    let Some(preview) = stoat.commit_preview.as_mut() else {
        return false;
    };
    let Some(loaded) = preview.load.poll(&stoat.redraw_notify) else {
        return false;
    };

    preview.summary = loaded.summary.map(|files| {
        let added = files.iter().map(|file| file.additions).sum();
        let removed = files.iter().map(|file| file.deletions).sum();
        (files.len(), added, removed)
    });
    preview.document = Some(loaded.document.map(Arc::new));
    true
}

/// Scroll the open preview's patch down by `rows`, or up for a negative count.
///
/// The scroll stops at the first row, and the paint pulls a scroll past the
/// last page back to it.
pub(crate) fn scroll_by(stoat: &mut Stoat, rows: i32) -> UpdateEffect {
    let Some(preview) = stoat.commit_preview.as_mut() else {
        return UpdateEffect::None;
    };
    preview.scroll = preview.scroll.saturating_add_signed(rows as isize);
    UpdateEffect::Redraw
}

/// Close the preview, which leaves the blamed pane it opened over.
pub(crate) fn close(stoat: &mut Stoat) -> UpdateEffect {
    stoat.commit_preview = None;
    UpdateEffect::Redraw
}

/// Start the build of `sha`'s patch and summary on the blocking pool, or
/// `None` outside a repository with a working tree.
fn start_load(stoat: &Stoat, sha: &str) -> Option<Task<PreviewLoad>> {
    let repo = stoat
        .git_host
        .discover(&stoat.active_workspace().git_root)?;
    let workdir = repo.workdir()?;
    Some(commits::spawn_commit_preview_load(
        &stoat.executor,
        repo,
        workdir,
        sha.to_owned(),
        stoat.language_registry.clone(),
        stoat.redraw_notify.clone(),
        PreviewHighlights::from_stoat(stoat),
    ))
}

/// The key and the label of each labeled binding in the preview's key block,
/// in keymap order, with the keys of one label joined by `/`.
fn footer_hints(keymap: &Keymap) -> Vec<(String, String)> {
    let state = StoatKeymapState::with_flags("normal", Flags::default()).with_modal(MODAL);
    let mut hints: Vec<(String, String)> = Vec::new();
    for (key, _, label) in keymap.scoped_bindings(&state, "modal", MODAL) {
        let Some(label) = label else {
            continue;
        };
        match hints.iter_mut().find(|(_, shown)| shown == label) {
            Some((keys, _)) => {
                keys.push('/');
                keys.push_str(&key);
            },
            None => hints.push((key, label.to_owned())),
        }
    }
    hints
}
