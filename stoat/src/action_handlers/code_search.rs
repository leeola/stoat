use crate::{
    app::{Stoat, UpdateEffect},
    code_search::{
        ast::{ast_scan_file, AstLang},
        scan_file, CodeSearchFinder, SearchMatch, SearchMode, MATCH_CAP,
    },
    pane::View,
    picker::PreviewSource,
};
use ast_grep_core::Pattern;
use regex::Regex;
use std::{ops::ControlFlow, path::PathBuf, sync::Arc};
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
    let target_lang = focused_buffer_language(stoat);
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let finder = CodeSearchFinder::new(ws, executor, target_lang);
    stoat.code_search = Some(finder);
    UpdateEffect::Redraw
}

/// The language of the focused editor's buffer, or `None` when focus is not on a
/// path-bound editor. Resolves the AST-mode target language at finder open.
fn focused_buffer_language(stoat: &Stoat) -> Option<Arc<stoat_language::Language>> {
    let ws = stoat.active_workspace();
    let View::Editor(editor_id) = &ws.panes.pane(ws.panes.focus()).view else {
        return None;
    };
    let editor = ws.editors.get(*editor_id)?;
    ws.buffers.language_for(editor.buffer_id)
}

/// Flip between regex and AST search, clearing the current results and re-arming
/// the scan.
///
/// Toggling to AST with no resolvable target language is a no-op, since AST mode
/// needs a language to parse patterns against.
pub(crate) fn code_search_mode_toggle(stoat: &mut Stoat) -> UpdateEffect {
    let Some(finder) = stoat.code_search.as_mut() else {
        return UpdateEffect::None;
    };
    let next = match finder.mode {
        SearchMode::Regex if finder.target_lang.is_some() => SearchMode::Ast,
        SearchMode::Regex => return UpdateEffect::Redraw,
        SearchMode::Ast => SearchMode::Regex,
    };
    finder.mode = next;
    finder.matches.clear();
    finder.selected = 0;
    finder.invalid_pattern = false;
    // Force the next sync to treat the query as changed so it re-arms under the
    // new mode.
    finder.last_query = None;
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

/// Page the code-search selection by half its rendered list height in `dir`
/// (-1 up, 1 down). Before the first render the viewport is unset and the step
/// falls back to a single row.
pub(crate) fn code_search_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    if let Some(finder) = stoat.code_search.as_mut() {
        finder.page(dir);
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

/// Spawn the streaming workspace scan for `query` under the finder's current
/// mode, rooted at `git_root`.
///
/// Returns `None` when no finder is open or the pattern does not compile, so an
/// invalid pattern never starts a walk. Regex mode scans every file; AST mode
/// scans only files of the finder's target language. Each non-empty batch pings
/// the redraw notifier so the open modal repaints as matches stream in.
pub(crate) fn spawn_code_search(
    stoat: &Stoat,
    git_root: PathBuf,
    query: &str,
) -> Option<PendingCodeSearch> {
    let finder = stoat.code_search.as_ref()?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let fs_host = stoat.fs_host.clone();
    let redraw_notify = stoat.redraw_notify.clone();

    let task = match finder.mode {
        SearchMode::Regex => {
            let regex = Regex::new(query).ok()?;
            stoat.executor.spawn_blocking(move || {
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
            })
        },
        SearchMode::Ast => {
            let lang = finder.target_lang.as_ref()?.clone();
            let ast_lang = AstLang::new(lang.clone());
            let pattern = Pattern::try_new(query, ast_lang.clone()).ok()?;
            let language_registry = stoat.language_registry.clone();
            let target_name = lang.name;
            stoat.executor.spawn_blocking(move || {
                fs_host.walk_workspace_files_streaming(&git_root, &mut |batch| {
                    let mut matches = Vec::new();
                    for path in batch {
                        if language_registry.for_path(&path).map(|l| l.name) != Some(target_name) {
                            continue;
                        }
                        let mut buf = Vec::new();
                        if fs_host.read(&path, &mut buf).is_ok()
                            && let Ok(text) = std::str::from_utf8(&buf)
                        {
                            ast_scan_file(text, &ast_lang, &pattern, &path, &mut matches);
                        }
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
            })
        },
    };
    Some(PendingCodeSearch { rx, _task: task })
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
