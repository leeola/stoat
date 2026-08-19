use crate::{
    app::{Stoat, UpdateEffect},
    code_search::{
        ast::{self, ast_scan_file, AstLang},
        read_text, scan_file, scan_paths_parallel, scan_text, CodeSearchFinder, SearchMatch,
        SearchMode, MATCH_CAP,
    },
    debounce,
    pane::View,
    picker::PreviewSource,
};
use ast_grep_core::Pattern;
use regex::Regex;
use std::{
    collections::{HashMap, HashSet},
    ops::ControlFlow,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};
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
        super::dispatch(
            stoat,
            &OpenFile {
                path: path.to_path_buf(),
            },
        );
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
        debounce::set_code_search_query(stoat, query);
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
            // An open buffer is what the match was found in when the file is
            // edited, so previewing the file would show text the match's line
            // number does not describe.
            let source = match ws.buffers.id_for_path(&path) {
                Some(id) => PreviewSource::Buffer(id),
                None => PreviewSource::File(path.to_path_buf()),
            };
            finder.preview.sync(ws, fs_host, language_registry, source);
            finder.preview.scroll_to_line(ws, line.saturating_sub(1));
        },
        None => finder.preview.clear(ws),
    }
}

/// The unsaved text of every dirty file-backed buffer, keyed by its path.
///
/// A search reads these instead of the files behind them, so a match reflects
/// what the user is looking at and its offset indexes that same text. Clean
/// buffers are left out because they equal their files, and scratch buffers
/// because they have no path a walked one could equal.
///
/// Taken once per query rather than read per file, so one scan sees one
/// consistent state of the workspace even while the user keeps typing.
fn dirty_buffer_overlay(stoat: &Stoat) -> Arc<HashMap<PathBuf, Arc<str>>> {
    let buffers = &stoat.active_workspace().buffers;
    let overlay = buffers
        .dirty_buffers()
        .into_iter()
        .filter_map(|dirty| {
            let path = dirty.path?;
            let buffer = buffers.get(dirty.id)?;
            let text = buffer
                .read()
                .expect("buffer poisoned")
                .snapshot
                .visible_text
                .to_string();
            Some((path, Arc::from(text.as_str())))
        })
        .collect();

    Arc::new(overlay)
}

/// Collects the paths a walk offers, so the rest of the modal session can scan
/// them without walking again.
///
/// Recording is all-or-nothing. A scan stops its walk as soon as its receiver
/// goes, which happens whenever the match cap truncates the list or the user
/// types again, and such a walk has seen only part of the tree. Publishing that
/// part would silently shrink every later query in the session, so a walk that
/// broke publishes nothing and the next query walks for real.
struct WalkRecorder {
    paths: Option<Vec<PathBuf>>,
}

impl WalkRecorder {
    fn new() -> Self {
        Self {
            paths: Some(Vec::new()),
        }
    }

    /// Fold `batch` into the recording, or abandon the recording if `flow` says
    /// the scan that just read `batch` gave up.
    fn record(&mut self, batch: Vec<PathBuf>, flow: ControlFlow<()>) {
        match flow {
            ControlFlow::Continue(()) => {
                if let Some(paths) = self.paths.as_mut() {
                    paths.extend(batch);
                }
            },
            ControlFlow::Break(()) => self.paths = None,
        }
    }

    /// Publish a completed walk into `cache`, leaving a broken one unpublished.
    ///
    /// Losing the race to another scan is not an error. Both walked the same
    /// tree, so either answer serves.
    fn publish(self, cache: &OnceLock<Vec<PathBuf>>) {
        if let Some(paths) = self.paths {
            let _ = cache.set(paths);
        }
    }
}

