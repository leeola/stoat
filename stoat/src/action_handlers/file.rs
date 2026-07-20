use crate::{
    action_handlers::read_string_via_host,
    app::{Stoat, UpdateEffect},
    badge::{Anchor, Badge, BadgeSource, BadgeState},
    buffer::{BufferId, SharedBuffer},
    buffer_registry::AutoReloadMode,
    editor_state::{EditorId, EditorState},
    host::LanguageServerFeature,
    pane::{PaneId, View},
};
use lsp_types::{
    DidCloseTextDocumentParams, DidSaveTextDocumentParams, DocumentFormattingParams,
    FormattingOptions, TextDocumentIdentifier, TextEdit, Uri, WorkDoneProgressParams,
    WorkspaceEdit,
};
use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::{atomic::Ordering, Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, SystemTime},
};
use stoat_scheduler::{Executor, Task};
use stoat_text::{Bias, SelectionGoal};

/// Write the focused buffer to its backing file via
/// [`crate::host::FsHost::write_atomic`], clear the dirty flag, and notify the
/// LSP server via [`crate::host::LspHost::did_save`].
///
/// No-op for scratch buffers (no path) or when no editor is focused. Refuses to
/// write when the file changed on disk since it was opened, leaving the buffer
/// dirty and setting [`Stoat::pending_message`]. Use [`force_save_buffer`] to
/// override. Write errors likewise leave the dirty flag set and surface the
/// failure in the bottom message row rather than logging it silently.
pub(super) fn save_buffer(stoat: &mut Stoat) -> UpdateEffect {
    save_effect(save_flow(stoat, false))
}

/// Save the focused buffer even when it changed on disk since it was opened,
/// overwriting the external edit. Backs the `:w!` command. See [`save_buffer`]
/// for the guarded variant.
pub(super) fn force_save_buffer(stoat: &mut Stoat) -> UpdateEffect {
    save_effect(save_flow(stoat, true))
}

/// Save the focused buffer, then close its pane and exit when it is the last,
/// like [`Quit`](stoat_action::Quit). Backs the `:wq` command.
///
/// The quit aborts whenever the save did not land. A scratch buffer with no
/// path, a file changed on disk since it was opened, or a write error all leave
/// the app running with the failure in [`Stoat::pending_message`]. When
/// `format_on_save` defers the write, the quit is deferred too --
/// [`Stoat::quit_after_save`] arms it and [`pump_format_on_save`] quits once the
/// formatted write actually lands.
pub(super) fn write_quit(stoat: &mut Stoat) -> UpdateEffect {
    match save_flow(stoat, false) {
        SaveFlow::Wrote => {
            if super::pane::close_focused_pane(stoat) {
                UpdateEffect::Redraw
            } else {
                UpdateEffect::Quit
            }
        },
        SaveFlow::Armed | SaveFlow::AlreadyPending => {
            stoat.quit_after_save = true;
            UpdateEffect::Redraw
        },
        SaveFlow::RefusedDiskChanged | SaveFlow::Failed => UpdateEffect::Redraw,
        SaveFlow::NoTarget => {
            stoat.set_status("nothing to write; use :q to quit");
            UpdateEffect::Redraw
        },
    }
}

/// What a save attempt did, so a caller can chain on the outcome (e.g. quit
/// only once the write actually lands).
///
/// [`save_flow`] returns this; [`save_effect`] maps it back to the
/// [`UpdateEffect`] the save commands surface.
enum SaveFlow {
    /// No focused editor, or the buffer is a scratch buffer with no backing
    /// path. Nothing to save.
    NoTarget,
    /// The file changed on disk since it was opened, so a guarded save was
    /// refused and [`Stoat::pending_message`] set. `:w!` overrides.
    RefusedDiskChanged,
    /// A format-on-save request was armed. The write lands asynchronously when
    /// the request resolves, via [`pump_format_on_save`].
    Armed,
    /// A format-on-save write was already in flight, so this save was dropped.
    /// The in-flight write still lands the latest text.
    AlreadyPending,
    /// The buffer's bytes were written to disk and the dirty flag cleared.
    Wrote,
    /// The write was attempted and failed. [`Stoat::pending_message`] carries
    /// the error and the buffer stays dirty.
    Failed,
}

/// Map a [`SaveFlow`] to the [`UpdateEffect`] the save commands return.
///
/// A no-op outcome (nothing to save, or a dropped duplicate) needs no redraw;
/// every other outcome touched the message row, the buffer, or the disk.
fn save_effect(flow: SaveFlow) -> UpdateEffect {
    match flow {
        SaveFlow::NoTarget | SaveFlow::AlreadyPending => UpdateEffect::None,
        SaveFlow::RefusedDiskChanged | SaveFlow::Armed | SaveFlow::Wrote | SaveFlow::Failed => {
            UpdateEffect::Redraw
        },
    }
}

fn save_flow(stoat: &mut Stoat, force: bool) -> SaveFlow {
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return SaveFlow::NoTarget;
    };
    let buffer_id = editor.buffer_id;
    let path = match stoat.active_workspace().buffers.path_for(buffer_id) {
        Some(p) => p.to_path_buf(),
        None => return SaveFlow::NoTarget,
    };

    if !force && disk_changed_since_open(stoat, buffer_id, &path) {
        stoat.set_status("file changed on disk; use :w! to overwrite");
        return SaveFlow::RefusedDiskChanged;
    }

    if let Some(host) = format_on_save_host(stoat, buffer_id) {
        // A save already formatting drops later ones so a burst does not queue
        // duplicate writes. The in-flight one still lands the latest text.
        if stoat.pending_format_on_save.is_some() {
            return SaveFlow::AlreadyPending;
        }
        arm_format_on_save(stoat, host, buffer_id, path);
        return SaveFlow::Armed;
    }

    if write_buffer_to_disk(stoat, buffer_id, &path) {
        SaveFlow::Wrote
    } else {
        SaveFlow::Failed
    }
}

/// What a completed format-on-save request hands back to the pump.
///
/// Carries the buffer and path to write, plus the edits to apply first. The
/// edits are `None` when the server errored or the save-time budget elapsed, in
/// which case the buffer is written unchanged.
pub(crate) struct FormatOnSaveOutcome {
    buffer_id: BufferId,
    path: PathBuf,
    uri: Uri,
    edits: Option<Vec<TextEdit>>,
}

/// Save-time budget for `format_on_save`. A formatting response slower than this
/// is abandoned and the buffer is written unchanged, so a sluggish server never
/// blocks a save.
const FORMAT_ON_SAVE_BUDGET: Duration = Duration::from_millis(500);

/// The routed server that formats `buffer_id` on save, or `None` when the
/// setting is off or no capable server serves the buffer.
fn format_on_save_host(
    stoat: &Stoat,
    buffer_id: BufferId,
) -> Option<Arc<dyn crate::host::LspHost>> {
    if stoat.settings.format_on_save != Some(true) {
        return None;
    }
    stoat
        .feature_hosts(buffer_id, LanguageServerFeature::Format)
        .into_iter()
        .next()
        .map(|(_, host)| host)
}

/// Race a `textDocument/formatting` request against [`FORMAT_ON_SAVE_BUDGET`]
/// and park the outcome in [`Stoat::pending_format_on_save`] for
/// [`pump_format_on_save`]. Writes immediately without formatting when the path
/// has no `file:` URI.
fn arm_format_on_save(
    stoat: &mut Stoat,
    host: Arc<dyn crate::host::LspHost>,
    buffer_id: BufferId,
    path: PathBuf,
) {
    let Some(uri) = super::lsp::path_to_uri(&path) else {
        write_buffer_to_disk(stoat, buffer_id, &path);
        return;
    };

    let params = DocumentFormattingParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        options: FormattingOptions {
            tab_size: 4,
            insert_spaces: true,
            ..FormattingOptions::default()
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };

    let executor = stoat.executor.clone();
    let task = stoat.executor.spawn(async move {
        let format = std::pin::pin!(host.formatting(params));
        let timer = std::pin::pin!(executor.timer(FORMAT_ON_SAVE_BUDGET));
        let edits = match futures::future::select(format, timer).await {
            futures::future::Either::Left((Ok(Some(edits)), _)) if !edits.is_empty() => Some(edits),
            _ => None,
        };
        FormatOnSaveOutcome {
            buffer_id,
            path,
            uri,
            edits,
        }
    });
    stoat.pending_format_on_save = Some(task);
}

