use crate::{
    app::{Stoat, UpdateEffect},
    file_finder::{Browse, FileFinder, FinderScope, OpenIntent},
    picker::PathPicker,
};
use std::{collections::HashSet, ops::ControlFlow, path::PathBuf};
use stoat_action::{OpenFile, SplitNewDown, SplitNewRight};
use stoat_scheduler::Task;
use tokio::sync::mpsc::UnboundedReceiver;

/// Load the open file finder's preview content ahead of the parse scheduler.
///
/// A no-op when no finder is open. Mirrors [`super::sync_palette_picker`]:
/// preview content has to be synced during `drive_background`, before
/// `drive_parse_jobs` runs, not during the paint pass. Synced later, the parse
/// for the newly selected file is never driven while the modal sits idle, so
/// the preview stays unhighlighted until the next unrelated event.
pub(crate) fn sync_file_finder_preview(stoat: &mut Stoat) {
    if stoat.file_finder.is_none() {
        return;
    }
    sync_file_finder_browse(stoat);
    let active_idx = stoat.active_workspace;
    let ws = &mut stoat.workspaces[active_idx];
    let fs_host = &*stoat.fs_host;
    let language_registry = &stoat.language_registry;
    let finder = stoat.file_finder.as_mut().expect("file_finder present");
    finder.refilter_from_input(ws);
    finder.sync_preview(ws, fs_host, language_registry);
}

/// Enter, re-root, or leave the finder's directory-browse mode for the current
/// query.
///
/// A `/` or `~/` query walks the typed directory; the walk re-roots (a fresh
/// [`spawn_workspace_walk`]) whenever the directory part changes, so typing a
/// deeper segment follows it. A query that stops being path-shaped drops browse
/// and disposes its preview so the workspace list resumes. Runs before
/// [`FileFinder::refilter_from_input`] so the picker it selects is up to date.
fn sync_file_finder_browse(stoat: &mut Stoat) {
    let query = {
        let ws = stoat.active_workspace();
        let finder = stoat.file_finder.as_ref().expect("file_finder present");
        finder.input.text(ws)
    };
    let home = stoat.env_host.var("HOME");

    let Some((typed_dir, root, partial)) =
        crate::file_finder::split_path_query(&query, home.as_deref())
    else {
        let active_idx = stoat.active_workspace;
        let finder = stoat.file_finder.as_mut().expect("file_finder present");
        finder.leave_browse(&mut stoat.workspaces[active_idx]);
        return;
    };

    let root_changed = stoat
        .file_finder
        .as_ref()
        .expect("file_finder present")
        .browse
        .as_ref()
        .map(|browse| &browse.root)
        != Some(&root);

    if root_changed {
        let (walk_rx, walk_task) = spawn_workspace_walk(stoat, root.clone());
        let executor = stoat.executor.clone();
        let active_idx = stoat.active_workspace;
        let ws = &mut stoat.workspaces[active_idx];
        let finder = stoat.file_finder.as_mut().expect("file_finder present");
        match &mut finder.browse {
            Some(browse) => {
                browse.root = root.clone();
                browse.picker.git_root = root.clone();
                browse.picker.reset_walk(walk_rx, walk_task);
            },
            None => {
                let picker =
                    PathPicker::new(ws, executor, root.clone(), Some((walk_rx, walk_task)));
                finder.browse = Some(Browse {
                    typed_dir: String::new(),
                    root: root.clone(),
                    partial: String::new(),
                    picker,
                });
            },
        }
    }

    if let Some(browse) = &mut stoat
        .file_finder
        .as_mut()
        .expect("file_finder present")
        .browse
    {
        browse.typed_dir = typed_dir;
        browse.partial = partial;
    }
}