/// Spawn the streaming workspace scan for `query` under the finder's current
/// mode, rooted at `git_root`.
///
/// Returns `None` when no finder is open or the pattern does not compile, so an
/// invalid pattern never starts a walk. Regex mode scans every file; AST mode
/// scans only files of the finder's target language. Each non-empty batch pings
/// the redraw notifier so the open modal repaints as matches stream in.
///
/// The tree is walked only until one scan finishes walking it. After that the
/// session scans the files that walk found, which is why refining a query is
/// cheaper than the query before it and why a file created since then does not
/// match. See [`CodeSearchFinder::walked`].
pub(crate) fn spawn_code_search(
    stoat: &Stoat,
    git_root: PathBuf,
    query: &str,
) -> Option<PendingCodeSearch> {
    let finder = stoat.code_search.as_ref()?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let fs_host = stoat.fs_host.clone();
    let redraw_notify = stoat.redraw_notify.clone();
    let overlay = dirty_buffer_overlay(stoat);

    let walked = finder.walked.clone();

    let task = match finder.mode {
        SearchMode::Regex => {
            let regex = Regex::new(query).ok()?;
            stoat.executor.spawn_blocking(move || {
                let overlaid_seen: Mutex<HashSet<PathBuf>> = Mutex::new(HashSet::new());

                {
                    let scan_batch = |batch: &[PathBuf]| {
                        // Cloned per batch rather than shared across the
                        // scanning threads, which keeps them off one internal
                        // cache pool. The clone shares the compiled program, so
                        // it costs a refcount and amortises over the batch.
                        let regex = regex.clone();

                        // One buffer for the batch rather than one per file. The
                        // callback is shared across the scanning threads, so it
                        // cannot hold state between calls and this is as long as
                        // a buffer can live.
                        let mut read_buf = Vec::new();

                        let mut matches = Vec::new();
                        let mut overlaid = Vec::new();
                        for path in batch {
                            match overlay.get(path) {
                                Some(text) => {
                                    overlaid.push(path.clone());
                                    scan_text(&regex, text, path, &mut matches);
                                },
                                None => {
                                    scan_file(&*fs_host, &regex, path, &mut read_buf, &mut matches)
                                },
                            }
                        }

                        // Recorded per batch rather than per path, so the shared
                        // set is touched once by a thread that saw an overlaid
                        // file and not at all by one that did not.
                        if !overlaid.is_empty() {
                            overlaid_seen
                                .lock()
                                .expect("overlaid set poisoned")
                                .extend(overlaid);
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
                    };

                    match walked.get() {
                        Some(paths) => scan_paths_parallel(paths, &scan_batch),
                        None => {
                            let recorder = Mutex::new(WalkRecorder::new());
                            fs_host.walk_workspace_files_parallel(&git_root, &|batch| {
                                let flow = scan_batch(&batch);
                                recorder
                                    .lock()
                                    .expect("walk recorder poisoned")
                                    .record(batch, flow);
                                flow
                            });
                            recorder
                                .into_inner()
                                .expect("walk recorder poisoned")
                                .publish(&walked);
                        },
                    }
                }

                let overlaid_seen = overlaid_seen.into_inner().expect("overlaid set poisoned");

                // A buffer the scan never offered is one the workspace does not
                // have on disk yet, or one its ignore rules skip. It is still
                // open and still edited, so it still matches.
                let mut matches = Vec::new();
                for (path, text) in overlay.iter() {
                    if !overlaid_seen.contains(path) {
                        scan_text(&regex, text, path, &mut matches);
                    }
                }
                if !matches.is_empty() && tx.send(matches).is_ok() {
                    redraw_notify.notify_one();
                }
            })
        },
        SearchMode::Ast => {
            let lang = finder.target_lang.as_ref()?.clone();
            let ast_lang = AstLang::new(lang.clone());
            let pattern = Pattern::try_new(query, ast_lang.clone()).ok()?;
            let language_registry = stoat.language_registry.clone();
            let target_name = lang.name;
            let parse_cache = finder.parse_cache.clone();
            stoat.executor.spawn_blocking(move || {
                // Every file locks only the shard its path hashes to, so files
                // that hash apart parse at the same time while a refined query
                // still finds the parse the last one left.
                let scan = |path: &Path, text: &str, matches: &mut Vec<_>| {
                    let shard = &parse_cache[ast::parse_cache_shard(path)];
                    let mut cache = shard.lock().expect("parse cache poisoned");
                    ast_scan_file(text, &ast_lang, &pattern, path, &mut cache, matches);
                };

                let overlaid_seen: Mutex<HashSet<PathBuf>> = Mutex::new(HashSet::new());

                {
                    let scan_batch = |batch: &[PathBuf]| {
                        // One buffer for the batch rather than one per file. The
                        // callback is shared across the scanning threads, so it
                        // cannot hold state between calls and this is as long as
                        // a buffer can live.
                        let mut read_buf = Vec::new();

                        let mut matches = Vec::new();
                        let mut overlaid = Vec::new();
                        for path in batch {
                            if language_registry.for_path(path).map(|l| l.name) != Some(target_name)
                            {
                                continue;
                            }
                            match overlay.get(path) {
                                Some(text) => {
                                    overlaid.push(path.clone());
                                    scan(path, text, &mut matches);
                                },
                                None => {
                                    if let Some(text) = read_text(&*fs_host, path, &mut read_buf) {
                                        scan(path, text, &mut matches);
                                    }
                                },
                            }
                        }

                        // Recorded per batch rather than per path, so the shared
                        // set is touched once by a thread that saw an overlaid
                        // file and not at all by one that did not.
                        if !overlaid.is_empty() {
                            overlaid_seen
                                .lock()
                                .expect("overlaid set poisoned")
                                .extend(overlaid);
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
                    };

                    match walked.get() {
                        Some(paths) => scan_paths_parallel(paths, &scan_batch),
                        None => {
                            let recorder = Mutex::new(WalkRecorder::new());
                            fs_host.walk_workspace_files_parallel(&git_root, &|batch| {
                                let flow = scan_batch(&batch);
                                recorder
                                    .lock()
                                    .expect("walk recorder poisoned")
                                    .record(batch, flow);
                                flow
                            });
                            recorder
                                .into_inner()
                                .expect("walk recorder poisoned")
                                .publish(&walked);
                        },
                    }
                }

                let overlaid_seen = overlaid_seen.into_inner().expect("overlaid set poisoned");

                // A buffer the scan never offered is one the workspace does not
                // have on disk yet, or one its ignore rules skip. It is still
                // open and still edited, so it still matches.
                let mut matches = Vec::new();
                for (path, text) in overlay.iter() {
                    if !overlaid_seen.contains(path)
                        && language_registry.for_path(path).map(|l| l.name) == Some(target_name)
                    {
                        scan(path, text, &mut matches);
                    }
                }
                if !matches.is_empty() && tx.send(matches).is_ok() {
                    redraw_notify.notify_one();
                }
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

/// A walk that breaks is unreachable through the modal, since a scan under the
/// test scheduler runs inline while its receiver is still alive. Neither the
/// match cap nor a re-typed query can cut one short there, so the rule that
/// keeps a partial tree out of the cache is pinned on the recorder directly.
#[cfg(test)]
mod tests {
    use super::WalkRecorder;
    use std::{ops::ControlFlow, path::PathBuf, sync::OnceLock};

    fn batch(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_completed_walk_publishes_every_batch() {
        let mut recorder = WalkRecorder::new();
        recorder.record(batch(&["/repo/a.rs"]), ControlFlow::Continue(()));
        recorder.record(batch(&["/repo/b.rs"]), ControlFlow::Continue(()));

        let cache = OnceLock::new();
        recorder.publish(&cache);
        assert_eq!(cache.get(), Some(&batch(&["/repo/a.rs", "/repo/b.rs"])));
    }

    #[test]
    fn a_broken_walk_publishes_nothing() {
        let mut recorder = WalkRecorder::new();
        recorder.record(batch(&["/repo/a.rs"]), ControlFlow::Continue(()));
        recorder.record(batch(&["/repo/b.rs"]), ControlFlow::Break(()));

        let cache = OnceLock::new();
        recorder.publish(&cache);
        assert_eq!(cache.get(), None, "half a tree is worse than none");
    }

    /// The parallel walker's other threads finish the batch they are inside
    /// when one of them breaks, so batches keep arriving after the break.
    #[test]
    fn a_batch_after_the_break_does_not_revive_the_recording() {
        let mut recorder = WalkRecorder::new();
        recorder.record(batch(&["/repo/a.rs"]), ControlFlow::Break(()));
        recorder.record(batch(&["/repo/b.rs"]), ControlFlow::Continue(()));

        let cache = OnceLock::new();
        recorder.publish(&cache);
        assert_eq!(cache.get(), None);
    }

    #[test]
    fn publishing_into_a_filled_cache_leaves_it_alone() {
        let cache = OnceLock::new();
        let _ = cache.set(batch(&["/repo/a.rs"]));

        let mut recorder = WalkRecorder::new();
        recorder.record(batch(&["/repo/b.rs"]), ControlFlow::Continue(()));
        recorder.publish(&cache);

        assert_eq!(
            cache.get(),
            Some(&batch(&["/repo/a.rs"])),
            "the first walk to finish is the one the session keeps"
        );
    }
}