/// Poll the in-flight format-on-save request. On completion, apply any formatting
/// edits as a single-document [`WorkspaceEdit`] and then write the buffer.
/// Returns true when state changed so the caller can request a redraw.
pub(crate) fn pump_format_on_save(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_format_on_save.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(outcome) => {
            if let Some(edits) = outcome.edits {
                #[allow(clippy::mutable_key_type)]
                let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
                changes.insert(outcome.uri, edits);
                let edit = WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                };
                if let Err(err) = crate::lsp::edit_apply::apply_workspace_edit(stoat, edit) {
                    tracing::warn!(
                        target: "stoat::lsp",
                        ?err,
                        "format-on-save edit failed to apply",
                    );
                }
            }
            let wrote = write_buffer_to_disk(stoat, outcome.buffer_id, &outcome.path);
            // A `:wq` that deferred behind this write quits once it lands, but
            // only if it succeeded, so a failed deferred write leaves the buffer
            // for the user instead of exiting over unsaved changes.
            if std::mem::take(&mut stoat.quit_after_save) {
                stoat.quit_requested = wrote;
            }
            true
        },
        Poll::Pending => {
            stoat.pending_format_on_save = Some(task);
            false
        },
    }
}

/// Write `buffer_id`'s current text to `path`, clear the dirty flag, refresh the
/// recorded disk mtime, persist the saved shard, and fire the LSP `did_save`
/// notification. Reads the buffer fresh so a format-on-save edit applied just
/// before is included.
///
/// Returns `true` when the bytes landed and the buffer was marked clean, and
/// `false` when the write failed (with [`Stoat::pending_message`] set) or the
/// buffer had already vanished. A skipped `did_save` notification (an
/// unmappable path) still counts as a successful write.
fn write_buffer_to_disk(stoat: &mut Stoat, buffer_id: BufferId, path: &Path) -> bool {
    let Some(buffer) = stoat.active_workspace().buffers.get(buffer_id) else {
        return false;
    };
    let text = {
        let guard = buffer.read().expect("buffer poisoned");
        guard.rope().to_string()
    };

    if let Err(err) = stoat.fs_host.write_atomic(path, text.as_bytes()) {
        tracing::warn!(target: "stoat::file", ?err, ?path, "buffer save failed");
        stoat.set_status(format!("save failed: {err}"));
        return false;
    }
    {
        let mut guard = buffer.write().expect("buffer poisoned");
        guard.mark_clean();
    }
    if let Some(mtime) = stoat
        .fs_host
        .metadata(path)
        .ok()
        .flatten()
        .map(|m| m.modified)
    {
        stoat
            .active_workspace_mut()
            .buffers
            .set_disk_mtime(buffer_id, mtime);
    }
    stoat.persist_saved_shard(buffer_id, path, &text);
    let Some(path_str) = path.to_str() else {
        return true;
    };
    let Ok(uri) = Uri::from_str(&format!("file://{path_str}")) else {
        return true;
    };
    let params = DidSaveTextDocumentParams {
        text_document: TextDocumentIdentifier { uri },
        text: Some(text),
    };
    for lsp in stoat.hosts_for_buffer(buffer_id) {
        let params = params.clone();
        stoat
            .executor
            .spawn(async move {
                if let Err(err) = lsp.did_save(params).await {
                    tracing::warn!(target: "stoat::lsp", ?err, "did_save notification failed");
                }
            })
            .detach();
    }
    true
}

/// True when the file at `path` has an on-disk mtime newer than the baseline
/// recorded for `buffer_id` at open or last save.
///
/// A buffer with no recorded baseline (e.g. opened for a not-yet-existing file)
/// or a file whose metadata cannot be read is treated as unchanged. This
/// matches Helix, which never blocks a save it cannot justify.
fn disk_changed_since_open(stoat: &Stoat, buffer_id: BufferId, path: &Path) -> bool {
    let Some(recorded) = stoat.active_workspace().buffers.disk_mtime(buffer_id) else {
        return false;
    };
    let Some(current) = stoat
        .fs_host
        .metadata(path)
        .ok()
        .flatten()
        .map(|m| m.modified)
    else {
        return false;
    };
    current > recorded
}

/// Arm the auto-reload poll if it is not already running.
///
/// Spawns a timer loop that wakes [`Stoat::drive_background`] every
/// [`crate::app::AUTO_RELOAD_POLL`] so [`pump_auto_reload`] can re-read flagged
/// buffers. The task cancels when dropped, and [`pump_auto_reload`] drops it
/// once no buffer is flagged. Called when a buffer opts into file-following,
/// such as the session log buffer and the `:auto-reload` command.
#[allow(dead_code)]
pub(crate) fn ensure_auto_reload_poll(stoat: &mut Stoat) {
    if stoat.auto_reload_poll.is_some() {
        return;
    }
    let executor = stoat.executor.clone();
    let redraw = stoat.redraw_notify.clone();
    let tick = stoat.auto_reload_tick.clone();
    let task = stoat.executor.spawn(async move {
        loop {
            executor.timer(crate::app::AUTO_RELOAD_POLL).await;
            tick.store(true, Ordering::Relaxed);
            redraw.notify_one();
        }
    });
    stoat.auto_reload_poll = Some(task);
}

/// Re-read every auto-reload-flagged buffer whose file advanced past its
/// recorded mtime, and disarm the poll when none remain.
///
/// A dirty buffer is skipped so in-memory edits are never clobbered. When the
/// new content extends the old it is appended in place, preserving anchors for
/// the log-tail case. Otherwise the buffer is fully replaced. A cursor sitting
/// on the old last line follows to the new end, while any other cursor stays
/// put.
pub(crate) fn pump_auto_reload(stoat: &mut Stoat) {
    if stoat.auto_reload_poll.is_none() {
        return;
    }
    // Only re-stat when the poll timer has ticked since the last pump, so the
    // per-frame drive_background calls in between do no fs work.
    if !stoat.auto_reload_tick.swap(false, Ordering::Relaxed) {
        return;
    }
    let paths = stoat.active_workspace().buffers.auto_reload_paths();
    if paths.is_empty() {
        stoat.auto_reload_poll = None;
        return;
    }

    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    let mut changed = false;

    for (id, path, mode) in paths {
        let Some(buffer) = stoat.active_workspace().buffers.get(id) else {
            continue;
        };
        if buffer.read().expect("buffer poisoned").dirty {
            continue;
        }
        let Some(mtime) = stoat
            .fs_host
            .metadata(&path)
            .ok()
            .flatten()
            .map(|m| m.modified)
        else {
            continue;
        };
        if stoat.active_workspace().buffers.disk_mtime(id) == Some(mtime) {
            continue;
        }
        let Ok(new) = read_string_via_host(&*stoat.fs_host, &path) else {
            continue;
        };

        let (old_len, old_last_row, common) = {
            let guard = buffer.read().expect("buffer poisoned");
            let text = &guard.snapshot.visible_text;
            (
                text.len(),
                text.max_point().row,
                common_prefix_len(text.chunks(), &new),
            )
        };
        // The buffer is a prefix of `new` only when every one of its bytes
        // matched, which is the appended-log fast path.
        let appended = common == old_len;
        if appended && new.len() == old_len {
            stoat
                .active_workspace_mut()
                .buffers
                .set_disk_mtime(id, mtime);
            continue;
        }

        let tail_followers: Vec<EditorId> = if mode == AutoReloadMode::Tail {
            stoat
                .active_workspace_mut()
                .editors
                .iter_mut()
                .filter_map(|(eid, editor)| {
                    (editor.buffer_id == id && editor_cursor_row(editor) == old_last_row)
                        .then_some(eid)
                })
                .collect()
        } else {
            Vec::new()
        };
        let follow_offset = (mode == AutoReloadMode::Follow).then(|| {
            if appended {
                old_len
            } else {
                let mut offset = common;
                while !new.is_char_boundary(offset) {
                    offset -= 1;
                }
                offset
            }
        });

        {
            let mut guard = buffer.write().expect("buffer poisoned");
            if appended {
                guard.edit(old_len..old_len, &new[old_len..]);
            } else {
                guard.edit(0..old_len, &new);
            }
            guard.mark_clean();
        }
        stoat
            .active_workspace_mut()
            .buffers
            .set_disk_mtime(id, mtime);
        changed = true;

        let ws = stoat.active_workspace_mut();
        for eid in tail_followers {
            if let Some(editor) = ws.editors.get_mut(eid) {
                collapse_to_buffer_end(editor, scrolloff);
            }
        }
        if let Some(offset) = follow_offset {
            let follow_editors: Vec<EditorId> = ws
                .editors
                .iter()
                .filter_map(|(eid, editor)| (editor.buffer_id == id).then_some(eid))
                .collect();
            for eid in follow_editors {
                if let Some(editor) = ws.editors.get_mut(eid) {
                    collapse_to_offset(editor, offset, scrolloff);
                }
            }
        }
    }

    if changed {
        super::lsp::notify_buffer_changes_pending(stoat);
    }
}

