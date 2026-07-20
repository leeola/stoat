use crate::{
    app::{Stoat, UpdateEffect},
    code_search::{scan_file, CodeSearchFinder, SearchMatch, MATCH_CAP},
    picker::PreviewSource,
};
use regex::Regex;
use std::{ops::ControlFlow, path::PathBuf};
use stoat_action::OpenFile;
use stoat_scheduler::Task;
use tokio::sync::mpsc::{error::TryRecvError, UnboundedReceiver};

/// An in-flight code-search scan streaming match batches from the blocking pool.
///
/// Dropping this cancels the walk, since the streaming walker stops via
/// [`ControlFlow::Break`] once its receiver is gone.
pub(crate) struct PendingCodeSearch {
    rx: UnboundedReceiver<Vec<SearchMatch>>,
    _task: Task<()>,
}

/// Open the live code-search modal over the workspace, unless one is already
/// open.
pub(crate) fn open_code_search(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.code_search.is_some() {
        return UpdateEffect::None;
    }
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let finder = CodeSearchFinder::new(ws, executor);
    stoat.code_search = Some(finder);
    UpdateEffect::Redraw
}

pub(crate) fn code_search_next(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(finder) = stoat.code_search.as_mut() {
        finder.move_selection(1);
    }
    UpdateEffect::Redraw
}

pub(crate) fn code_search_prev(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(finder) = stoat.code_search.as_mut() {
        finder.move_selection(-1);
    }
    UpdateEffect::Redraw
}

/// Close the code-search modal, disposing its input and preview. Returns whether
/// a modal was open.
pub(crate) fn close_code_search(stoat: &mut Stoat) -> bool {
    let Some(finder) = stoat.code_search.take() else {
        return false;
    };
    stoat.pending_code_search = None;
    let ws = stoat.active_workspace_mut();
    finder.dispose(ws);
    true
}

/// Open the file under the selection and jump to the match site, then close the
/// modal. An empty selection just closes. Returns whether a modal was open.
pub(crate) fn code_search_select(stoat: &mut Stoat) -> bool {
    let Some(finder) = stoat.code_search.take() else {
        return false;
    };
    stoat.pending_code_search = None;
    let target = finder.selected_match().map(|m| (m.path.clone(), m.offset));
    {
        let ws = stoat.active_workspace_mut();
        finder.dispose(ws);
    }
    if let Some((path, offset)) = target {
        super::jump::push_jump(stoat);
        super::dispatch(stoat, &OpenFile { path });
        stoat.jump_focused_to_match_offset(offset);
    }
    true
}

/// Re-arm the debounced scan when the query changed and sync the preview onto
/// the selected match.
///
/// Called from `drive_background` so typing picks up without a dedicated sync
/// action. An empty or invalid pattern clears the list without scanning.
pub(crate) fn sync_code_search(stoat: &mut Stoat) {
    if stoat.code_search.is_none() {
        return;
    }
    let query = {
        let ws = stoat.active_workspace();
        stoat
            .code_search
            .as_ref()
            .expect("code_search present")
            .input
            .text(ws)
    };
    let changed = stoat
        .code_search
        .as_ref()
        .expect("code_search present")
        .last_query
        .as_deref()
        != Some(query.as_str());
    if changed {
        {
            let finder = stoat.code_search.as_mut().expect("code_search present");
            finder.last_query = Some(query.clone());
            finder.matches.clear();
            finder.selected = 0;
        }
        stoat.set_code_search_query(query);
    }
    sync_code_search_preview(stoat);
}

fn sync_code_search_preview(stoat: &mut Stoat) {
    let selected = stoat
        .code_search
        .as_ref()
        .and_then(|finder| finder.selected_match())
        .map(|m| (m.path.clone(), m.line));

    let active_idx = stoat.active_workspace;
    let ws = &mut stoat.workspaces[active_idx];
    let fs_host = &*stoat.fs_host;
    let language_registry = &stoat.language_registry;
    let Some(finder) = stoat.code_search.as_mut() else {
        return;
    };
    match selected {
        Some((path, line)) => {
            finder
                .preview
                .sync(ws, fs_host, language_registry, PreviewSource::File(path));
            finder.preview.scroll_to_line(ws, line.saturating_sub(1));
        },
        None => finder.preview.clear(ws),
    }
}

/// Spawn the streaming workspace scan for `pattern`, rooted at `git_root`.
///
/// Returns the pending scan, or the compiled-regex error so an invalid pattern
/// never starts a walk. Each non-empty batch pings the redraw notifier so the
/// open modal repaints as matches stream in.
pub(crate) fn spawn_code_search(
    stoat: &Stoat,
    git_root: PathBuf,
    pattern: &str,
) -> Result<PendingCodeSearch, regex::Error> {
    let regex = Regex::new(pattern)?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let fs_host = stoat.fs_host.clone();
    let redraw_notify = stoat.redraw_notify.clone();
    let task = stoat.executor.spawn_blocking(move || {
        fs_host.walk_workspace_files_streaming(&git_root, &mut |batch| {
            let mut matches = Vec::new();
            for path in batch {
                scan_file(&*fs_host, &regex, &path, &mut matches);
            }
            if !matches.is_empty() {
                if tx.send(matches).is_err() {
                    return ControlFlow::Break(());
                }
                redraw_notify.notify_one();
            } else if tx.is_closed() {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });
    });
    Ok(PendingCodeSearch { rx, _task: task })
}

/// Drain streamed code-search batches into the open finder, capped at
/// [`MATCH_CAP`].
///
/// Reaching the cap drops the pending scan, which cancels the walk. Returns
/// whether a batch was drained.
pub(crate) fn pump_code_search(stoat: &mut Stoat) -> bool {
    let Some(mut pending) = stoat.pending_code_search.take() else {
        return false;
    };
    if stoat.code_search.is_none() {
        return false;
    }
    let mut drained = false;
    loop {
        match pending.rx.try_recv() {
            Ok(batch) => {
                if let Some(finder) = stoat.code_search.as_mut() {
                    finder.push_matches(batch);
                    if finder.matches.len() >= MATCH_CAP {
                        finder.matches.truncate(MATCH_CAP);
                        return true;
                    }
                }
                drained = true;
            },
            Err(TryRecvError::Empty) => {
                stoat.pending_code_search = Some(pending);
                return drained;
            },
            Err(TryRecvError::Disconnected) => {
                return true;
            },
        }
    }
}