/// Open the file finder. No-op if one is already open so that a second
/// `space p` keystroke cannot stack modals or reset progress the user has
/// made. Snapshots the workspace file list and the current git-modified
/// list at open time.
///
/// Resets the underlying editor to normal mode as it opens. The finder is a
/// top-level modal usually launched from a leader like `space`, so without
/// this the editor it opened over keeps the leader mode and surfaces a
/// confusing secondary menu when focus returns to it on close.
pub(super) fn open_file_finder(
    stoat: &mut Stoat,
    open_intent: OpenIntent,
    forced_scope: Option<FinderScope>,
) -> UpdateEffect {
    if stoat.file_finder.is_some() {
        return UpdateEffect::None;
    }

    let initial_scope = forced_scope.unwrap_or_else(|| resolve_remembered_scope(stoat));

    stoat.set_focused_mode("normal".into());

    let executor = stoat.executor.clone();
    let git_root = stoat.active_workspace().git_root.clone();
    let (walk_rx, walk_task) = spawn_workspace_walk(stoat, git_root.clone());
    let modified_paths = crate::file_finder::query_modified(&*stoat.git_host, &git_root);
    let buffer_paths = stoat.active_workspace().buffers.open_paths();
    let finder_scopes = stoat.settings.finder_scopes.clone();

    let ws = stoat.active_workspace_mut();
    stoat.file_finder = Some(FileFinder::new(
        ws,
        executor,
        open_intent,
        initial_scope,
        git_root,
        walk_rx,
        walk_task,
        modified_paths,
        buffer_paths,
        &finder_scopes,
    ));
    UpdateEffect::Redraw
}

/// Resolve the scope a finder open with no forced scope should land in.
///
/// Prefers the workspace's remembered scope, then the configured default, then
/// [`FinderScope::All`]. Both the remembered name and the configured default
/// are validated against the current named scopes, so a name whose scope has
/// been removed from config falls through to the next fallback rather than
/// opening an empty list.
fn resolve_remembered_scope(stoat: &Stoat) -> FinderScope {
    let named = &stoat.settings.finder_scopes;
    let remembered = stoat
        .active_workspace()
        .last_finder_scope
        .as_deref()
        .and_then(|name| FinderScope::from_persist_name(name, named));

    remembered
        .or_else(|| {
            stoat
                .settings
                .finder_default_scope
                .as_deref()
                .and_then(|name| FinderScope::from_persist_name(name, named))
        })
        .unwrap_or(FinderScope::All)
}

/// Spawn the streaming workspace-file walker rooted at `git_root`.
///
/// Returns the receiver yielding batches of discovered paths and the task
/// running the blocking walk. The task must be held to keep the walk alive:
/// dropping it cancels the in-flight walk on runtimes that propagate
/// cancellation. Each batch pings the redraw notifier so a live picker repaints
/// as paths stream in.
pub(super) fn spawn_workspace_walk(
    stoat: &Stoat,
    git_root: PathBuf,
) -> (UnboundedReceiver<Vec<PathBuf>>, Task<()>) {
    let (walk_tx, walk_rx) = tokio::sync::mpsc::unbounded_channel();
    let fs_host = stoat.fs_host.clone();
    let redraw_notify = stoat.redraw_notify.clone();
    let task = stoat.executor.spawn_blocking(move || {
        fs_host.walk_workspace_files_streaming(&git_root, &mut |batch| {
            if walk_tx.send(batch).is_err() {
                return ControlFlow::Break(());
            }
            redraw_notify.notify_one();
            ControlFlow::Continue(())
        });
    });
    (walk_rx, task)
}