/// Open this session's log file and follow it as new lines are written.
///
/// Resolves `stoat_log::log_dir()/stoat-<pid>.log` and delegates to
/// [`open_log_buffer`]. Reports via [`Stoat::pending_message`] when the log
/// directory cannot be resolved.
pub(super) fn open_logs(stoat: &mut Stoat) -> UpdateEffect {
    let Ok(dir) = stoat_log::log_dir() else {
        stoat.set_status("could not resolve the log directory");
        return UpdateEffect::Redraw;
    };
    let path = dir.join(format!("stoat-{}.log", std::process::id()));
    open_log_buffer(stoat, &path)
}

/// Open `path` as an auto-reloading buffer tailing its end, or report when the
/// file is absent.
///
/// The path is taken as a parameter so tests inject a fixture. When no file
/// exists there (e.g. the session logs to stderr), sets [`Stoat::pending_message`]
/// and opens nothing. Otherwise opens the file, flags it auto-reload, arms the
/// poll, and drops the focused cursor on the last line.
pub(crate) fn open_log_buffer(stoat: &mut Stoat, path: &Path) -> UpdateEffect {
    if !matches!(stoat.fs_host.metadata(path), Ok(Some(_))) {
        stoat.set_status("no log file for this session; started with --log-stderr?");
        return UpdateEffect::Redraw;
    }

    let Some(id) = open_file(stoat, path) else {
        return UpdateEffect::Redraw;
    };
    stoat
        .active_workspace_mut()
        .buffers
        .set_auto_reload(id, AutoReloadMode::Tail);
    ensure_auto_reload_poll(stoat);

    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    if let Some(editor) = super::focused_editor_mut(stoat) {
        collapse_to_buffer_end(editor, scrolloff);
    }
    UpdateEffect::Redraw
}

/// Set the focused buffer's auto-reload mode, backing `:auto-reload`.
///
/// The `state` argument is matched case-insensitively. "on" tails the file,
/// "off" disables reload, and "follow" jumps the cursor to each reload's first
/// changed region. Any other value reports the expected form and changes
/// nothing. Requesting "follow" while already following toggles it back off, so
/// one binding both starts and stops it. Enabling a scratch buffer with no
/// backing file reports that and changes nothing.
///
/// Enabling arms the poll. Disabling relies on the pump auto-disarming once no
/// buffer is set.
pub(super) fn set_buffer_auto_reload(stoat: &mut Stoat, state: &str) -> UpdateEffect {
    let requested = match state.trim().to_ascii_lowercase().as_str() {
        "on" => AutoReloadMode::Tail,
        "off" => AutoReloadMode::Off,
        "follow" => AutoReloadMode::Follow,
        _ => {
            stoat.set_status("auto-reload: expected on, off, or follow");
            return UpdateEffect::Redraw;
        },
    };

    let Some(id) = super::focused_editor_mut(stoat).map(|e| e.buffer_id) else {
        return UpdateEffect::None;
    };

    let already_following =
        stoat.active_workspace().buffers.auto_reload_mode(id) == AutoReloadMode::Follow;
    let mode = if requested == AutoReloadMode::Follow && already_following {
        AutoReloadMode::Off
    } else {
        requested
    };

    if mode != AutoReloadMode::Off && stoat.active_workspace().buffers.path_for(id).is_none() {
        stoat.set_status("buffer has no file to reload");
        return UpdateEffect::Redraw;
    }

    stoat
        .active_workspace_mut()
        .buffers
        .set_auto_reload(id, mode);
    if mode != AutoReloadMode::Off {
        ensure_auto_reload_poll(stoat);
    }
    stoat.set_status(match mode {
        AutoReloadMode::Off => "auto-reload off",
        AutoReloadMode::Tail => "auto-reload on",
        AutoReloadMode::Follow => "auto-reload follow",
    });
    UpdateEffect::Redraw
}

/// Collapse `editor`'s selection onto the end of its buffer and scroll it into
/// view, so a tailing buffer shows its newest content.
fn collapse_to_buffer_end(editor: &mut EditorState, scrolloff: u32) {
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let end = buf_snap.rope().len();
    let anchor = buf_snap.anchor_at(end, Bias::Left);
    editor.selections.transform(buf_snap, |s| {
        let mut sel = s.clone();
        sel.collapse_to(anchor, SelectionGoal::None);
        sel
    });
    super::movement::ensure_cursor_in_view(editor, scrolloff);
}

/// Collapse `editor`'s selection onto `offset` and scroll it into view, so a
/// following buffer jumps to the first region a reload changed.
fn collapse_to_offset(editor: &mut EditorState, offset: usize, scrolloff: u32) {
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let anchor = buf_snap.anchor_at(offset, Bias::Left);
    editor.selections.transform(buf_snap, |s| {
        let mut sel = s.clone();
        sel.collapse_to(anchor, SelectionGoal::None);
        sel
    });
    super::movement::ensure_cursor_in_view(editor, scrolloff);
}

/// The number of leading bytes the rope `chunks` share with `new`.
///
/// Walks the chunks byte by byte, stopping at the first mismatch or once `new`
/// is exhausted, so a followed log is diffed against the new file without ever
/// materializing the whole buffer. The result is the append splice point when
/// it equals the buffer length, and the first divergence otherwise.
fn common_prefix_len<'a>(chunks: impl Iterator<Item = &'a str>, new: &str) -> usize {
    let new = new.as_bytes();
    let mut matched = 0;
    for chunk in chunks {
        if matched == new.len() {
            break;
        }
        let chunk = chunk.as_bytes();
        let tail = &new[matched..];
        let n = chunk.len().min(tail.len());
        let common = chunk[..n]
            .iter()
            .zip(tail)
            .take_while(|(a, b)| a == b)
            .count();
        matched += common;
        if common < chunk.len() {
            break;
        }
    }
    matched
}

/// The buffer-line row of `editor`'s primary cursor, resolved through its
/// current display snapshot.
fn editor_cursor_row(editor: &mut EditorState) -> u32 {
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let head = buf_snap.resolve_anchor(&editor.selections.newest_anchor().head());
    buf_snap.rope().offset_to_point(head).row
}

/// Drop the focused buffer from the workspace's
/// [`crate::buffer_registry::BufferRegistry`] and notify the LSP
/// server via [`crate::host::LspHost::did_close`]. Editor states
/// that referenced the buffer are rebound to fresh scratch buffers
/// so panes stay coherent. Refuses to close when the buffer is
/// dirty so unsaved edits aren't silently lost.
pub(super) fn close_buffer(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let buffer_id = editor.buffer_id;
    let buffer = match stoat.active_workspace().buffers.get(buffer_id) {
        Some(b) => b,
        None => return UpdateEffect::None,
    };
    if buffer.read().expect("buffer poisoned").dirty {
        tracing::warn!(target: "stoat::file", ?buffer_id, "refusing close of dirty buffer");
        return UpdateEffect::None;
    }

    let executor = stoat.executor.clone();
    let editor_ids: Vec<EditorId> = stoat
        .active_workspace()
        .editors
        .iter()
        .filter_map(|(id, e)| (e.buffer_id == buffer_id).then_some(id))
        .collect();
    for editor_id in &editor_ids {
        let ws = stoat.active_workspace_mut();
        let (new_buffer_id, new_buffer) = ws.buffers.new_scratch();
        if let Some(slot) = ws.editors.get_mut(*editor_id) {
            *slot = EditorState::new(new_buffer_id, new_buffer, executor.clone());
        }
    }

    let path = stoat.active_workspace_mut().buffers.remove(buffer_id);

    // Purge the closed buffer from every pane's jumplist so a later walk can
    // never resolve a stale entry into it.
    let ws = stoat.active_workspace_mut();
    for pane_id in ws.panes.split_pane_ids() {
        ws.panes.pane_mut(pane_id).jumplist.remove_buffer(buffer_id);
    }

    if let Some(done) = stoat
        .active_workspace_mut()
        .editor_bridge_waiters
        .remove(&buffer_id)
    {
        let _ = done.send(());
    }
    stoat.lsp_opened.remove(&buffer_id);
    stoat.lsp_buffer_versions.remove(&buffer_id);
    stoat.lsp_pending_changes.remove(&buffer_id);
    stoat.lsp_doc_versions.remove(&buffer_id);
    stoat
        .lsp_last_delivered_text
        .lock()
        .expect("lsp text mutex")
        .remove(&buffer_id);
    stoat
        .lsp_last_delivered_buffer_version
        .lock()
        .expect("lsp version mutex")
        .remove(&buffer_id);

    if let Some(path) = path
        && let Some(path_str) = path.to_str()
        && let Ok(uri) = Uri::from_str(&format!("file://{path_str}"))
    {
        let params = DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri },
        };
        for lsp in stoat.hosts_for_buffer(buffer_id) {
            let params = params.clone();
            stoat
                .executor
                .spawn(async move {
                    if let Err(err) = lsp.did_close(params).await {
                        tracing::warn!(target: "stoat::lsp", ?err, "did_close notification failed");
                    }
                })
                .detach();
        }
    }
    UpdateEffect::Redraw
}

