use crate::{
    action_handlers::read_string_via_host,
    apc_emit,
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    buffer_registry::AutoReloadMode,
    editor_state::{EditorId, EditorState},
    host::LanguageServerFeature,
    lsp::sync,
};
use lsp_types::{
    DidSaveTextDocumentParams, DocumentFormattingParams, TextDocumentIdentifier, TextEdit, Uri,
    WorkDoneProgressParams, WorkspaceEdit,
};
use std::{
    collections::HashMap,
    future::Future,
    ops::Range,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use stoat_text::{Bias, LineEnding, Rope, SelectionGoal};

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
        arm_format_on_save(stoat, host, buffer_id, path, force);
        return SaveFlow::Armed;
    }

    if write_buffer_to_disk(stoat, buffer_id, &path, force) {
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
    /// The buffer version the edits were computed against. The editor stays
    /// interactive for the whole request, so an edit inside the budget shifts
    /// every offset the edits name and [`pump_format_on_save`] discards them.
    version: u64,
    /// The units the formatting server reads positions in.
    encoding: crate::host::OffsetEncoding,
    /// Whether the save that armed this was a forced one, which the write needs
    /// to know because the disk-change guard runs there rather than at the
    /// command.
    force: bool,
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
    crate::lsp::hosts::feature_hosts(stoat, buffer_id, LanguageServerFeature::Format)
        .into_iter()
        .next()
        .map(|(_, host)| host)
}

/// Race a `textDocument/formatting` request against [`FORMAT_ON_SAVE_BUDGET`]
/// and park the outcome in [`Stoat::pending_format_on_save`] for
/// [`pump_format_on_save`]. Writes immediately without formatting when the path
/// has no `file:` URI.
///
/// The request goes out only after the buffer's pending `did_change` reaches the
/// server, so the server formats the text being saved rather than whatever the
/// 50ms debounce is still holding. The buffer version read here rides along on
/// the outcome and gates the edits at the pump.
fn arm_format_on_save(
    stoat: &mut Stoat,
    host: Arc<dyn crate::host::LspHost>,
    buffer_id: BufferId,
    path: PathBuf,
    force: bool,
) {
    let Some(uri) = super::lsp::path_to_uri(&path) else {
        write_buffer_to_disk(stoat, buffer_id, &path, force);
        return;
    };

    let params = DocumentFormattingParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        options: stoat.buffer_formatting_options(buffer_id),
        work_done_progress_params: WorkDoneProgressParams::default(),
    };

    let version = buffer_version(stoat, buffer_id).unwrap_or_default();
    let pending_change = sync::flush_pending_did_change(stoat, buffer_id);

    let executor = stoat.executor.clone();
    let encoding = host.offset_encoding();
    let task = stoat.executor.spawn(async move {
        if let Some(pending_change) = pending_change {
            pending_change.await;
        }

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
            version,
            encoding,
            force,
        }
    });
    stoat.pending_format_on_save = Some(task);
}

/// The buffer's edit counter, or `None` when the buffer is gone.
fn buffer_version(stoat: &Stoat, buffer_id: BufferId) -> Option<u64> {
    let buffer = stoat.active_workspace().buffers.get(buffer_id)?;
    let version = buffer.read().expect("buffer poisoned").version();
    Some(version)
}