/// Spawn a streaming walker that yields the workspace's directories, derived
/// from the same file walk [`spawn_workspace_walk`] uses.
///
/// Each streamed file batch maps to its ancestor directories strictly below
/// `git_root` (the root itself excluded), deduped across batches by a shared
/// set so every directory is sent once. Directories containing no files never
/// appear, an accepted limit of deriving from the file walk. Returns the
/// receiver and the task, which must be held to keep the walk alive.
pub(super) fn spawn_workspace_dir_walk(
    stoat: &Stoat,
    git_root: PathBuf,
) -> (UnboundedReceiver<Vec<PathBuf>>, Task<()>) {
    let (walk_tx, walk_rx) = tokio::sync::mpsc::unbounded_channel();
    let fs_host = stoat.fs_host.clone();
    let redraw_notify = stoat.redraw_notify.clone();
    let task = stoat.executor.spawn_blocking(move || {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        fs_host.walk_workspace_files_streaming(&git_root, &mut |batch| {
            let mut dirs = Vec::new();
            for path in batch {
                let mut ancestor = path.parent();
                while let Some(dir) = ancestor {
                    if dir == git_root || !dir.starts_with(&git_root) {
                        break;
                    }
                    if seen.insert(dir.to_path_buf()) {
                        dirs.push(dir.to_path_buf());
                    }
                    ancestor = dir.parent();
                }
            }
            if !dirs.is_empty() {
                if walk_tx.send(dirs).is_err() {
                    return ControlFlow::Break(());
                }
                redraw_notify.notify_one();
            }
            ControlFlow::Continue(())
        });
    });
    (walk_rx, task)
}

/// Handle a submit keypress while the finder is open. Returns `Some(effect)`
/// when the finder consumed the submission, `None` if no finder is open so
/// the caller can fall through to other prompt consumers.
pub(super) fn file_finder_submit(stoat: &mut Stoat) -> Option<UpdateEffect> {
    let (path, intent) = {
        let finder = stoat.file_finder.as_ref()?;
        (finder.selected_path()?.to_path_buf(), finder.open_intent)
    };
    close_file_finder(stoat);
    match intent {
        OpenIntent::Replace => {},
        OpenIntent::HSplit => {
            super::dispatch(stoat, &SplitNewDown);
        },
        OpenIntent::VSplit => {
            super::dispatch(stoat, &SplitNewRight);
        },
    }
    Some(dispatch_open_file(stoat, path))
}

/// Cancel the finder on Escape / Ctrl-C.
pub(super) fn file_finder_cancel(stoat: &mut Stoat) -> Option<UpdateEffect> {
    stoat.file_finder.as_ref()?;
    close_file_finder(stoat);
    Some(UpdateEffect::Redraw)
}

pub(super) fn file_finder_move_selection(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let Some(finder) = stoat.file_finder.as_mut() else {
        return UpdateEffect::None;
    };
    finder.move_selection(delta);
    UpdateEffect::Redraw
}

/// Page the file finder selection by half its rendered list height in `dir`
/// (-1 up, 1 down). Before the first render the viewport is unset and the
/// step falls back to a single row.
pub(super) fn file_finder_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    let Some(finder) = stoat.file_finder.as_mut() else {
        return UpdateEffect::None;
    };
    finder.active_core().page(dir);
    UpdateEffect::Redraw
}

pub(super) fn file_finder_scope_toggle(stoat: &mut Stoat) -> UpdateEffect {
    let git_host = stoat.git_host.clone();
    let Some(finder) = stoat.file_finder.as_mut() else {
        return UpdateEffect::None;
    };
    finder.toggle_scope(&*git_host);
    UpdateEffect::Redraw
}

fn dispatch_open_file(stoat: &mut Stoat, path: PathBuf) -> UpdateEffect {
    super::dispatch(stoat, &OpenFile { path })
}

/// Dispose the finder's owned editors and restore the pre-open mode.
///
/// Records the scope the finder closed in on the workspace so `space p`
/// reopens there. [`FinderScope::Buffers`] has no persisted name, so closing
/// in it leaves the prior remembered scope intact.
pub(crate) fn close_file_finder(stoat: &mut Stoat) {
    let Some(finder) = stoat.file_finder.take() else {
        return;
    };

    let active_idx = stoat.active_workspace;
    if let Some(name) = finder.scope().persist_name() {
        stoat.workspaces[active_idx].last_finder_scope = Some(name);
    }
    finder.dispose(&mut stoat.workspaces[active_idx]);
}