pub(crate) fn open_file(stoat: &mut Stoat, path: &Path) -> Option<BufferId> {
    let target = stoat.active_workspace().panes.focus();
    open_file_in_pane(stoat, target, path)
}

/// Open the user config in the focused pane.
///
/// Resolves the config path from the environment, setting a status message
/// when none can be resolved. Delegates the seed-and-open to [`open_config_at`].
pub(crate) fn open_config(stoat: &mut Stoat) {
    match crate::paths::user_config_path() {
        Some(path) => open_config_at(stoat, &path),
        None => stoat.set_status("could not resolve the user config path"),
    }
}

/// Open `path` in the focused pane, seeding it with the built-in default keymap
/// when the filesystem reports it missing.
pub(crate) fn open_config_at(stoat: &mut Stoat, path: &Path) {
    if !stoat.fs_host.exists(path) {
        if let Some(parent) = path.parent() {
            let _ = stoat.fs_host.create_dir_all(parent);
        }
        if let Err(err) = stoat
            .fs_host
            .write(path, crate::app::DEFAULT_KEYMAP.as_bytes())
        {
            tracing::error!("failed to seed user config {}: {}", path.display(), err);
        }
    }
    open_file(stoat, path);
}

/// Largest file opened synchronously on the main thread.
///
/// Files over this size read on the blocking pool and install once the read
/// finishes (see [`install_pending_opens`]), so a huge file or slow mount does
/// not stall input before first paint.
const OPEN_SYNC_MAX_BYTES: u64 = 1 << 20;

/// A large file reading on the blocking pool, awaiting install.
///
/// The task fills `result` with the read outcome and wakes the run loop;
/// [`install_pending_opens`] then finishes the open on the main thread. Held in
/// [`Stoat::pending_file_opens`] so the task is not dropped, which would cancel
/// the read, before it lands.
pub(crate) struct PendingFileOpen {
    path: PathBuf,
    target: PaneId,
    disk_mtime: Option<SystemTime>,
    _task: Task<()>,
    result: Arc<Mutex<Option<std::io::Result<String>>>>,
}

pub(crate) fn open_file_in_pane(
    stoat: &mut Stoat,
    target: PaneId,
    path: &Path,
) -> Option<BufferId> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        stoat.active_workspace().git_root.join(path)
    };

    let meta = stoat.fs_host.metadata(&absolute).ok().flatten();
    let disk_mtime = meta.map(|m| m.modified);
    if meta.map_or(0, |m| m.len) > OPEN_SYNC_MAX_BYTES {
        spawn_pending_open(stoat, target, absolute, disk_mtime);
        return None;
    }

    let content = match read_string_via_host(&*stoat.fs_host, &absolute) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "\n".to_string(),
        Err(e) => {
            tracing::error!("failed to read {}: {}", absolute.display(), e);
            return None;
        },
    };
    finish_open(stoat, target, &absolute, &content, disk_mtime)
}

/// Read `absolute` on the blocking pool and queue it for install.
///
/// A no-op if an open for the same path is already pending, so repeated opens of
/// one large file spawn a single read.
fn spawn_pending_open(
    stoat: &mut Stoat,
    target: PaneId,
    absolute: PathBuf,
    disk_mtime: Option<SystemTime>,
) {
    if stoat.pending_file_opens.iter().any(|p| p.path == absolute) {
        return;
    }

    let result: Arc<Mutex<Option<std::io::Result<String>>>> = Arc::new(Mutex::new(None));
    let task = {
        let result = result.clone();
        let fs_host = stoat.fs_host.clone();
        let redraw = stoat.redraw_notify.clone();
        let path = absolute.clone();
        stoat.executor.spawn_blocking(move || {
            let content = read_string_via_host(&*fs_host, &path);
            *result.lock().expect("pending open mutex") = Some(content);
            redraw.notify_one();
        })
    };
    stoat.pending_file_opens.push(PendingFileOpen {
        path: absolute,
        target,
        disk_mtime,
        _task: task,
        result,
    });

    let ws = stoat.active_workspace_mut();
    ws.badges.remove_by_source(BadgeSource::FileOpen);
    ws.badges.insert(Badge {
        source: BadgeSource::FileOpen,
        anchor: Anchor::BottomRight,
        state: BadgeState::Active,
        label: "opening file".to_string(),
        detail: None,
    });
}

/// Install every pending open whose read has finished.
///
/// Called from [`Stoat::drive_background`]. Drops an open whose target pane
/// vanished while it read, and clears the [`BadgeSource::FileOpen`] badge once
/// none remain.
pub(crate) fn install_pending_opens(stoat: &mut Stoat) {
    let mut ready = Vec::new();
    let mut i = 0;
    while i < stoat.pending_file_opens.len() {
        let done = stoat.pending_file_opens[i]
            .result
            .lock()
            .expect("pending open mutex")
            .is_some();
        if done {
            ready.push(stoat.pending_file_opens.remove(i));
        } else {
            i += 1;
        }
    }

    for pending in ready {
        let content = match pending.result.lock().expect("pending open mutex").take() {
            Some(Ok(c)) => c,
            Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => "\n".to_string(),
            Some(Err(e)) => {
                tracing::error!("failed to read {}: {}", pending.path.display(), e);
                continue;
            },
            None => continue,
        };
        if !stoat.active_workspace().panes.contains(pending.target) {
            continue;
        }
        finish_open(
            stoat,
            pending.target,
            &pending.path,
            &content,
            pending.disk_mtime,
        );
    }

    if stoat.pending_file_opens.is_empty() {
        stoat
            .active_workspace_mut()
            .badges
            .remove_by_source(BadgeSource::FileOpen);
    }
}

/// Open `content` as the buffer for `absolute` and show it in `target`.
///
/// The shared tail of the sync and background open paths. It registers the
/// buffer (deduping on path), applies mtime and language, notifies LSP, records
/// the pane switch, and installs the editor.
fn finish_open(
    stoat: &mut Stoat,
    target: PaneId,
    absolute: &Path,
    content: &str,
    disk_mtime: Option<SystemTime>,
) -> Option<BufferId> {
    let lang = stoat.language_registry.for_path(absolute);
    let executor = stoat.executor.clone();

    let (buffer_id, buffer) = {
        let ws = stoat.active_workspace_mut();
        let (buffer_id, buffer) = ws.buffers.open(absolute, content);
        if let Some(mtime) = disk_mtime {
            ws.buffers.set_disk_mtime(buffer_id, mtime);
        }
        if let Some(lang) = lang
            && ws.buffers.language_for(buffer_id).is_none()
        {
            ws.buffers.set_language(buffer_id, lang);
        }
        (buffer_id, buffer)
    };

    super::lsp::notify_buffer_opened(stoat, buffer_id, absolute, content);

    super::jump::record_pane_switch(stoat, target, buffer_id);
    show_buffer_in_pane(stoat, target, buffer_id, buffer, executor)
}

