//! The Commit Preview, a read-only overlay that shows one commit's header over
//! its patch against one of its parents.
//!
//! `Enter` on a committed line of a blamed pane opens it on the patch against
//! the first parent, and `P` steps a merge through its other parents. The patch
//! builds on the blocking pool from git objects alone, so the working tree,
//! HEAD, and the buffers stay as they were while it shows.

use crate::{
    action_handlers::{
        commits::{self, PreviewHighlights},
        review,
    },
    app::{Stoat, UpdateEffect},
    commit_list::PreviewLoad,
    host::{BlameCommit, CommitInfo},
    keymap::Keymap,
    keymap_state::{Flags, StoatKeymapState},
    lsp::pending::Pending,
    render::commit_patch::{self, PatchRow},
    review_session::DiffDocument,
};
use std::{path::PathBuf, sync::Arc};
use stoat_scheduler::Task;

/// The `modal` predicate value of the preview, which its key block matches.
const MODAL: &str = "commit_preview";

/// Where the reader opened the preview from, which the patch finds and rings.
pub(crate) struct PreviewOrigin {
    /// The repo-relative path the blamed file has in the commit.
    pub(crate) path: PathBuf,
    /// The 0-based line the blamed line has in the commit's version of the
    /// file.
    pub(crate) line: u32,
    /// The pane's file name and the cursor's 1-based line, which the ring's
    /// label reads.
    pub(crate) label: String,
}