/// Poll the in-flight format-on-save request. On completion, apply any formatting
/// edits as a single-document [`WorkspaceEdit`] and then write the buffer.
/// Returns true when state changed so the caller can request a redraw.
///
/// Edits computed against a buffer that has since changed are discarded and the
/// buffer is written as it stands. Their offsets name text that moved, and the
/// `changes` carrier they travel in has no version gate of its own, so applying
/// them corrupts what the user typed inside the save window.
pub(crate) fn pump_format_on_save(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_format_on_save.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(outcome) => {
            let current = buffer_version(stoat, outcome.buffer_id);
            if let Some(edits) = outcome.edits {
                if current == Some(outcome.version) {
                    #[allow(clippy::mutable_key_type)]
                    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
                    changes.insert(outcome.uri, edits);
                    let edit = WorkspaceEdit {
                        changes: Some(changes),
                        document_changes: None,
                        change_annotations: None,
                    };
                    if let Err(err) =
                        crate::lsp::edit_apply::apply_workspace_edit(stoat, edit, outcome.encoding)
                    {
                        tracing::warn!(
                            target: "stoat::lsp",
                            ?err,
                            "format-on-save edit failed to apply",
                        );
                    }
                } else {
                    tracing::info!(
                        target: "stoat::lsp",
                        formatted = outcome.version,
                        ?current,
                        "format-on-save edits discarded; the buffer changed while formatting",
                    );
                }
            }
            let wrote =
                write_buffer_to_disk(stoat, outcome.buffer_id, &outcome.path, outcome.force);
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
/// Unless `force`, the write refuses a file that changed on disk since the
/// buffer's baseline. The check belongs here rather than only at the save
/// command, because format-on-save defers the write by up to
/// [`FORMAT_ON_SAVE_BUDGET`] and a file can change inside that window.
///
/// Returns `true` when the bytes landed and the buffer was marked clean, and
/// `false` when the write was refused or failed (with
/// [`Stoat::pending_message`] set) or the buffer had already vanished. A
/// skipped `did_save` notification (an unmappable path) still counts as a
/// successful write.
fn write_buffer_to_disk(stoat: &mut Stoat, buffer_id: BufferId, path: &Path, force: bool) -> bool {
    let Some(buffer) = stoat.active_workspace().buffers.get(buffer_id) else {
        return false;
    };
    if !force && disk_changed_since_open(stoat, buffer_id, path) {
        stoat.set_status("file changed on disk; use :w! to overwrite");
        return false;
    }
    let text = {
        let guard = buffer.read().expect("buffer poisoned");
        guard.rope().to_string()
    };

    let ending = stoat.active_workspace().buffers.line_ending(buffer_id);
    if let Err(err) = stoat
        .fs_host
        .write_atomic(path, ending.restore(&text).as_bytes())
    {
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
    // The file on disk now matches the buffer, so this is the moment the shard
    // a later open warm-loads becomes worth writing. Extraction and both writes
    // ride the reindex pipeline off the run loop.
    if !stoat.persistence_disabled {
        let executor = stoat.executor.clone();
        let index_update_tx = stoat.index_update_tx.clone();
        let redraw_notify = stoat.redraw_notify.clone();
        stoat.active_workspace_mut().enqueue_reindex(
            &executor,
            &index_update_tx,
            &redraw_notify,
            buffer_id,
            true,
        );
    }
    maybe_apply_config_save(
        stoat,
        path,
        &text,
        crate::paths::user_config_path().as_deref(),
        crate::paths::stoatty_config_path().as_deref(),
    );
    let Some(uri) = super::lsp::path_to_uri(path) else {
        return true;
    };
    let params = DidSaveTextDocumentParams {
        text_document: TextDocumentIdentifier { uri },
        text: Some(text),
    };
    for lsp in crate::lsp::hosts::hosts_for_buffer(stoat, buffer_id) {
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

/// Re-apply the config just written to `path`, when it is one of the two this
/// editor knows about.
///
/// `text` is the bytes that were written, parsed directly rather than read back
/// from disk. Called from the single write path both a direct save and a
/// format-on-save deferred write funnel through, so either triggers the reload.
///
/// `stoat_config` and `stoatty_config` are where the two configs live, passed in
/// rather than resolved here so the decision stays independent of the
/// environment.
///
/// stoat re-applies its own config in-process. The hosting terminal's is only
/// reported to it, since the terminal owns that file and re-reads it itself.
///
/// Does nothing when `config.auto_reload` is off, or when `path` is any other
/// file.
fn maybe_apply_config_save(
    stoat: &mut Stoat,
    path: &Path,
    text: &str,
    stoat_config: Option<&Path>,
    stoatty_config: Option<&Path>,
) {
    if !stoat.settings.config_auto_reload.unwrap_or(true) {
        return;
    }
    if stoat_config == Some(path) {
        stoat.reload_user_config(text);
    } else if stoatty_config == Some(path) {
        apc_emit::emit_config_reload(stoat);
        stoat.set_status("stoatty config reloaded");
    }
}

/// Drive [`ActionKind::FontSizeInc`](stoat_action::ActionKind::FontSizeInc) and
/// its decrementing twin, stepping the hosting terminal's font size by `delta`.
///
/// Only stoatty can be asked, and only the startup ident handshake knows
/// whether one is there. Under any other terminal the frame would be swallowed
/// silently, so this reports the requirement rather than appearing to work.
pub(crate) fn font_size_step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    if !stoat.stoatty {
        stoat.set_status("font size needs stoatty");
        return UpdateEffect::Redraw;
    }

    apc_emit::emit_font_step(stoat, delta);
    UpdateEffect::None
}

/// How a path is named in a status message.
///
/// The bare file name, since the status row is one line shared with everything
/// else and the directory is rarely what distinguishes the file the user just
/// acted on. A path with no file name component falls back to all of it.
pub(crate) fn display_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
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
/// Spawns a timer loop sending a tick every [`crate::app::AUTO_RELOAD_POLL`],
/// which the run loop receives and answers by driving [`pump_auto_reload`]. The
/// tick deliberately does not wake a repaint on its own, so following an idle
/// file costs no frames. The task cancels when dropped, and
/// [`pump_auto_reload`] drops it once no buffer is flagged. Called when a buffer
/// opts into file-following, such as the session log buffer and the
/// `:auto-reload` command.
#[allow(dead_code)]
pub(crate) fn ensure_auto_reload_poll(stoat: &mut Stoat) {
    if stoat.auto_reload_poll.is_some() {
        return;
    }
    let executor = stoat.executor.clone();
    let tick = stoat.auto_reload_tx.clone();
    let task = stoat.executor.spawn(async move {
        loop {
            executor.timer(crate::app::AUTO_RELOAD_POLL).await;
            // The single-slot channel makes this wait out a busy loop rather
            // than queue ticks behind it, so a stall costs one late pass and
            // not a burst of them.
            if tick.send(()).await.is_err() {
                break;
            }
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
///
/// Returns whether anything the reader can see moved, either a buffer reloaded
/// or a status reporting a file that could not be read. A poll finding every
/// followed file unchanged returns `false`, which is what lets the run loop
/// skip the frame rather than repaint at the poll cadence.
pub(crate) fn pump_auto_reload(stoat: &mut Stoat) -> bool {
    if stoat.auto_reload_poll.is_none() {
        return false;
    }
    let paths = stoat.active_workspace().buffers.auto_reload_paths();
    if paths.is_empty() {
        stoat.auto_reload_poll = None;
        return false;
    }

    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    // Separate from `changed` because a failed read repaints the status without
    // editing the buffer, and the LSP notify below must not hear about a buffer
    // that never changed.
    let mut status_set = false;
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
        let new = match read_string_via_host(&*stoat.fs_host, &path) {
            Ok(new) => new,
            Err(e) => {
                // Recorded even though nothing was loaded, so this says the
                // version was seen and could not be used rather than that
                // nothing has been seen. Otherwise the poll re-reads and
                // re-reports it twice a second until the file changes again. A
                // later write moves the mtime and gets its own read, so a file
                // caught mid-write still recovers.
                stoat
                    .active_workspace_mut()
                    .buffers
                    .set_disk_mtime(id, mtime);
                stoat.set_status(format!("cannot reload {}: {e}", display_name(&path)));
                status_set = true;
                continue;
            },
        };
        // Before the prefix diff, which would otherwise compare the file's
        // carriage returns against a buffer that has none and call every line
        // changed. The file may also have changed style since it was opened, so
        // its current one is the honest answer.
        stoat
            .active_workspace_mut()
            .buffers
            .set_line_ending(id, LineEnding::detect(&new));
        let new = LineEnding::normalize(&new).into_owned();

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
                let (old_span, new_span) = changed_span(&guard.snapshot.visible_text, &new);
                guard.edit(old_span, &new[new_span]);
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
        sync::notify_buffer_changes_pending(stoat);
    }
    changed || status_set
}

/// Re-read the focused buffer's backing file from disk, backing `:reload` and
/// `:reload!`.
///
/// Refuses when the buffer has unsaved edits unless `force` discards them. A
/// scratch buffer with no path reports and changes nothing, as does a file that
/// no longer exists on disk (the buffer keeps its content). A successful reload
/// replaces the buffer content in one undoable edit, marks it clean, records the
/// new disk mtime, and notifies the language server. Returns [`UpdateEffect::None`]
/// only when no editor is focused. Every other path redraws.
pub(super) fn reload_focused(stoat: &mut Stoat, force: bool) -> UpdateEffect {
    let Some(id) = super::focused_editor_mut(stoat).map(|e| e.buffer_id) else {
        return UpdateEffect::None;
    };

    let Some(path) = stoat
        .active_workspace()
        .buffers
        .path_for(id)
        .map(Path::to_path_buf)
    else {
        stoat.set_status("buffer has no file to reload");
        return UpdateEffect::Redraw;
    };

    let dirty = stoat
        .active_workspace()
        .buffers
        .get(id)
        .map(|b| b.read().expect("buffer poisoned").dirty)
        .unwrap_or(false);
    if dirty && !force {
        stoat.set_status("unsaved changes; :reload! discards them");
        return UpdateEffect::Redraw;
    }

    match reload_from_disk(stoat, id, &path) {
        ReloadOutcome::Missing => stoat.set_status("file no longer exists on disk"),
        ReloadOutcome::Unchanged => stoat.set_status("already up to date"),
        ReloadOutcome::Reloaded => {
            stoat.set_status(format!("reloaded {}", display_name(&path)));
            sync::notify_buffer_changes_pending(stoat);
        },
    }
    UpdateEffect::Redraw
}

/// Re-read every open file-backed buffer from disk, backing `:reload-all` and
/// `:reload-all!`.
///
/// Each buffer reloads via [`reload_from_disk`]. The unforced form skips a
/// buffer with unsaved edits, while the forced form discards them. A file
/// missing on disk is reported and its buffer left intact, never blocking the
/// other reloads. The status summarizes the outcome by cause: how many buffers
/// reloaded, how many unsaved buffers were skipped, and how many files were
/// missing.
pub(super) fn reload_all(stoat: &mut Stoat, force: bool) -> UpdateEffect {
    let paths = stoat.active_workspace().buffers.open_paths();
    if paths.is_empty() {
        stoat.set_status("no files to reload");
        return UpdateEffect::Redraw;
    }

    let mut reloaded = 0usize;
    let mut skipped = 0usize;
    let mut missing = 0usize;

    for path in paths {
        let Some(id) = stoat.active_workspace().buffers.id_for_path(&path) else {
            continue;
        };
        let dirty = stoat
            .active_workspace()
            .buffers
            .get(id)
            .map(|b| b.read().expect("buffer poisoned").dirty)
            .unwrap_or(false);
        if dirty && !force {
            skipped += 1;
            continue;
        }
        match reload_from_disk(stoat, id, &path) {
            ReloadOutcome::Reloaded => reloaded += 1,
            ReloadOutcome::Missing => missing += 1,
            ReloadOutcome::Unchanged => {},
        }
    }

    if reloaded > 0 {
        sync::notify_buffer_changes_pending(stoat);
    }

    let mut parts: Vec<String> = Vec::new();
    if reloaded > 0 {
        parts.push(format!("reloaded {reloaded}"));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} unsaved skipped (:reload-all! discards)"));
    }
    if missing > 0 {
        parts.push(format!("{missing} missing on disk"));
    }
    let status = if parts.is_empty() {
        "all files up to date".to_string()
    } else {
        parts.join("; ")
    };
    stoat.set_status(status);
    UpdateEffect::Redraw
}

/// The disposition of a [`reload_from_disk`] attempt.
enum ReloadOutcome {
    /// The buffer content was replaced with the file's newer bytes.
    Reloaded,
    /// The file already matched the buffer. Only the clean flag and mtime
    /// baseline were refreshed.
    Unchanged,
    /// The file has no metadata or could not be read. The buffer is untouched.
    Missing,
}

/// Replace buffer `id`'s content with `path`'s current bytes, mirroring
/// [`pump_auto_reload`]'s replace arm without the cursor-follow logic.
///
/// A file that cannot be stat-ed or read yields [`ReloadOutcome::Missing`] and
/// leaves the buffer untouched. A file already byte-identical to the buffer is
/// left un-edited to avoid version churn, but the buffer is still marked clean
/// and its mtime baseline refreshed. Otherwise the whole buffer is replaced in
/// one undoable edit and marked clean.
fn reload_from_disk(stoat: &mut Stoat, id: BufferId, path: &Path) -> ReloadOutcome {
    let Some(mtime) = stoat
        .fs_host
        .metadata(path)
        .ok()
        .flatten()
        .map(|m| m.modified)
    else {
        return ReloadOutcome::Missing;
    };
    let Ok(new) = read_string_via_host(&*stoat.fs_host, path) else {
        return ReloadOutcome::Missing;
    };
    stoat
        .active_workspace_mut()
        .buffers
        .set_line_ending(id, LineEnding::detect(&new));
    let new = LineEnding::normalize(&new).into_owned();
    let Some(buffer) = stoat.active_workspace().buffers.get(id) else {
        return ReloadOutcome::Missing;
    };

    let (old_len, common) = {
        let guard = buffer.read().expect("buffer poisoned");
        let text = &guard.snapshot.visible_text;
        (text.len(), common_prefix_len(text.chunks(), &new))
    };
    let unchanged = common == old_len && new.len() == old_len;

    if unchanged {
        if buffer.read().expect("buffer poisoned").dirty {
            buffer.write().expect("buffer poisoned").mark_clean();
        }
    } else {
        let mut guard = buffer.write().expect("buffer poisoned");
        let (old_span, new_span) = changed_span(&guard.snapshot.visible_text, &new);
        guard.edit(old_span, &new[new_span]);
        guard.mark_clean();
    }
    stoat
        .active_workspace_mut()
        .buffers
        .set_disk_mtime(id, mtime);

    if unchanged {
        ReloadOutcome::Unchanged
    } else {
        ReloadOutcome::Reloaded
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

/// Set whether saving a config file re-applies it, backing
/// `:auto-reload-config`.
///
/// The `state` argument is matched case-insensitively and accepts "on" or
/// "off". Any other value reports the expected form and changes nothing.
///
/// This overrides the `config.auto_reload` setting for the running session
/// only. Reloading stoat's config restores whatever value that file sets.
pub(super) fn set_auto_reload_config(stoat: &mut Stoat, state: &str) -> UpdateEffect {
    let enabled = match state.trim().to_ascii_lowercase().as_str() {
        "on" => true,
        "off" => false,
        _ => {
            stoat.set_status("auto-reload-config: expected on or off");
            return UpdateEffect::Redraw;
        },
    };

    stoat.settings.config_auto_reload = Some(enabled);
    stoat.set_status(if enabled {
        "auto-reload-config on"
    } else {
        "auto-reload-config off"
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
    super::view::ensure_cursor_in_view(editor, scrolloff);
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
    super::view::ensure_cursor_in_view(editor, scrolloff);
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

/// The number of trailing bytes the rope `chunks` share with `new`.
///
/// `chunks` runs back to front, each chunk's own text forward, which is what
/// [`Rope::reversed_chunks_in_range`] yields. The mirror of
/// [`common_prefix_len`], and walked the same way so a followed log is never
/// materialized whole.
fn common_suffix_len<'a>(chunks: impl Iterator<Item = &'a str>, new: &str) -> usize {
    let new = new.as_bytes();
    let mut matched = 0;
    for chunk in chunks {
        if matched == new.len() {
            break;
        }
        let chunk = chunk.as_bytes();
        let head = &new[..new.len() - matched];
        let n = chunk.len().min(head.len());
        let common = chunk[chunk.len() - n..]
            .iter()
            .rev()
            .zip(head.iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        matched += common;
        if common < chunk.len() {
            break;
        }
    }
    matched
}

/// The span of `old` that `new` changes, paired with the span of `new` that
/// replaces it.
///
/// Rewriting a buffer wholesale deletes every fragment the anchors point into,
/// and a deleted fragment resolves to the same offset for all of them, so
/// cursors, marks, jumplist entries, and folds all collapse together. Trimming
/// the shared prefix and suffix leaves the edit covering only what moved, and
/// anchors outside it ride through untouched. Anchors inside it still collapse
/// to its boundary.
///
/// Both splits land on char boundaries in both texts. A boundary in one is not
/// a boundary in the other, since the byte at the split can differ.
fn changed_span(old: &Rope, new: &str) -> (Range<usize>, Range<usize>) {
    let mut prefix = common_prefix_len(old.chunks(), new);
    while prefix > 0 && !(old.is_char_boundary(prefix) && new.is_char_boundary(prefix)) {
        prefix -= 1;
    }

    // The prefix already claimed its bytes, so the suffix can only take what is
    // left of the shorter text. Without the clamp the two spans would overlap
    // and describe more text than either holds.
    let mut suffix = common_suffix_len(old.reversed_chunks_in_range(0..old.len()), new)
        .min(old.len().min(new.len()) - prefix);
    while suffix > 0
        && !(old.is_char_boundary(old.len() - suffix) && new.is_char_boundary(new.len() - suffix))
    {
        suffix -= 1;
    }

    (prefix..old.len() - suffix, prefix..new.len() - suffix)
}

/// The buffer-line row of `editor`'s primary cursor, resolved through its
/// current display snapshot.
fn editor_cursor_row(editor: &mut EditorState) -> u32 {
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let head = buf_snap.resolve_anchor(&editor.selections.newest_anchor().head());
    buf_snap.rope().offset_to_point(head).row
}

pub(crate) fn open_file(stoat: &mut Stoat, path: &Path) -> Option<BufferId> {
    let target = stoat.active_workspace().panes.focus();
    crate::buffer_lifecycle::open_file_in_pane(stoat, target, path)
}

/// Open a user config in the focused pane.
///
/// [`None`] and `"stoat"` open stoat's own config, `"stoatty"` opens the
/// hosting terminal's. Any other target, or a path that does not resolve, only
/// sets a status message. Delegates the seed-and-open to [`open_config_at`].
pub(crate) fn open_config(stoat: &mut Stoat, target: Option<&str>) {
    let (path, seed) = match target {
        None | Some("stoat") => (crate::paths::user_config_path(), crate::app::DEFAULT_KEYMAP),
        Some("stoatty") => (
            crate::paths::stoatty_config_path(),
            crate::app::DEFAULT_STOATTY_CONFIG,
        ),
        Some(_) => {
            stoat.set_status("open-config: expected stoat or stoatty");
            return;
        },
    };

    match path {
        Some(path) => open_config_at(stoat, &path, seed),
        None => stoat.set_status("could not resolve the user config path"),
    }
}

/// Open `path` in the focused pane, writing `seed` to it first when the
/// filesystem reports it missing.
pub(crate) fn open_config_at(stoat: &mut Stoat, path: &Path, seed: &str) {
    if !stoat.fs_host.exists(path) {
        if let Some(parent) = path.parent() {
            let _ = stoat.fs_host.create_dir_all(parent);
        }
        if let Err(err) = stoat.fs_host.write(path, seed.as_bytes()) {
            tracing::error!("failed to seed user config {}: {}", path.display(), err);
        }
    }
    open_file(stoat, path);
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::dispatch,
        app::UpdateEffect,
        buffer::BufferId,
        buffer_registry::AutoReloadMode,
        host::{FakeFsOp, FsHost},
        lsp::sync,
        test_harness::{editor, TestHarness},
        Stoat,
    };
    use std::path::{Path, PathBuf};
    use stoat_action::{ForceSaveBuffer, MoveDown, OpenFile, SaveBuffer, WriteQuit};
    use stoatty_protocol::command;

    /// Latin-1 bytes, which are not valid UTF-8.
    const NOT_UTF8: &[u8] = b"caf\xe9 au lait\n";

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

    #[test]
    fn auto_reload_reports_undecodable_content_once_per_change() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-latin1");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"line1\n");

        h.fake_fs().insert_file(&path, NOT_UTF8);
        pump_poll(&mut h);

        assert_eq!(
            buffer_text(&h, id),
            "line1\n",
            "the buffer keeps what it had"
        );
        let message = h.stoat.pending_message.as_deref().unwrap_or("");
        assert!(
            message.contains("log.txt") && message.contains("utf-8"),
            "following must say why it stopped, got {message:?}"
        );

        // Polling twice a second, a version that cannot be decoded must not be
        // re-read and re-reported until the file changes again.
        h.stoat.pending_message = None;
        pump_poll(&mut h);
        assert_eq!(
            h.stoat.pending_message, None,
            "the same undecodable version must not report again"
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

        super::open_config_at(&mut h.stoat, &path, crate::app::DEFAULT_KEYMAP);

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

        super::open_config_at(&mut h.stoat, &path, crate::app::DEFAULT_KEYMAP);

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

    #[test]
    fn open_config_seeds_the_stoatty_default_when_missing() {
        let mut h = TestHarness::with_size(80, 10);
        let path = PathBuf::from("/cfg/stoatty/config.toml");

        super::open_config_at(&mut h.stoat, &path, crate::app::DEFAULT_STOATTY_CONFIG);

        let mut bytes = Vec::new();
        h.fake_fs()
            .read(&path, &mut bytes)
            .expect("the missing stoatty config was seeded");
        assert_eq!(bytes, crate::app::DEFAULT_STOATTY_CONFIG.as_bytes());

        let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        assert_eq!(
            buffer_text(&h, buffer_id),
            crate::app::DEFAULT_STOATTY_CONFIG
        );
    }

    #[test]
    fn open_config_opens_an_existing_stoatty_config_unmodified() {
        let mut h = TestHarness::with_size(80, 10);
        let path = PathBuf::from("/cfg/stoatty/config.toml");
        let custom = "font_size = 22\n";
        h.fake_fs().insert_file(&path, custom.as_bytes());

        super::open_config_at(&mut h.stoat, &path, crate::app::DEFAULT_STOATTY_CONFIG);

        let mut bytes = Vec::new();
        h.fake_fs()
            .read(&path, &mut bytes)
            .expect("config readable");
        assert_eq!(
            bytes,
            custom.as_bytes(),
            "an existing stoatty config is opened without being overwritten"
        );
    }

    #[test]
    fn open_config_rejects_an_unknown_target() {
        let mut h = TestHarness::with_size(80, 10);
        let before = h.fake_fs().ops().len();

        super::open_config(&mut h.stoat, Some("emacs"));

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("open-config: expected stoat or stoatty"),
        );
        assert_eq!(
            h.fake_fs().ops().len(),
            before,
            "an unknown target touches the filesystem not at all"
        );
    }

    fn focused_cursor_row(h: &mut TestHarness) -> u32 {
        super::editor_cursor_row(
            crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor"),
        )
    }

    /// Drive one poll pass, standing in for the tick the timer sends in
    /// production, and report whether the pump asked for a repaint.
    fn pump_poll(h: &mut TestHarness) -> bool {
        super::pump_auto_reload(&mut h.stoat)
    }

    /// The run loop paints a poll tick only when the pump reports one, so a
    /// buffer following an idle file has to cost no frames.
    #[test]
    fn only_the_poll_that_reloads_asks_for_a_repaint() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-quiet");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"line1\n");

        assert!(!pump_poll(&mut h), "an unchanged file asks for no repaint");

        h.fake_fs().insert_file(&path, b"line1\nline2\n");

        assert!(pump_poll(&mut h), "the reload asks for a repaint");
        assert_eq!(buffer_text(&h, id), "line1\nline2\n");
        assert!(
            !pump_poll(&mut h),
            "the poll after the reload lands asks for no repaint",
        );
    }

    #[test]
    fn pump_auto_reload_appends_and_keeps_the_buffer_clean() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-append");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"line1\n");

        h.fake_fs().insert_file(&path, b"line1\nline2\n");
        pump_poll(&mut h);

        assert_eq!(buffer_text(&h, id), "line1\nline2\n");
        assert!(
            !editor::focused_dirty(&h.stoat),
            "a reloaded buffer stays clean"
        );
    }

    #[test]
    fn pump_auto_reload_normalizes_before_the_prefix_diff() {
        // The re-read is compared against the buffer to find the common prefix.
        // Carriage returns still in the disk text diverge on the first line, so
        // an ordinary append would be read as a whole-file rewrite, and the
        // buffer would take the carriage returns as content.
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-crlf");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"line1\r\n");

        h.fake_fs().insert_file(&path, b"line1\r\nline2\r\n");
        pump_poll(&mut h);

        assert_eq!(buffer_text(&h, id), "line1\nline2\n");
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
        pump_poll(&mut h);

        assert_eq!(buffer_text(&h, id), appended);
        assert!(
            !editor::focused_dirty(&h.stoat),
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
        pump_poll(&mut h);

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
        pump_poll(&mut h);

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
        pump_poll(&mut h);

        assert_eq!(buffer_text(&h, id), "aaa\nbbb\nccc\nddd\n");
        assert_eq!(
            focused_cursor_row(&mut h),
            4,
            "a last-line cursor follows the append to the new end"
        );
    }

    /// Park the cursor, a buffer-local mark, and a jumplist entry on row 3, so
    /// a reload can be checked against three anchors that ride the same edit.
    fn anchor_row_3(h: &mut TestHarness, id: BufferId) {
        let offset = {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            let snapshot = editor.display_map.snapshot();
            let offset = snapshot
                .buffer_snapshot()
                .rope()
                .point_to_offset(stoat_text::Point::new(3, 0));
            super::collapse_to_offset(editor, offset, 0);
            offset
        };

        let anchor = {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            let snapshot = editor.display_map.snapshot();
            snapshot
                .buffer_snapshot()
                .anchor_at(offset, stoat_text::Bias::Right)
        };
        h.stoat.marks.insert((id, 'a'), anchor);
        crate::action_handlers::jump::push_jump(&mut h.stoat);
    }

    /// The rows the cursor, the mark, and the jumplist entry parked by
    /// [`anchor_row_3`] now resolve to.
    fn anchored_rows(h: &mut TestHarness, id: BufferId) -> (u32, u32, u32) {
        let cursor = focused_cursor_row(h);

        let mark = *h.stoat.marks.get(&(id, 'a')).expect("mark stored");
        let jump = {
            let ws = h.stoat.active_workspace();
            let pane = ws.panes.pane(ws.panes.focus());
            let entry = pane.jumplist.entries().last().expect("jump recorded");
            entry.selections.first().expect("selection").start
        };

        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf = snapshot.buffer_snapshot();
        let row_of = |anchor| buf.rope().offset_to_point(buf.resolve_anchor(&anchor)).row;
        (cursor, row_of(mark), row_of(jump))
    }

    /// Replacing the whole buffer deletes every fragment the anchors point
    /// into, and they all resolve to the same place afterwards. Editing only
    /// the changed span leaves everything below it where it was.
    #[test]
    fn pump_auto_reload_keeps_anchors_below_a_mid_file_change() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-anchors");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"l0\nl1\nl2\nl3\nl4\n");
        anchor_row_3(&mut h, id);
        assert_eq!(
            anchored_rows(&mut h, id),
            (3, 3, 3),
            "all three start on row 3"
        );

        h.fake_fs().insert_file(&path, b"l0\nCHANGED\nl2\nl3\nl4\n");
        pump_poll(&mut h);

        assert_eq!(buffer_text(&h, id), "l0\nCHANGED\nl2\nl3\nl4\n");
        assert_eq!(anchored_rows(&mut h, id), (3, 3, 3));
    }

    #[test]
    fn reload_keeps_anchors_below_a_mid_file_change() {
        let mut h = Stoat::test();
        let root = Path::new("/reload-anchors");
        let path = root.join("a.txt");
        let id = open_plain(&mut h, root, "a.txt", b"l0\nl1\nl2\nl3\nl4\n");
        anchor_row_3(&mut h, id);

        h.fake_fs().insert_file(&path, b"l0\nCHANGED\nl2\nl3\nl4\n");
        super::reload_focused(&mut h.stoat, false);

        assert_eq!(buffer_text(&h, id), "l0\nCHANGED\nl2\nl3\nl4\n");
        assert_eq!(anchored_rows(&mut h, id), (3, 3, 3));
    }

    #[test]
    fn pump_auto_reload_leaves_a_mid_file_cursor_put() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-mid");
        let (path, _) = open_auto_reload(&mut h, &root, "log.txt", b"a\nb\nc\nd\n");
        assert_eq!(focused_cursor_row(&mut h), 0, "cursor starts mid-file");

        h.fake_fs().insert_file(&path, b"a\nb\nc\nd\ne\n");
        pump_poll(&mut h);

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
        pump_poll(&mut h);

        assert_eq!(buffer_text(&h, id), "a\nb\nCHANGED\nd\n");
        assert_eq!(
            focused_cursor_row(&mut h),
            2,
            "follow jumps the cursor to the first changed line"
        );
        assert!(
            !editor::focused_dirty(&h.stoat),
            "a followed reload stays clean"
        );
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
        pump_poll(&mut h);

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
        pump_poll(&mut h);

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

        pump_poll(&mut h);

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

        pump_poll(&mut h);
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
    fn reload_replaces_a_clean_buffer_and_advances_the_mtime() {
        let mut h = Stoat::test();
        let path = Path::new("/reload-clean").join("a.txt");
        let id = open_plain(&mut h, Path::new("/reload-clean"), "a.txt", b"old\n");
        let before = h.stoat.active_workspace().buffers.disk_mtime(id);

        h.fake_fs().insert_file(&path, b"new\n");
        let effect = super::reload_focused(&mut h.stoat, false);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(buffer_text(&h, id), "new\n");
        assert!(
            !editor::focused_dirty(&h.stoat),
            "a reloaded buffer is clean"
        );
        assert!(
            h.stoat.active_workspace().buffers.disk_mtime(id) > before,
            "the reload records the advanced disk mtime",
        );
    }

    #[test]
    fn reload_on_unchanged_disk_reports_up_to_date() {
        let mut h = Stoat::test();
        let id = open_plain(&mut h, Path::new("/reload-same"), "a.txt", b"same\n");

        let effect = super::reload_focused(&mut h.stoat, false);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("already up to date")
        );
        assert_eq!(buffer_text(&h, id), "same\n");
    }

    #[test]
    fn reload_unforced_refuses_a_dirty_buffer() {
        let mut h = Stoat::test();
        open_edited(&mut h, Path::new("/reload-dirty"), "a.txt", b"disk\n");
        let id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;

        let effect = super::reload_focused(&mut h.stoat, false);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("unsaved changes; :reload! discards them"),
        );
        assert_eq!(
            buffer_text(&h, id),
            "edited disk\n",
            "the unsaved edits survive"
        );
        assert!(editor::focused_dirty(&h.stoat), "the buffer stays dirty");
    }

    #[test]
    fn reload_forced_discards_edits_and_reloads() {
        let mut h = Stoat::test();
        let path = open_edited(&mut h, Path::new("/reload-force"), "a.txt", b"disk\n");
        let id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        h.fake_fs().insert_file(&path, b"changed\n");

        let effect = super::reload_focused(&mut h.stoat, true);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(buffer_text(&h, id), "changed\n");
        assert!(
            !editor::focused_dirty(&h.stoat),
            "the forced reload clears the dirty flag"
        );
    }

    #[test]
    fn reload_reports_a_file_missing_on_disk() {
        let mut h = Stoat::test();
        let path = Path::new("/reload-gone").join("a.txt");
        let id = open_plain(&mut h, Path::new("/reload-gone"), "a.txt", b"content\n");

        h.fake_fs().remove_file(&path).expect("remove");
        let effect = super::reload_focused(&mut h.stoat, false);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("file no longer exists on disk"),
        );
        assert_eq!(
            buffer_text(&h, id),
            "content\n",
            "the buffer keeps its content"
        );
    }

    #[test]
    fn reload_rejects_a_scratch_buffer() {
        let mut h = Stoat::test();

        let effect = super::reload_focused(&mut h.stoat, false);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("buffer has no file to reload"),
        );
    }

    fn buffer_dirty(h: &TestHarness, id: BufferId) -> bool {
        h.stoat
            .active_workspace()
            .buffers
            .get(id)
            .expect("buffer")
            .read()
            .expect("poisoned")
            .dirty
    }

    #[test]
    fn reload_all_unforced_reloads_clean_skips_dirty_reports_missing() {
        let mut h = Stoat::test();
        let root = Path::new("/reload-all");
        let clean = root.join("clean.txt");
        let gone = root.join("gone.txt");
        let clean_id = open_plain(&mut h, root, "clean.txt", b"aaa\n");
        open_edited(&mut h, root, "dirty.txt", b"bbb\n");
        let dirty_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        let gone_id = open_plain(&mut h, root, "gone.txt", b"ccc\n");

        h.fake_fs().insert_file(&clean, b"AAA\n");
        h.fake_fs().remove_file(&gone).expect("remove");

        let effect = super::reload_all(&mut h.stoat, false);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(
            buffer_text(&h, clean_id),
            "AAA\n",
            "the clean buffer reloads"
        );
        assert_eq!(
            buffer_text(&h, dirty_id),
            "edited bbb\n",
            "the dirty buffer keeps its edits",
        );
        assert_eq!(
            buffer_text(&h, gone_id),
            "ccc\n",
            "the missing buffer keeps its content",
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("reloaded 1; 1 unsaved skipped (:reload-all! discards); 1 missing on disk"),
        );
    }

    #[test]
    fn reload_all_forced_replaces_dirty_and_still_reports_missing() {
        let mut h = Stoat::test();
        let root = Path::new("/reload-all-force");
        let dirty = root.join("dirty.txt");
        let gone = root.join("gone.txt");
        open_edited(&mut h, root, "dirty.txt", b"bbb\n");
        let dirty_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        let gone_id = open_plain(&mut h, root, "gone.txt", b"ccc\n");

        h.fake_fs().insert_file(&dirty, b"BBB\n");
        h.fake_fs().remove_file(&gone).expect("remove");

        let effect = super::reload_all(&mut h.stoat, true);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(
            buffer_text(&h, dirty_id),
            "BBB\n",
            "the forced reload replaces the dirty buffer",
        );
        assert!(
            !buffer_dirty(&h, dirty_id),
            "the forced reload clears the dirty flag",
        );
        assert_eq!(
            buffer_text(&h, gone_id),
            "ccc\n",
            "the missing buffer keeps its content",
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("reloaded 1; 1 missing on disk"),
        );
    }

    #[test]
    fn reload_all_reports_up_to_date_when_nothing_changed() {
        let mut h = Stoat::test();
        let root = Path::new("/reload-all-same");
        open_plain(&mut h, root, "a.txt", b"aaa\n");
        open_plain(&mut h, root, "b.txt", b"bbb\n");

        let effect = super::reload_all(&mut h.stoat, false);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("all files up to date"),
        );
    }

    #[test]
    fn space_r_reloads_all_and_returns_to_normal() {
        let mut h = Stoat::test();
        let root = Path::new("/space-r");
        let path = root.join("a.txt");
        let id = open_plain(&mut h, root, "a.txt", b"before\n");

        h.fake_fs().insert_file(&path, b"after\n");
        h.type_keys("space r");

        assert_eq!(
            buffer_text(&h, id),
            "after\n",
            "space r reloads the buffer from disk",
        );
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "space r returns to normal mode",
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
    fn set_auto_reload_config_flips_the_setting_both_ways() {
        let mut h = Stoat::test();

        assert_eq!(
            super::set_auto_reload_config(&mut h.stoat, "off"),
            UpdateEffect::Redraw
        );
        assert_eq!(h.stoat.settings.config_auto_reload, Some(false));
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("auto-reload-config off")
        );

        assert_eq!(
            super::set_auto_reload_config(&mut h.stoat, "ON"),
            UpdateEffect::Redraw
        );
        assert_eq!(h.stoat.settings.config_auto_reload, Some(true));
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("auto-reload-config on")
        );
    }

    #[test]
    fn set_auto_reload_config_rejects_a_bogus_argument() {
        let mut h = Stoat::test();
        let before = h.stoat.settings.config_auto_reload;

        assert_eq!(
            super::set_auto_reload_config(&mut h.stoat, "sometimes"),
            UpdateEffect::Redraw
        );

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("auto-reload-config: expected on or off")
        );
        assert_eq!(
            h.stoat.settings.config_auto_reload, before,
            "a bad argument leaves the setting untouched"
        );
    }

    /// A config naming a theme, setting format_on_save, and binding a key that
    /// the default keymap leaves free in normal mode.
    const RELOADED_CONFIG: &str = "theme swapped { ui.text.fg = \"#abcdef\"; }\n\
         on init { theme = swapped; format_on_save = true; }\n\
         on key { mode == normal { F2 -> SaveBuffer(); } }\n";

    fn binds_f2(stoat: &Stoat) -> bool {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let state = crate::keymap_state::StoatKeymapState::from_stoat(stoat);
        stoat
            .keymap
            .lookup(&state, &KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE))
            .is_some()
    }

    #[test]
    fn config_save_swaps_the_keymap_settings_and_theme() {
        let mut h = Stoat::test();
        let config = PathBuf::from("/cfg/config.stcfg");
        assert!(!binds_f2(&h.stoat), "F2 is free in the default keymap");

        super::maybe_apply_config_save(
            &mut h.stoat,
            &config,
            RELOADED_CONFIG,
            Some(config.as_path()),
            None,
        );

        assert_eq!(h.stoat.settings.format_on_save, Some(true), "settings swap");
        assert_eq!(h.stoat.theme.name, "swapped", "theme swaps");
        assert!(binds_f2(&h.stoat), "the new binding resolves");
        assert_eq!(h.stoat.pending_message.as_deref(), Some("config reloaded"));
    }

    #[test]
    fn config_save_with_a_syntax_error_keeps_the_running_config() {
        let mut h = Stoat::test();
        let config = PathBuf::from("/cfg/config.stcfg");
        let theme_before = h.stoat.theme.name.clone();

        super::maybe_apply_config_save(
            &mut h.stoat,
            &config,
            "on init { format_on_save = ",
            Some(config.as_path()),
            None,
        );

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("config parse failed; keeping the current config")
        );
        assert_eq!(h.stoat.theme.name, theme_before, "the theme is untouched");
        assert!(!binds_f2(&h.stoat), "no keymap was swapped in");
    }

    #[test]
    fn config_save_does_not_reload_when_auto_reload_is_off() {
        let mut h = Stoat::test();
        let config = PathBuf::from("/cfg/config.stcfg");
        h.stoat.settings.config_auto_reload = Some(false);
        let before = h.stoat.settings.format_on_save;

        super::maybe_apply_config_save(
            &mut h.stoat,
            &config,
            RELOADED_CONFIG,
            Some(config.as_path()),
            None,
        );

        assert_eq!(
            h.stoat.settings.format_on_save, before,
            "settings untouched"
        );
        assert!(!binds_f2(&h.stoat), "keymap untouched");
        assert_eq!(h.stoat.pending_message, None);
    }

    #[test]
    fn saving_a_file_other_than_the_config_does_not_reload() {
        let mut h = Stoat::test();
        let config = PathBuf::from("/cfg/config.stcfg");
        let before = h.stoat.settings.format_on_save;

        super::maybe_apply_config_save(
            &mut h.stoat,
            &PathBuf::from("/src/main.rs"),
            RELOADED_CONFIG,
            Some(config.as_path()),
            None,
        );

        assert_eq!(
            h.stoat.settings.format_on_save, before,
            "settings untouched"
        );
        assert!(!binds_f2(&h.stoat), "keymap untouched");
        assert_eq!(h.stoat.pending_message, None);
    }

    /// Save `path` through the config hook with an APC channel installed, and
    /// return every command the emit put on the wire.
    fn config_save_commands(
        h: &mut TestHarness,
        path: &Path,
        stoatty_config: &Path,
    ) -> Vec<command::Command> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        super::maybe_apply_config_save(&mut h.stoat, path, "", None, Some(stoatty_config));

        let mut bytes = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            bytes.extend_from_slice(&batch);
        }
        command::decode_stream(&bytes)
    }

    /// Dispatch `action` with an APC channel installed, and return every
    /// command the handler put on the wire.
    fn commands_from(
        h: &mut TestHarness,
        action: &dyn stoat_action::Action,
    ) -> Vec<command::Command> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        dispatch(&mut h.stoat, action);

        let mut bytes = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            bytes.extend_from_slice(&batch);
        }
        command::decode_stream(&bytes)
    }

    #[test]
    fn font_size_actions_step_the_terminal_under_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();

        assert_eq!(
            commands_from(&mut h, &stoat_action::FontSizeInc),
            vec![Command::FontStep { delta: 1 }],
            "inc asks for one size larger"
        );
        assert_eq!(
            commands_from(&mut h, &stoat_action::FontSizeDec),
            vec![Command::FontStep { delta: -1 }],
            "and dec for one smaller"
        );
        assert_eq!(
            h.stoat.pending_message, None,
            "a terminal that can act on it needs no explanation"
        );
    }

    /// A foreign terminal swallows the frame, so the command has to say why
    /// nothing happened rather than appear to work.
    #[test]
    fn font_size_actions_report_the_requirement_outside_stoatty() {
        let mut h = Stoat::test();
        h.stoat.stoatty = false;

        assert_eq!(
            commands_from(&mut h, &stoat_action::FontSizeInc),
            Vec::new(),
            "nothing goes on the wire"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("font size needs stoatty")
        );
    }

    #[test]
    fn saving_the_stoatty_config_emits_a_reload() {
        let mut h = Stoat::test();
        let stoatty_config = PathBuf::from("/cfg/stoatty/config.toml");

        let commands = config_save_commands(&mut h, &stoatty_config, &stoatty_config);

        assert_eq!(
            commands,
            vec![command::Command::ConfigReload],
            "exactly one reload reaches the terminal"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("stoatty config reloaded")
        );
    }

    #[test]
    fn saving_the_stoatty_config_emits_nothing_when_auto_reload_is_off() {
        let mut h = Stoat::test();
        let stoatty_config = PathBuf::from("/cfg/stoatty/config.toml");
        h.stoat.settings.config_auto_reload = Some(false);

        let commands = config_save_commands(&mut h, &stoatty_config, &stoatty_config);

        assert!(commands.is_empty(), "no reload is emitted: {commands:?}");
        assert_eq!(h.stoat.pending_message, None);
    }

    #[test]
    fn saving_another_file_emits_no_stoatty_reload() {
        let mut h = Stoat::test();
        let stoatty_config = PathBuf::from("/cfg/stoatty/config.toml");

        let commands =
            config_save_commands(&mut h, &PathBuf::from("/src/main.rs"), &stoatty_config);

        assert!(commands.is_empty(), "no reload is emitted: {commands:?}");
        assert_eq!(h.stoat.pending_message, None);
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
        assert!(editor::focused_dirty(&h.stoat));

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
    fn format_on_save_discards_edits_computed_before_a_keystroke() {
        // The editor stays interactive while the request runs, so a keystroke
        // inside the budget moves every offset the parked edits name.
        use std::time::Duration;
        let mut h = Stoat::test();
        enable_format_on_save(&mut h);
        let root = PathBuf::from("/fos-stale");
        let path = open_rs(&mut h, &root, "a.rs", b"fn  main (){}\n");
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![whole_file_edit("fn main() {}\n")],
        );
        h.fake_lsp()
            .set_request_delay("textDocument/formatting", Duration::from_millis(100));

        dispatch(&mut h.stoat, &SaveBuffer);
        h.edit_focused(0..0, "// typed\n");
        h.advance_clock(Duration::from_millis(200));

        let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        assert_eq!(
            buffer_text(&h, buffer_id),
            "// typed\nfn  main (){}\n",
            "the stale format must not rewrite what the user typed",
        );
        assert_eq!(
            on_disk(&h, &path),
            b"// typed\nfn  main (){}\n",
            "the write must land the buffer as it stands",
        );
    }

    #[test]
    fn format_on_save_flushes_the_pending_did_change_first() {
        // Without the flush the edit sits in the 50ms debounce and the server
        // formats the text it was told about last, which is one edit behind.
        use lsp_types::TextDocumentSyncKind;
        let mut h = Stoat::test();
        enable_format_on_save(&mut h);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::FULL);
        let root = PathBuf::from("/fos-flush");
        let path = open_rs(&mut h, &root, "a.rs", b"fn  main (){}\n");
        h.edit_focused(0..0, "// typed\n");
        sync::notify_buffer_changes_pending(&mut h.stoat);

        dispatch(&mut h.stoat, &SaveBuffer);
        h.settle();

        let texts: Vec<String> = h
            .fake_lsp()
            .observed_changes()
            .into_iter()
            .flat_map(|change| change.content_changes)
            .map(|content| content.text)
            .collect();
        assert_eq!(
            texts,
            ["// typed\nfn  main (){}\n"],
            "the save must deliver the edit before it asks the server to format",
        );
        assert_eq!(on_disk(&h, &path), b"// typed\nfn  main (){}\n");
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
        assert!(editor::focused_dirty(&h.stoat));

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
        assert!(
            editor::focused_dirty(&h.stoat),
            "failed save keeps buffer dirty"
        );
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
        assert!(
            editor::focused_dirty(&h.stoat),
            "refused save keeps buffer dirty"
        );
        let mut written = Vec::new();
        h.fake_fs().read(&path, &mut written).expect("readable");
        assert_eq!(written, b"external\n", "refused save leaves disk untouched");
    }

    #[test]
    fn a_crlf_file_saves_back_as_crlf_unchanged() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/crlf-roundtrip");
        let path = open_rs(&mut h, &root, "a.rs", b"one\r\ntwo\r\n");

        dispatch(&mut h.stoat, &ForceSaveBuffer);
        h.settle();

        assert_eq!(on_disk(&h, &path), b"one\r\ntwo\r\n");
    }

    /// Restore the workspace `h` last saved into a fresh one, the way
    /// `--continue` does.
    fn restore_session(h: &mut TestHarness) {
        let state_path = PathBuf::from("/state/session.ron");
        h.stoat
            .active_workspace()
            .save_state(&state_path, &*h.stoat.fs_host)
            .expect("save state");

        let target = h.create_workspace();
        h.set_active_workspace(target);
        h.stoat.spawn_workspace_restore(target, state_path);
        h.settle();
        h.stoat.drive_background();
    }

    /// The buffer normalised the terminators away on open, so nothing but the
    /// session record says the file was CRLF. A restore that forgets rewrites
    /// every line of the file on the first save.
    #[test]
    fn a_restored_crlf_file_still_saves_back_as_crlf() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/crlf-restore");
        let path = open_rs(&mut h, &root, "a.rs", b"one\r\ntwo\r\n");

        restore_session(&mut h);

        dispatch(&mut h.stoat, &ForceSaveBuffer);
        h.settle();

        assert_eq!(on_disk(&h, &path), b"one\r\ntwo\r\n");
    }

    /// The guard compares against the mtime recorded when the file was read, so
    /// a restore that forgets it leaves every restored buffer overwriting
    /// whatever else touched the file meanwhile.
    #[test]
    fn a_restored_buffer_still_refuses_a_save_over_a_disk_change() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/mtime-restore");
        let path = open_rs(&mut h, &root, "a.txt", b"original\n");

        restore_session(&mut h);
        h.fake_fs().insert_file(&path, b"external\n");

        dispatch(&mut h.stoat, &SaveBuffer);
        h.settle();

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("file changed on disk; use :w! to overwrite"),
        );
        assert_eq!(on_disk(&h, &path), b"external\n");
    }

    #[test]
    fn a_crlf_buffer_holds_no_carriage_returns() {
        // The whole point of normalizing on load is that nothing downstream has
        // to know about them. A carriage return left in the buffer is
        // addressable content. The cursor stops on it, and appending at a line
        // end lands between it and the newline.
        let mut h = Stoat::test();
        let root = PathBuf::from("/crlf-buffer");
        open_rs(&mut h, &root, "a.rs", b"one\r\ntwo\r\n");

        let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        let text = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .read()
            .expect("poisoned")
            .rope()
            .to_string();
        assert_eq!(text, "one\ntwo\n");
    }

    #[test]
    fn an_edited_crlf_file_saves_uniformly_crlf() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/crlf-edit");
        let path = open_rs(&mut h, &root, "a.rs", b"one\r\ntwo\r\n");

        let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .edit(0..0, "zero\n");

        dispatch(&mut h.stoat, &ForceSaveBuffer);
        h.settle();

        assert_eq!(on_disk(&h, &path), b"zero\r\none\r\ntwo\r\n");
    }

    #[test]
    fn an_lf_file_is_left_alone() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/lf-untouched");
        let path = open_rs(&mut h, &root, "a.rs", b"one\ntwo\n");

        dispatch(&mut h.stoat, &ForceSaveBuffer);
        h.settle();

        assert_eq!(on_disk(&h, &path), b"one\ntwo\n");
    }

    #[test]
    fn reloading_a_crlf_file_keeps_the_buffer_in_lf() {
        // `:e` re-reads from disk straight into the buffer, so it needs the
        // same normalization the open path does.
        let mut h = Stoat::test();
        let root = PathBuf::from("/crlf-reload");
        let path = open_rs(&mut h, &root, "a.rs", b"one\r\ntwo\r\n");
        h.fake_fs().insert_file(&path, b"one\r\ntwo\r\nthree\r\n");

        super::reload_focused(&mut h.stoat, true);
        h.settle();

        let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        let text = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .read()
            .expect("poisoned")
            .rope()
            .to_string();
        assert_eq!(text, "one\ntwo\nthree\n");
    }

    #[test]
    fn reopening_a_buffer_keeps_the_baseline_it_was_opened_with() {
        // Re-opening a path already in the registry hands back the edited
        // buffer and drops the text just read. Taking the mtime that came with
        // that discarded read would move the baseline onto the external write,
        // and the guard would then be comparing it against itself.
        let mut h = Stoat::test();
        let root = PathBuf::from("/reopen-guard");
        let path = open_edited(&mut h, &root, "a.txt", b"original\n");
        h.fake_fs().insert_file(&path, b"external\n");

        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        assert_eq!(dispatch(&mut h.stoat, &SaveBuffer), UpdateEffect::Redraw);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("file changed on disk; use :w! to overwrite"),
        );
        assert_eq!(
            on_disk(&h, &path),
            b"external\n",
            "a save after reopening must still refuse to clobber the disk"
        );
    }

    #[test]
    fn a_format_on_save_write_refuses_a_change_that_landed_while_formatting() {
        // The save-time guard runs before the format request goes out, and the
        // write lands up to the format budget later. A file changing inside
        // that window is invisible to a check that already happened.
        let mut h = Stoat::test();
        enable_format_on_save(&mut h);
        let root = PathBuf::from("/fos-guard");
        let path = open_rs(&mut h, &root, "a.rs", b"fn  main (){}\n");
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![whole_file_edit("fn main() {}\n")],
        );

        dispatch(&mut h.stoat, &SaveBuffer);
        h.fake_fs().insert_file(&path, b"external\n");
        h.settle();

        assert_eq!(
            on_disk(&h, &path),
            b"external\n",
            "the deferred write must re-check before landing"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("file changed on disk; use :w! to overwrite"),
        );
    }

    #[test]
    fn a_forced_format_on_save_write_still_overwrites() {
        // Forcing the save has to survive the trip through the format request,
        // or :w! would start refusing whenever format-on-save is enabled.
        let mut h = Stoat::test();
        enable_format_on_save(&mut h);
        let root = PathBuf::from("/fos-force");
        let path = open_rs(&mut h, &root, "a.rs", b"fn  main (){}\n");
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![whole_file_edit("fn main() {}\n")],
        );

        dispatch(&mut h.stoat, &ForceSaveBuffer);
        h.fake_fs().insert_file(&path, b"external\n");
        h.settle();

        assert_eq!(on_disk(&h, &path), b"fn main() {}\n");
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
        assert!(!editor::focused_dirty(&h.stoat), "force save clears dirty");
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
        assert!(
            !editor::focused_dirty(&h.stoat),
            "wq cleared the dirty flag"
        );
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
        assert!(
            editor::focused_dirty(&h.stoat),
            "aborted wq keeps the buffer dirty"
        );
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
        assert!(!editor::focused_dirty(&h.stoat));

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
            !editor::focused_dirty(&h.stoat),
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
        assert!(editor::focused_dirty(&h.stoat));

        dispatch(&mut h.stoat, &SaveBuffer);
        assert!(!editor::focused_dirty(&h.stoat));
    }

    /// A raw path holding a space parses as no URI at all. A notification built
    /// that way reaches no server, and the work a save drives never fires.
    #[test]
    fn a_save_names_the_document_by_the_uri_the_open_registered() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/save uri");
        let path = root.join("my file.txt");
        h.fake_fs().insert_file(&path, b"x");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        dispatch(&mut h.stoat, &SaveBuffer);
        h.settle();

        let saved: Vec<String> = h
            .fake_lsp()
            .observed_saves()
            .iter()
            .map(|params| params.text_document.uri.as_str().to_string())
            .collect();
        let opened: Vec<String> = h
            .fake_lsp()
            .observed_opens()
            .iter()
            .map(|params| params.text_document.uri.as_str().to_string())
            .collect();
        assert_eq!(opened, ["file:///save%20uri/my%20file.txt"]);
        assert_eq!(saved, opened);
    }

    #[test]
    fn save_buffer_on_scratch_buffer_is_noop() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("scratch text");
        assert!(editor::focused_dirty(&h.stoat));
        assert_eq!(dispatch(&mut h.stoat, &SaveBuffer), UpdateEffect::None);
        assert!(
            editor::focused_dirty(&h.stoat),
            "scratch buffer dirty flag preserved when no path",
        );
    }
}