/// Show `buffer_id` in `target` by swapping the pane's editor to a fresh
/// [`EditorState`] over the buffer, garbage-collecting the outgoing one.
///
/// Returns early with the pane untouched when it already shows this buffer,
/// so re-showing an open buffer skips the editor swap. The buffer must
/// already be registered in the workspace. Callers that read from disk go
/// through [`open_file_in_pane`].
pub(crate) fn show_buffer_in_pane(
    stoat: &mut Stoat,
    target: PaneId,
    buffer_id: BufferId,
    buffer: SharedBuffer,
    executor: Executor,
) -> Option<BufferId> {
    let ws = stoat.active_workspace_mut();
    ws.buffers.mark_shown(buffer_id);
    if let View::Editor(eid) = ws.panes.pane(target).view
        && ws
            .editors
            .get(eid)
            .is_some_and(|e| e.buffer_id == buffer_id)
    {
        return Some(buffer_id);
    }

    let editor = ws.seeded_editor(buffer_id, buffer, executor);
    let new_editor_id = ws.editors.insert(editor);

    let old = match ws.panes.pane(target).view {
        View::Editor(eid) => Some(eid),
        _ => None,
    };
    ws.panes.pane_mut(target).view = View::Editor(new_editor_id);

    if let Some(old_id) = old {
        super::gc_editor_if_unreferenced(ws, old_id);
    }

    Some(buffer_id)
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::dispatch,
        app::UpdateEffect,
        buffer::BufferId,
        buffer_registry::AutoReloadMode,
        host::{FakeFsOp, FsHost},
        test_harness::TestHarness,
        Stoat,
    };
    use std::{
        path::{Path, PathBuf},
        sync::atomic::Ordering,
    };
    use stoat_action::{
        CloseBuffer, ForceSaveBuffer, MoveDown, OpenBuffer, OpenFile, SaveBuffer, WriteQuit,
    };

    /// Open `name` (seeded with `seed`) under `root`, dirty the buffer with a
    /// leading insert, and return its absolute path. The open records the disk
    /// mtime baseline the save guard checks against.
    fn open_edited(h: &mut TestHarness, root: &Path, name: &str, seed: &[u8]) -> PathBuf {
        let path = root.join(name);
        h.fake_fs().insert_file(&path, seed);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        buffer.write().expect("poisoned").edit(0..0, "edited ");
        path
    }

    fn focused_dirty(stoat: &Stoat) -> bool {
        let editor_id = match stoat
            .active_workspace()
            .panes
            .pane(stoat.active_workspace().panes.focus())
            .view
        {
            crate::pane::View::Editor(id) => id,
            _ => return false,
        };
        let buffer_id = stoat.active_workspace().editors[editor_id].buffer_id;
        let buffer = stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        let guard = buffer.read().expect("buffer poisoned");
        guard.dirty
    }

    #[test]
    fn large_file_opens_on_the_background_pool() {
        use crate::badge::BadgeSource;

        let mut h = TestHarness::with_size(80, 24);
        let root = Path::new("/big");
        let path = root.join("huge.txt");
        let big = vec![b'x'; super::OPEN_SYNC_MAX_BYTES as usize + 16];
        h.fake_fs().insert_file(&path, &big);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();

        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });

        assert!(
            h.stoat
                .active_workspace()
                .buffers
                .id_for_path(&path)
                .is_none(),
            "a large open defers past the synchronous dispatch"
        );
        assert!(
            h.stoat
                .active_workspace()
                .badges
                .find_by_source(BadgeSource::FileOpen)
                .is_some(),
            "the pending badge shows while the read runs"
        );

        h.settle();
        super::install_pending_opens(&mut h.stoat);

        let buffer_id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&path)
            .expect("the buffer installs once the read finishes");
        assert_eq!(
            h.stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer")
                .read()
                .expect("poisoned")
                .rope()
                .len(),
            big.len(),
            "the full file content lands in the buffer"
        );
        assert!(
            h.stoat
                .active_workspace()
                .badges
                .find_by_source(BadgeSource::FileOpen)
                .is_none(),
            "the badge clears once no open is pending"
        );
    }

    #[test]
    fn small_file_opens_synchronously() {
        let mut h = TestHarness::with_size(80, 24);
        let root = Path::new("/small");
        let path = root.join("tiny.txt");
        h.fake_fs().insert_file(&path, b"hello\n");
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();

        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });

        assert!(
            h.stoat
                .active_workspace()
                .buffers
                .id_for_path(&path)
                .is_some(),
            "a small file opens on the dispatch with no background read"
        );
    }

    /// Open `name` (seeded with `seed`) under `root`, flag it auto-reload, and
    /// arm the poll. Returns the absolute path and buffer id.
    fn open_auto_reload(
        h: &mut TestHarness,
        root: &Path,
        name: &str,
        seed: &[u8],
    ) -> (PathBuf, BufferId) {
        let path = root.join(name);
        h.fake_fs().insert_file(&path, seed);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        let id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        h.stoat
            .active_workspace_mut()
            .buffers
            .set_auto_reload(id, AutoReloadMode::Tail);
        super::ensure_auto_reload_poll(&mut h.stoat);
        (path, id)
    }

    fn buffer_text(h: &TestHarness, id: BufferId) -> String {
        h.stoat
            .active_workspace()
            .buffers
            .get(id)
            .expect("buffer")
            .read()
            .expect("poisoned")
            .snapshot
            .visible_text
            .to_string()
    }

    #[test]
    fn open_config_seeds_the_default_when_missing() {
        let mut h = TestHarness::with_size(80, 10);
        let path = PathBuf::from("/cfg/config.stcfg");

        super::open_config_at(&mut h.stoat, &path);

        let mut bytes = Vec::new();
        h.fake_fs()
            .read(&path, &mut bytes)
            .expect("the missing config was seeded");
        assert_eq!(bytes, crate::app::DEFAULT_KEYMAP.as_bytes());

        let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        assert_eq!(buffer_text(&h, buffer_id), crate::app::DEFAULT_KEYMAP);
    }

    #[test]
    fn open_config_opens_an_existing_config_unmodified() {
        let mut h = TestHarness::with_size(80, 10);
        let path = PathBuf::from("/cfg/config.stcfg");
        let custom = "format_on_save = true;\n";
        h.fake_fs().insert_file(&path, custom.as_bytes());

        super::open_config_at(&mut h.stoat, &path);

        let mut bytes = Vec::new();
        h.fake_fs()
            .read(&path, &mut bytes)
            .expect("config readable");
        assert_eq!(
            bytes,
            custom.as_bytes(),
            "an existing config is opened without being overwritten"
        );

        let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        assert_eq!(buffer_text(&h, buffer_id), custom);
    }

    fn focused_cursor_row(h: &mut TestHarness) -> u32 {
        super::editor_cursor_row(
            crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor"),
        )
    }

    /// Arm the poll tick and drive the pump, standing in for the timer that sets
    /// the tick in production.
    fn arm_and_pump(h: &mut TestHarness) {
        h.stoat.auto_reload_tick.store(true, Ordering::Relaxed);
        super::pump_auto_reload(&mut h.stoat);
    }

    #[test]
    fn pump_auto_reload_skips_until_the_poll_ticks() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-untick");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"line1\n");

        h.fake_fs().insert_file(&path, b"line1\nline2\n");
        super::pump_auto_reload(&mut h.stoat);

        assert_eq!(
            buffer_text(&h, id),
            "line1\n",
            "the pump does no fs work until the poll timer ticks"
        );
    }

    #[test]
    fn pump_auto_reload_appends_and_keeps_the_buffer_clean() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-append");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"line1\n");

        h.fake_fs().insert_file(&path, b"line1\nline2\n");
        arm_and_pump(&mut h);

        assert_eq!(buffer_text(&h, id), "line1\nline2\n");
        assert!(!focused_dirty(&h.stoat), "a reloaded buffer stays clean");
    }

    #[test]
    fn pump_auto_reload_appends_across_a_chunk_boundary() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-chunks");
        // Larger than one rope chunk, so the streaming compare walks several
        // chunks before reaching the append point.
        let seed = "abcdefghij\n".repeat(200);
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", seed.as_bytes());

        let appended = format!("{seed}tail line\n");
        h.fake_fs().insert_file(&path, appended.as_bytes());
        arm_and_pump(&mut h);

        assert_eq!(buffer_text(&h, id), appended);
        assert!(
            !focused_dirty(&h.stoat),
            "a chunk-boundary append stays clean"
        );
    }

    #[test]
    fn pump_auto_reload_replaces_on_a_mid_chunk_change() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-chunk-replace");
        let seed = "abcdefghij\n".repeat(200);
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", seed.as_bytes());

        // Flip a byte deep inside the buffer, so the compare finds a mismatch
        // mid-chunk and full-replaces rather than appending.
        let mut changed = seed.into_bytes();
        changed[1500] = b'Z';
        h.fake_fs().insert_file(&path, &changed);
        arm_and_pump(&mut h);

        assert_eq!(
            buffer_text(&h, id).into_bytes(),
            changed,
            "a mid-chunk change replaces the buffer"
        );
    }

    #[test]
    fn pump_auto_reload_skips_dirty_buffers() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-dirty");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"line1\n");
        h.stoat
            .active_workspace()
            .buffers
            .get(id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .edit(0..0, "x");

        h.fake_fs().insert_file(&path, b"line1\nline2\n");
        arm_and_pump(&mut h);

        assert_eq!(
            buffer_text(&h, id),
            "xline1\n",
            "a dirty buffer keeps its in-memory edits"
        );
    }

    #[test]
    fn pump_auto_reload_tail_follows_a_last_line_cursor() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-tail");
        // The seed has no trailing newline, so the last line "ccc" carries
        // content and a col-0 cursor on it sits before the append point. Natural
        // anchoring leaves such a cursor put. Only the tail-follow carries it to
        // the new end.
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"aaa\nbbb\nccc");
        dispatch(&mut h.stoat, &MoveDown);
        dispatch(&mut h.stoat, &MoveDown);
        assert_eq!(
            focused_cursor_row(&mut h),
            2,
            "cursor starts on the last line"
        );

        h.fake_fs().insert_file(&path, b"aaa\nbbb\nccc\nddd\n");
        arm_and_pump(&mut h);

        assert_eq!(buffer_text(&h, id), "aaa\nbbb\nccc\nddd\n");
        assert_eq!(
            focused_cursor_row(&mut h),
            4,
            "a last-line cursor follows the append to the new end"
        );
    }

    #[test]
    fn pump_auto_reload_leaves_a_mid_file_cursor_put() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-mid");
        let (path, _) = open_auto_reload(&mut h, &root, "log.txt", b"a\nb\nc\nd\n");
        assert_eq!(focused_cursor_row(&mut h), 0, "cursor starts mid-file");

        h.fake_fs().insert_file(&path, b"a\nb\nc\nd\ne\n");
        arm_and_pump(&mut h);

        assert_eq!(
            focused_cursor_row(&mut h),
            0,
            "a mid-file cursor stays put through an append"
        );
    }

    #[test]
    fn pump_auto_reload_follow_jumps_to_the_first_changed_line() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-follow-mid");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"a\nb\nc\nd\n");
        h.stoat
            .active_workspace_mut()
            .buffers
            .set_auto_reload(id, AutoReloadMode::Follow);
        assert_eq!(focused_cursor_row(&mut h), 0, "cursor starts at the top");

        h.fake_fs().insert_file(&path, b"a\nb\nCHANGED\nd\n");
        arm_and_pump(&mut h);

        assert_eq!(buffer_text(&h, id), "a\nb\nCHANGED\nd\n");
        assert_eq!(
            focused_cursor_row(&mut h),
            2,
            "follow jumps the cursor to the first changed line"
        );
        assert!(!focused_dirty(&h.stoat), "a followed reload stays clean");
    }

    #[test]
    fn pump_auto_reload_follow_jumps_to_the_appended_content() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-follow-append");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"a\nb\n");
        h.stoat
            .active_workspace_mut()
            .buffers
            .set_auto_reload(id, AutoReloadMode::Follow);
        assert_eq!(focused_cursor_row(&mut h), 0, "cursor starts at the top");

        h.fake_fs().insert_file(&path, b"a\nb\nc\nd\n");
        arm_and_pump(&mut h);

        assert_eq!(buffer_text(&h, id), "a\nb\nc\nd\n");
        assert_eq!(
            focused_cursor_row(&mut h),
            2,
            "follow jumps the cursor to the start of the appended content"
        );
    }

    #[test]
    fn pump_auto_reload_ignores_unflagged_buffers() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-unflagged");
        let path = root.join("log.txt");
        h.fake_fs().insert_file(&path, b"line1\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        let id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        super::ensure_auto_reload_poll(&mut h.stoat);

        h.fake_fs().insert_file(&path, b"line1\nline2\n");
        arm_and_pump(&mut h);

        assert_eq!(
            buffer_text(&h, id),
            "line1\n",
            "an unflagged buffer is never reloaded"
        );
    }

    #[test]
    fn pump_auto_reload_disarms_when_no_buffer_is_flagged() {
        let mut h = Stoat::test();
        super::ensure_auto_reload_poll(&mut h.stoat);
        assert!(h.stoat.auto_reload_poll.is_some(), "poll armed");

        arm_and_pump(&mut h);

        assert!(
            h.stoat.auto_reload_poll.is_none(),
            "the pump drops the poll task when no buffer is flagged"
        );
    }

    #[test]
    fn open_log_buffer_flags_auto_reload_and_tails() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/logs-open");
        let path = root.join("stoat-1.log");
        h.fake_fs().insert_file(&path, b"line1\nline2\nline3\n");
        h.stoat.active_workspace_mut().git_root = root;

        assert_eq!(
            super::open_log_buffer(&mut h.stoat, &path),
            UpdateEffect::Redraw
        );

        let id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        let flagged = h
            .stoat
            .active_workspace()
            .buffers
            .auto_reload_paths()
            .iter()
            .any(|(fid, _, _)| *fid == id);
        assert!(flagged, "the log buffer is flagged auto-reload");
        assert!(h.stoat.auto_reload_poll.is_some(), "the poll is armed");
        assert_eq!(
            focused_cursor_row(&mut h),
            3,
            "the cursor tails the last line"
        );
    }

    #[test]
    fn open_log_buffer_reports_a_missing_log_file() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/logs-missing");
        let path = root.join("stoat-1.log");
        h.stoat.active_workspace_mut().git_root = root;

        assert_eq!(
            super::open_log_buffer(&mut h.stoat, &path),
            UpdateEffect::Redraw
        );

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no log file for this session; started with --log-stderr?")
        );
        assert!(
            h.stoat.auto_reload_poll.is_none(),
            "no poll is armed for a missing log"
        );
        assert!(
            h.stoat
                .active_workspace()
                .buffers
                .id_for_path(&path)
                .is_none(),
            "no buffer is opened for a missing log"
        );
    }

    /// Open `name` (seeded with `seed`) under `root` and return its clean,
    /// unflagged buffer id.
    fn open_plain(h: &mut TestHarness, root: &Path, name: &str, seed: &[u8]) -> BufferId {
        let path = root.join(name);
        h.fake_fs().insert_file(&path, seed);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id
    }

    fn is_flagged(h: &TestHarness, id: BufferId) -> bool {
        h.stoat
            .active_workspace()
            .buffers
            .auto_reload_paths()
            .iter()
            .any(|(fid, _, _)| *fid == id)
    }

    #[test]
    fn set_buffer_auto_reload_on_flags_and_arms() {
        let mut h = Stoat::test();
        let id = open_plain(&mut h, &PathBuf::from("/ar-on"), "a.txt", b"x\n");

        assert_eq!(
            super::set_buffer_auto_reload(&mut h.stoat, "on"),
            UpdateEffect::Redraw
        );

        assert!(is_flagged(&h, id), "the focused buffer is flagged");
        assert!(h.stoat.auto_reload_poll.is_some(), "the poll is armed");
        assert_eq!(h.stoat.pending_message.as_deref(), Some("auto-reload on"));
    }

    #[test]
    fn set_buffer_auto_reload_off_clears_and_pump_disarms() {
        let mut h = Stoat::test();
        let id = open_plain(&mut h, &PathBuf::from("/ar-off"), "a.txt", b"x\n");
        super::set_buffer_auto_reload(&mut h.stoat, "on");

        assert_eq!(
            super::set_buffer_auto_reload(&mut h.stoat, "OFF"),
            UpdateEffect::Redraw
        );
        assert!(!is_flagged(&h, id), "the flag is cleared");
        assert_eq!(h.stoat.pending_message.as_deref(), Some("auto-reload off"));

        arm_and_pump(&mut h);
        assert!(
            h.stoat.auto_reload_poll.is_none(),
            "the pump drops the poll after the last flag clears"
        );
    }

    #[test]
    fn set_buffer_auto_reload_rejects_a_bogus_argument() {
        let mut h = Stoat::test();
        let id = open_plain(&mut h, &PathBuf::from("/ar-bogus"), "a.txt", b"x\n");

        assert_eq!(
            super::set_buffer_auto_reload(&mut h.stoat, "maybe"),
            UpdateEffect::Redraw
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("auto-reload: expected on, off, or follow")
        );
        assert!(!is_flagged(&h, id), "no flag changed");
        assert!(h.stoat.auto_reload_poll.is_none(), "no poll armed");
    }

    #[test]
    fn set_buffer_auto_reload_on_rejects_a_scratch_buffer() {
        let mut h = Stoat::test();

        assert_eq!(
            super::set_buffer_auto_reload(&mut h.stoat, "on"),
            UpdateEffect::Redraw
        );

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("buffer has no file to reload")
        );
        assert!(
            h.stoat.auto_reload_poll.is_none(),
            "no poll armed for a scratch buffer"
        );
    }

    #[test]
    fn set_buffer_auto_reload_follow_twice_toggles_off() {
        let mut h = Stoat::test();
        let id = open_plain(&mut h, &PathBuf::from("/ar-follow"), "a.txt", b"x\n");

        assert_eq!(
            super::set_buffer_auto_reload(&mut h.stoat, "follow"),
            UpdateEffect::Redraw
        );
        assert!(is_flagged(&h, id), "follow flags the buffer");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("auto-reload follow")
        );

        assert_eq!(
            super::set_buffer_auto_reload(&mut h.stoat, "follow"),
            UpdateEffect::Redraw
        );
        assert!(!is_flagged(&h, id), "a second follow unflags the buffer");
        assert_eq!(h.stoat.pending_message.as_deref(), Some("auto-reload off"));
    }

    #[test]
    fn set_buffer_auto_reload_follow_rejects_a_scratch_buffer() {
        let mut h = Stoat::test();

        assert_eq!(
            super::set_buffer_auto_reload(&mut h.stoat, "follow"),
            UpdateEffect::Redraw
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("buffer has no file to reload")
        );
        assert!(
            h.stoat.auto_reload_poll.is_none(),
            "no poll armed for a scratch buffer"
        );
    }

    #[test]
    fn save_buffer_writes_rope_to_path() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/save-test");
        h.fake_fs().insert_file(root.join("a.txt"), b"original\n");
        h.stoat.active_workspace_mut().git_root = root.clone();
        let path = root.join("a.txt");
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let buffer_id = editor.buffer_id;
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        {
            let mut guard = buffer.write().expect("poisoned");
            guard.edit(0..0, "edited ");
        }
        assert!(focused_dirty(&h.stoat));

        assert_eq!(dispatch(&mut h.stoat, &SaveBuffer), UpdateEffect::Redraw);

        let writes: Vec<_> = h
            .fake_fs()
            .ops()
            .into_iter()
            .filter(|op| matches!(op, FakeFsOp::WriteAtomic { .. }))
            .collect();
        assert_eq!(
            writes,
            [FakeFsOp::WriteAtomic {
                path: path.clone(),
                len: b"edited original\n".len(),
            }],
            "save must go through the atomic write path exactly once",
        );

        let mut written = Vec::new();
        h.fake_fs()
            .read(&path, &mut written)
            .expect("file readable");
        assert_eq!(written, b"edited original\n");
    }

    fn enable_format_on_save(h: &mut TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.stoat.settings.format_on_save = Some(true);
        h.fake_lsp().set_capabilities(ServerCapabilities {
            document_formatting_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    fn open_rs(h: &mut TestHarness, root: &Path, name: &str, content: &[u8]) -> PathBuf {
        let path = root.join(name);
        h.fake_fs().insert_file(&path, content);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        path
    }

    fn whole_file_edit(new_text: &str) -> lsp_types::TextEdit {
        use lsp_types::{Position, Range, TextEdit};
        TextEdit {
            range: Range::new(Position::new(0, 0), Position::new(1, 0)),
            new_text: new_text.to_string(),
        }
    }

    fn on_disk(h: &TestHarness, path: &Path) -> Vec<u8> {
        let mut buf = Vec::new();
        h.fake_fs().read(path, &mut buf).expect("file readable");
        buf
    }

    #[test]
    fn format_on_save_formats_then_writes() {
        let mut h = Stoat::test();
        enable_format_on_save(&mut h);
        let root = PathBuf::from("/fos-format");
        let path = open_rs(&mut h, &root, "a.rs", b"fn  main (){}\n");
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![whole_file_edit("fn main() {}\n")],
        );

        dispatch(&mut h.stoat, &SaveBuffer);
        h.settle();

        assert_eq!(on_disk(&h, &path), b"fn main() {}\n");
    }

    #[test]
    fn format_on_save_timeout_writes_original() {
        use std::time::Duration;
        let mut h = Stoat::test();
        enable_format_on_save(&mut h);
        let root = PathBuf::from("/fos-timeout");
        let path = open_rs(&mut h, &root, "a.rs", b"fn  main (){}\n");
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![whole_file_edit("fn main() {}\n")],
        );
        h.fake_lsp()
            .set_request_delay("textDocument/formatting", Duration::from_millis(600));

        dispatch(&mut h.stoat, &SaveBuffer);
        // The 500ms budget elapses before the delayed format returns.
        h.advance_clock(Duration::from_millis(500));
        h.settle();

        assert_eq!(on_disk(&h, &path), b"fn  main (){}\n");
    }

    #[test]
    fn format_on_save_disabled_writes_unformatted() {
        use lsp_types::{OneOf, ServerCapabilities};
        let mut h = Stoat::test();
        h.fake_lsp().set_capabilities(ServerCapabilities {
            document_formatting_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
        let root = PathBuf::from("/fos-disabled");
        let path = open_rs(&mut h, &root, "a.rs", b"fn  main (){}\n");
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![whole_file_edit("fn main() {}\n")],
        );

        dispatch(&mut h.stoat, &SaveBuffer);
        h.settle();

        assert_eq!(on_disk(&h, &path), b"fn  main (){}\n");
        assert!(h.stoat.pending_format_on_save.is_none());
    }

    #[test]
    fn save_buffer_failed_write_keeps_file_and_dirty() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/save-fail");
        h.fake_fs().insert_file(root.join("a.txt"), b"original\n");
        h.stoat.active_workspace_mut().git_root = root.clone();
        let path = root.join("a.txt");
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        {
            let mut guard = buffer.write().expect("poisoned");
            guard.edit(0..0, "edited ");
        }
        assert!(focused_dirty(&h.stoat));

        h.fake_fs()
            .fail_writes_to(&path, std::io::ErrorKind::PermissionDenied);
        assert_eq!(dispatch(&mut h.stoat, &SaveBuffer), UpdateEffect::Redraw);

        let mut written = Vec::new();
        h.fake_fs()
            .read(&path, &mut written)
            .expect("file readable");
        assert_eq!(
            written, b"original\n",
            "failed save leaves disk bytes intact"
        );
        assert!(focused_dirty(&h.stoat), "failed save keeps buffer dirty");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("save failed: /save-fail/a.txt: injected write failure"),
            "failed save surfaces an error message",
        );
    }

    #[test]
    fn snapshot_save_failure_shows_status_message() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/save-fail");
        h.fake_fs().insert_file(root.join("a.txt"), b"original\n");
        h.stoat.active_workspace_mut().git_root = root.clone();
        let path = root.join("a.txt");
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        {
            let mut guard = buffer.write().expect("poisoned");
            guard.edit(0..0, "edited ");
        }

        h.fake_fs()
            .fail_writes_to(&path, std::io::ErrorKind::PermissionDenied);
        dispatch(&mut h.stoat, &SaveBuffer);
        h.assert_snapshot("save_failure_shows_status_message");
    }

    #[test]
    fn snapshot_clean_frame_has_no_status_message() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/save-clean");
        h.fake_fs().insert_file(root.join("a.txt"), b"original\n");
        h.stoat.active_workspace_mut().git_root = root.clone();
        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("a.txt"),
            },
        );
        h.settle();
        h.assert_snapshot("clean_frame_has_no_status_message");
    }

    #[test]
    fn save_buffer_refuses_when_disk_changed() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/save-guard");
        let path = open_edited(&mut h, &root, "a.txt", b"original\n");
        h.fake_fs().insert_file(&path, b"external\n");

        assert_eq!(dispatch(&mut h.stoat, &SaveBuffer), UpdateEffect::Redraw);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("file changed on disk; use :w! to overwrite"),
        );
        assert!(focused_dirty(&h.stoat), "refused save keeps buffer dirty");
        let mut written = Vec::new();
        h.fake_fs().read(&path, &mut written).expect("readable");
        assert_eq!(written, b"external\n", "refused save leaves disk untouched");
    }

    #[test]
    fn force_save_buffer_overwrites_disk_change() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/force-guard");
        let path = open_edited(&mut h, &root, "a.txt", b"original\n");
        h.fake_fs().insert_file(&path, b"external\n");

        assert_eq!(
            dispatch(&mut h.stoat, &ForceSaveBuffer),
            UpdateEffect::Redraw
        );
        assert!(!focused_dirty(&h.stoat), "force save clears dirty");
        let mut written = Vec::new();
        h.fake_fs().read(&path, &mut written).expect("readable");
        assert_eq!(
            written, b"edited original\n",
            "force save overwrites the external edit",
        );
    }

    #[test]
    fn write_quit_saves_and_quits_the_last_pane() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/wq-save");
        let path = open_edited(&mut h, &root, "a.txt", b"original\n");

        assert_eq!(dispatch(&mut h.stoat, &WriteQuit), UpdateEffect::Quit);
        assert_eq!(
            on_disk(&h, &path),
            b"edited original\n",
            "wq wrote the buffer"
        );
        assert!(!focused_dirty(&h.stoat), "wq cleared the dirty flag");
    }

    #[test]
    fn write_quit_refuses_when_disk_changed() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/wq-guard");
        let path = open_edited(&mut h, &root, "a.txt", b"original\n");
        h.fake_fs().insert_file(&path, b"external\n");

        assert_eq!(dispatch(&mut h.stoat, &WriteQuit), UpdateEffect::Redraw);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("file changed on disk; use :w! to overwrite"),
        );
        assert_eq!(
            on_disk(&h, &path),
            b"external\n",
            "aborted wq leaves disk untouched"
        );
        assert!(focused_dirty(&h.stoat), "aborted wq keeps the buffer dirty");
    }

    #[test]
    fn write_quit_on_scratch_buffer_reports_nothing_to_write() {
        let mut h = Stoat::test();

        assert_eq!(dispatch(&mut h.stoat, &WriteQuit), UpdateEffect::Redraw);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("nothing to write; use :q to quit"),
        );
    }

    #[test]
    fn write_quit_with_format_on_save_defers_the_quit_until_the_write_lands() {
        let mut h = Stoat::test();
        enable_format_on_save(&mut h);
        let root = PathBuf::from("/wq-fos");
        let path = open_rs(&mut h, &root, "a.rs", b"fn  main (){}\n");
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![whole_file_edit("fn main() {}\n")],
        );

        assert_eq!(dispatch(&mut h.stoat, &WriteQuit), UpdateEffect::Redraw);
        assert!(
            h.stoat.quit_after_save,
            "the quit defers behind the formatted write"
        );
        assert!(!h.stoat.quit_requested);

        h.settle();

        assert_eq!(
            on_disk(&h, &path),
            b"fn main() {}\n",
            "the formatted write landed"
        );
        assert!(!h.stoat.quit_after_save, "the deferred quit is consumed");
        assert!(h.stoat.quit_requested, "the landed write requests the quit");
    }

    #[test]
    fn write_quit_deferred_write_failure_aborts_the_quit() {
        let mut h = Stoat::test();
        enable_format_on_save(&mut h);
        let root = PathBuf::from("/wq-fos-fail");
        let path = open_rs(&mut h, &root, "a.rs", b"fn  main (){}\n");
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![whole_file_edit("fn main() {}\n")],
        );
        h.fake_fs()
            .fail_writes_to(&path, std::io::ErrorKind::PermissionDenied);

        assert_eq!(dispatch(&mut h.stoat, &WriteQuit), UpdateEffect::Redraw);
        assert!(h.stoat.quit_after_save);

        h.settle();

        assert!(!h.stoat.quit_after_save, "the deferred quit is consumed");
        assert!(
            !h.stoat.quit_requested,
            "a failed deferred write aborts the quit"
        );
    }

    #[test]
    fn save_refreshes_disk_mtime_so_next_save_succeeds() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/save-restat");
        let path = open_edited(&mut h, &root, "a.txt", b"original\n");

        assert_eq!(dispatch(&mut h.stoat, &SaveBuffer), UpdateEffect::Redraw);
        assert!(!focused_dirty(&h.stoat));

        let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        buffer.write().expect("poisoned").edit(0..0, "more ");

        assert_eq!(dispatch(&mut h.stoat, &SaveBuffer), UpdateEffect::Redraw);
        assert!(
            !focused_dirty(&h.stoat),
            "second save succeeds because the first refreshed the mtime baseline",
        );
        let mut written = Vec::new();
        h.fake_fs().read(&path, &mut written).expect("readable");
        assert_eq!(written, b"more edited original\n");
    }

    #[test]
    fn save_buffer_clears_dirty_flag() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/save-dirty");
        h.fake_fs().insert_file(root.join("a.txt"), b"x");
        h.stoat.active_workspace_mut().git_root = root.clone();
        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("a.txt"),
            },
        );
        h.settle();

        let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        {
            let mut guard = buffer.write().expect("poisoned");
            guard.edit(1..1, "y");
        }
        assert!(focused_dirty(&h.stoat));

        dispatch(&mut h.stoat, &SaveBuffer);
        assert!(!focused_dirty(&h.stoat));
    }

    #[test]
    fn save_buffer_on_scratch_buffer_is_noop() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("scratch text");
        assert!(focused_dirty(&h.stoat));
        assert_eq!(dispatch(&mut h.stoat, &SaveBuffer), UpdateEffect::None);
        assert!(
            focused_dirty(&h.stoat),
            "scratch buffer dirty flag preserved when no path",
        );
    }

    fn focused_buffer_id(stoat: &mut Stoat) -> BufferId {
        crate::action_handlers::focused_editor_mut(stoat)
            .expect("editor")
            .buffer_id
    }

    #[test]
    fn open_buffer_activates_live_modified_buffer() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/open-buffer-test");
        h.fake_fs().insert_file(root.join("a.txt"), b"disk-a\n");
        h.fake_fs().insert_file(root.join("b.txt"), b"disk-b\n");
        h.stoat.active_workspace_mut().git_root = root.clone();

        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("a.txt"),
            },
        );
        h.settle();
        let a_id = focused_buffer_id(&mut h.stoat);
        {
            let buffer = h
                .stoat
                .active_workspace()
                .buffers
                .get(a_id)
                .expect("buffer");
            buffer.write().expect("poisoned").edit(0..0, "live-edit ");
        }

        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("b.txt"),
            },
        );
        h.settle();
        assert_ne!(
            focused_buffer_id(&mut h.stoat),
            a_id,
            "focus moved to b.txt"
        );

        dispatch(
            &mut h.stoat,
            &OpenBuffer {
                path: root.join("a.txt"),
            },
        );
        h.settle();
        assert_eq!(
            focused_buffer_id(&mut h.stoat),
            a_id,
            "OpenBuffer activates the existing buffer rather than creating a new one",
        );
        let text = {
            let buffer = h
                .stoat
                .active_workspace()
                .buffers
                .get(a_id)
                .expect("buffer");
            let guard = buffer.read().expect("poisoned");
            guard.rope().to_string()
        };
        assert_eq!(
            text, "live-edit disk-a\n",
            "the live in-memory edit must survive, proving no disk reload",
        );
    }

    fn open_path(h: &mut TestHarness, content: &[u8]) -> (PathBuf, BufferId) {
        let root = PathBuf::from("/close-test");
        let path = root.join("file.txt");
        h.fake_fs().insert_file(&path, content);
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        (path, buffer_id)
    }

    #[test]
    fn close_buffer_drops_buffer_from_registry() {
        let mut h = Stoat::test();
        let (_path, buffer_id) = open_path(&mut h, b"hello\n");
        assert!(h.stoat.active_workspace().buffers.get(buffer_id).is_some());
        assert_eq!(dispatch(&mut h.stoat, &CloseBuffer), UpdateEffect::Redraw);
        assert!(h.stoat.active_workspace().buffers.get(buffer_id).is_none());
    }

    #[test]
    fn close_buffer_replaces_editor_with_scratch() {
        let mut h = Stoat::test();
        let (_path, original_id) = open_path(&mut h, b"hello\n");
        dispatch(&mut h.stoat, &CloseBuffer);
        let new_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        assert_ne!(new_id, original_id);
        let new_buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(new_id)
            .expect("scratch buffer exists");
        assert_eq!(
            new_buffer.read().expect("poisoned").rope().to_string(),
            "\n"
        );
    }

    #[test]
    fn close_buffer_clears_lsp_opened() {
        let mut h = Stoat::test();
        let (_path, buffer_id) = open_path(&mut h, b"hello\n");
        assert!(h.stoat.lsp_opened.contains(&buffer_id));
        dispatch(&mut h.stoat, &CloseBuffer);
        assert!(!h.stoat.lsp_opened.contains(&buffer_id));
    }

    #[test]
    fn close_buffer_refuses_when_dirty() {
        let mut h = Stoat::test();
        let (_path, buffer_id) = open_path(&mut h, b"hello\n");
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        {
            let mut guard = buffer.write().expect("poisoned");
            guard.edit(0..0, "x");
        }
        assert_eq!(dispatch(&mut h.stoat, &CloseBuffer), UpdateEffect::None);
        assert!(
            h.stoat.active_workspace().buffers.get(buffer_id).is_some(),
            "dirty buffer should not be closed",
        );
    }

    #[test]
    fn close_buffer_on_scratch_buffer_succeeds() {
        let mut h = Stoat::test();
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let scratch_id = editor.buffer_id;
        assert!(!focused_dirty(&h.stoat));
        assert_eq!(dispatch(&mut h.stoat, &CloseBuffer), UpdateEffect::Redraw);
        assert!(h.stoat.active_workspace().buffers.get(scratch_id).is_none());
    }
}