/// A commit's header over its patch, open over the panes as a modal.
///
/// The preview holds its commit rather than reading the cursor's line, so the
/// blame keys it binds act on the commit it shows.
pub(crate) struct CommitPreview {
    pub(crate) commit: BlameCommit,
    /// The index in the commit's parents of the parent the patch reads
    /// against, which the meta line names.
    pub(crate) parent: usize,
    /// The patch, `None` while it builds and `Some(None)` for a commit with no
    /// diff to show.
    pub(crate) document: Option<Option<Arc<DiffDocument>>>,
    /// The rows of the patch, built when the document lands.
    pub(crate) rows: Vec<PatchRow>,
    pub(crate) origin: PreviewOrigin,
    /// The row of the patch that holds the origin's line, or `None` before the
    /// patch lands and when no hunk shows that line.
    ///
    /// Only the patch against the first parent, which the preview opens on,
    /// rings the origin. A step to another parent reads that patch from its
    /// top.
    pub(crate) origin_row: Option<usize>,
    /// Whether the first paint that holds the patch still owes the scroll that
    /// shows the origin.
    pub(crate) seek: bool,
    /// The patch's file count and its added and removed lines, once the build
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
/// Its first paint with the patch scrolls to `origin`'s line. Outside a
/// repository with a working tree it opens nothing.
pub(crate) fn open(stoat: &mut Stoat, commit: BlameCommit, origin: PreviewOrigin) {
    let Some(task) = start_load(stoat, &commit.info, 0) else {
        return;
    };
    let mut load = Pending::default();
    load.arm(task);

    stoat.set_focused_mode("normal".to_string());
    stoat.commit_preview = Some(CommitPreview {
        commit,
        parent: 0,
        document: None,
        rows: Vec::new(),
        origin,
        origin_row: None,
        seek: true,
        summary: None,
        scroll: 0,
        hints: footer_hints(&stoat.keymap),
        load,
    });
}

/// Build the open preview's patch again, against the parent it reads.
///
/// A theme switch needs this, because the build bakes the theme's syntax
/// colors into the patch. The preview keeps its old patch when no repository
/// answers.
pub(crate) fn reload(stoat: &mut Stoat) {
    let Some(preview) = stoat.commit_preview.as_ref() else {
        return;
    };
    let Some(task) = start_load(stoat, &preview.commit.info, preview.parent) else {
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

    preview.summary = Some(match loaded.summary {
        Some(files) => {
            let added = files.iter().map(|file| file.additions).sum();
            let removed = files.iter().map(|file| file.deletions).sum();
            (files.len(), added, removed)
        },
        None => loaded
            .document
            .as_ref()
            .map_or((0, 0, 0), commit_patch::patch_summary),
    });
    preview.rows = loaded
        .document
        .as_ref()
        .map(commit_patch::patch_rows)
        .unwrap_or_default();
    preview.origin_row = loaded
        .document
        .as_ref()
        .filter(|_| preview.parent == 0)
        .and_then(|doc| {
            commit_patch::origin_row(
                doc,
                &preview.rows,
                &preview.origin.path,
                preview.origin.line,
            )
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

/// Scroll the open preview's patch to the header of the next or the previous
/// file.
///
/// A forward hop takes the first header below the top row, and a backward hop
/// the nearest header above it. If the top row is inside a file, that file's
/// own header is the nearest one above it.
///
/// The header goes to the top of the body, unless the patch ends less than a
/// page below it. Past the last or the first file the scroll stays, and the
/// status says so.
pub(crate) fn hop_file(stoat: &mut Stoat, forward: bool) -> UpdateEffect {
    let Some(preview) = stoat.commit_preview.as_mut() else {
        return UpdateEffect::None;
    };

    let scroll = preview.scroll;
    let mut headers = preview
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| matches!(row, PatchRow::Header { .. }))
        .map(|(at, _)| at);
    let target = if forward {
        headers.find(|&at| at > scroll)
    } else {
        headers.rfind(|&at| at < scroll)
    };

    match target {
        Some(at) => preview.scroll = at,
        None => stoat.set_status(if forward {
            "no file below"
        } else {
            "no file above"
        }),
    }
    UpdateEffect::Redraw
}

/// Show the open preview's patch against the commit's next parent, and the
/// first parent again after the last.
///
/// The patch builds again and opens at its top. A commit with one parent or
/// none keeps its patch, and the status says so.
pub(crate) fn next_parent(stoat: &mut Stoat) -> UpdateEffect {
    let Some(preview) = stoat.commit_preview.as_ref() else {
        return UpdateEffect::None;
    };
    let parents = preview.commit.info.parents.len();
    if parents < 2 {
        stoat.set_status(match parents {
            0 => "commit has no parent",
            _ => "commit has one parent",
        });
        return UpdateEffect::Redraw;
    }

    let parent = (preview.parent + 1) % parents;
    let Some(task) = start_load(stoat, &preview.commit.info, parent) else {
        return UpdateEffect::None;
    };
    if let Some(preview) = stoat.commit_preview.as_mut() {
        preview.parent = parent;
        preview.document = None;
        preview.rows.clear();
        preview.summary = None;
        preview.origin_row = None;
        preview.scroll = 0;
        preview.load.arm(task);
    }
    UpdateEffect::Redraw
}

/// Close the preview, which leaves the blamed pane it opened over.
pub(crate) fn close(stoat: &mut Stoat) -> UpdateEffect {
    stoat.commit_preview = None;
    UpdateEffect::Redraw
}

/// Start the build of `commit`'s patch against the parent at index `parent`
/// on the blocking pool, or `None` outside a repository with a working tree.
///
/// The build against the first parent also reads the commit's change summary,
/// and it pairs a renamed file with the file it moved from. The build against
/// another parent reads the two trees alone, so a renamed file reads as a
/// removal and an addition, and it carries no summary.
///
/// The pump polls the build through [`Pending`], whose waker asks for the pass
/// that collects it. So the build against another parent sends no wake of its
/// own.
fn start_load(stoat: &Stoat, commit: &CommitInfo, parent: usize) -> Option<Task<PreviewLoad>> {
    let repo = stoat
        .git_host
        .discover(&stoat.active_workspace().git_root)?;
    let workdir = repo.workdir()?;
    let sha = commit.sha.clone();
    let registry = stoat.language_registry.clone();
    let highlights = PreviewHighlights::from_stoat(stoat);
    if parent == 0 {
        return Some(commits::spawn_commit_preview_load(
            &stoat.executor,
            repo,
            workdir,
            sha,
            registry,
            stoat.redraw_notify.clone(),
            highlights,
        ));
    }

    let base = commit.parents.get(parent)?.clone();
    Some(stoat.executor.spawn_blocking(move || {
        let document = review::changed_or_whole(&*repo, Some(&base), &sha)
            .and_then(|changes| review::build_document_from_changes(&registry, &workdir, changes))
            .map(|mut doc| {
                highlights.attach(&mut doc);
                doc
            });
        PreviewLoad {
            summary: None,
            message: None,
            document,
        }
    }))
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
