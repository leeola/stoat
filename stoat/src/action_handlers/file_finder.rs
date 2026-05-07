use crate::{
    app::{Stoat, UpdateEffect},
    file_finder::{FileFinder, FinderScope, OpenIntent},
};
use std::path::PathBuf;
use stoat_action::{OpenFile, SplitNewDown, SplitNewRight};

/// Open the file finder. No-op if one is already open so that a second
/// `space p` keystroke cannot stack modals or reset progress the user has
/// made. Snapshots the workspace file list and the current git-modified
/// list at open time. Always restores to normal mode on close: the finder
/// is a top-level modal, so returning to a leader mode like `space`
/// surfaces a confusing secondary menu instead of a clean editor state.
pub(super) fn open_file_finder(
    stoat: &mut Stoat,
    open_intent: OpenIntent,
    initial_scope: FinderScope,
) -> UpdateEffect {
    if stoat.file_finder.is_some() {
        return UpdateEffect::None;
    }

    let previous_mode = "normal".to_string();
    let executor = stoat.executor.clone();
    let git_root = stoat.active_workspace().git_root.clone();
    let walk_task = {
        let fs_host = stoat.fs_host.clone();
        let walk_root = git_root.clone();
        let inner = executor.spawn_blocking(move || fs_host.walk_workspace_files(&walk_root));
        let redraw_notify = stoat.redraw_notify.clone();
        executor.spawn(async move {
            let paths = inner.await;
            redraw_notify.notify_one();
            paths
        })
    };
    let modified_paths = crate::file_finder::query_modified(&*stoat.git_host, &git_root);
    let buffer_paths = stoat.active_workspace().buffers.open_paths();

    let ws = stoat.active_workspace_mut();
    stoat.file_finder = Some(FileFinder::new(
        ws,
        executor,
        previous_mode,
        open_intent,
        initial_scope,
        git_root,
        walk_task,
        modified_paths,
        buffer_paths,
    ));
    stoat.mode = "prompt".into();
    UpdateEffect::Redraw
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
pub(crate) fn close_file_finder(stoat: &mut Stoat) {
    let Some(finder) = stoat.file_finder.take() else {
        return;
    };
    {
        let active_idx = stoat.active_workspace;
        finder.dispose(&mut stoat.workspaces[active_idx]);
    }
    stoat.mode = finder.previous_mode.clone();
}
