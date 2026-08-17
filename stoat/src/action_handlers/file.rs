use crate::{
    apc_emit,
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    host::LanguageServerFeature,
    lsp::sync,
};
use lsp_types::{
    DidSaveTextDocumentParams, DocumentFormattingParams, TextDocumentIdentifier,
    TextDocumentSyncCapability, TextDocumentSyncSaveOptions, TextEdit, Uri, WorkDoneProgressParams,
    WorkspaceEdit,
};
use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{mpsc, Arc},
    task::{Context, Poll},
    time::Duration,
};
use stoat_text::Rope;

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

    // A save already on its way to disk drops later ones, the same rule a save
    // already formatting follows. The in-flight one still lands the text it
    // took, and a buffer that moved since keeps its dirty flag.
    if stoat.pending_save.is_some() {
        return SaveFlow::AlreadyPending;
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

    match write_buffer_to_disk(stoat, buffer_id, &path, force) {
        WriteOutcome::Wrote => SaveFlow::Wrote,
        WriteOutcome::Armed => SaveFlow::Armed,
        WriteOutcome::Failed => SaveFlow::Failed,
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
            // A `:wq` that deferred behind this write quits once it lands, but
            // only if it succeeded, so a failed deferred write leaves the buffer
            // for the user instead of exiting over unsaved changes. An armed
            // write leaves the flag for its own pump to consume.
            match write_buffer_to_disk(stoat, outcome.buffer_id, &outcome.path, outcome.force) {
                WriteOutcome::Armed => {},
                WriteOutcome::Wrote => {
                    if std::mem::take(&mut stoat.quit_after_save) {
                        stoat.quit_requested = true;
                    }
                },
                WriteOutcome::Failed => {
                    if std::mem::take(&mut stoat.quit_after_save) {
                        stoat.quit_requested = false;
                    }
                },
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
/// [`Stoat::pending_message`] set) or the buffer had already vanished. A save
/// that notifies no server still counts as a successful write, whether the path
/// maps to no URI or no server asked for the notification.
fn write_buffer_to_disk(
    stoat: &mut Stoat,
    buffer_id: BufferId,
    path: &Path,
    force: bool,
) -> WriteOutcome {
    let Some(buffer) = stoat.active_workspace().buffers.get(buffer_id) else {
        return WriteOutcome::Failed;
    };
    if !force && disk_changed_since_open(stoat, buffer_id, path) {
        stoat.set_status("file changed on disk; use :w! to overwrite");
        return WriteOutcome::Failed;
    }

    // A config is re-applied from the bytes just written, and both files are
    // small, so deferring one puts a reload behind a thread hop for nothing.
    // Every other save streams off the run loop.
    if !is_config_path(path) {
        arm_pending_save(stoat, buffer_id, path);
        return WriteOutcome::Armed;
    }

    let rope = {
        let guard = buffer.read().expect("buffer poisoned");
        guard.rope().clone()
    };
    let text = rope.to_string();

    let ending = stoat.active_workspace().buffers.line_ending(buffer_id);
    if let Err(err) = stoat
        .fs_host
        .write_atomic(path, ending.restore(&text).as_bytes())
    {
        tracing::warn!(target: "stoat::file", ?err, ?path, "buffer save failed");
        stoat.set_status(format!("save failed: {err}"));
        return WriteOutcome::Failed;
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
    notify_did_save(stoat, buffer_id, path, &rope);
    WriteOutcome::Wrote
}

/// Tell every language server mirroring `buffer_id` that `path` was written.
///
/// A server states in its sync options whether it wants save notifications at
/// all and whether it wants the saved text with them. A server that asked for
/// neither hears nothing, and `text` is materialized once per server that asked
/// for it rather than once for the whole fan-out.
///
/// A path that maps to no URI notifies nobody.
fn notify_did_save(stoat: &Stoat, buffer_id: BufferId, path: &Path, text: &Rope) {
    let Some(uri) = super::lsp::path_to_uri(path) else {
        return;
    };
    for lsp in crate::lsp::hosts::hosts_for_buffer(stoat, buffer_id) {
        let Some(include_text) = save_notification_text(&lsp.capabilities().text_document_sync)
        else {
            continue;
        };
        let params = DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            text: include_text.then(|| text.to_string()),
        };
        stoat
            .executor
            .spawn(async move {
                if let Err(err) = lsp.did_save(params).await {
                    tracing::warn!(target: "stoat::lsp", ?err, "did_save notification failed");
                }
            })
            .detach();
    }
}

/// Whether a server declaring `cap` wants a save notification, and whether it
/// wants the saved text with it.
///
/// `None` when it wants no notification. The `save` field is what asks for one,
/// so a server that omits it, or sets it to `false`, is left alone. The bare
/// `Kind` form predates the field and says nothing about saves, which the
/// protocol reads as a notification without the text.
fn save_notification_text(cap: &Option<TextDocumentSyncCapability>) -> Option<bool> {
    match cap.as_ref()? {
        TextDocumentSyncCapability::Kind(_) => Some(false),
        TextDocumentSyncCapability::Options(options) => match options.save.as_ref()? {
            TextDocumentSyncSaveOptions::Supported(false) => None,
            TextDocumentSyncSaveOptions::Supported(true) => Some(false),
            TextDocumentSyncSaveOptions::SaveOptions(save) => {
                Some(save.include_text.unwrap_or(false))
            },
        },
    }
}

/// What a write did.
///
/// The armed case is the ordinary one. A write streams off the run loop, so the
/// caller learns nothing here beyond that it started.
enum WriteOutcome {
    /// The bytes landed and the buffer was marked clean, which only a config
    /// save answers now.
    Wrote,
    /// The write is on its way to disk. [`pump_pending_save`] finishes it.
    Armed,
    /// The write was refused or failed, with [`Stoat::pending_message`] set.
    Failed,
}

/// Whether `path` is one of the two configs this editor re-applies on save.
fn is_config_path(path: &Path) -> bool {
    [
        crate::paths::user_config_path(),
        crate::paths::stoatty_config_path(),
    ]
    .into_iter()
    .flatten()
    .any(|config| config == path)
}

/// A buffer's bytes on their way to disk.
pub(crate) struct PendingSave {
    rx: mpsc::Receiver<std::io::Result<()>>,
    _task: stoat_scheduler::Task<()>,
    buffer_id: BufferId,
    path: PathBuf,
    /// The buffer version the written bytes came from.
    ///
    /// A buffer that has moved past it stays dirty when the write lands, since
    /// the disk then holds older text than the buffer does.
    version: u64,
}

/// Start streaming `buffer_id`'s current text to `path` off the run loop.
///
/// The rope is cloned, which is cheap, and the chunks are written as they come,
/// so neither the whole file nor its CRLF-restored copy ever exists beside the
/// buffer it came from.
fn arm_pending_save(stoat: &mut Stoat, buffer_id: BufferId, path: &Path) {
    let Some(buffer) = stoat.active_workspace().buffers.get(buffer_id) else {
        return;
    };
    let (rope, version) = {
        let guard = buffer.read().expect("buffer poisoned");
        (guard.rope().clone(), guard.version())
    };

    let ending = stoat.active_workspace().buffers.line_ending(buffer_id);
    let fs_host = stoat.fs_host.clone();
    let redraw = stoat.redraw_notify.clone();
    let target = path.to_path_buf();
    let (tx, rx) = mpsc::channel();

    let task = {
        let target = target.clone();
        stoat.executor.spawn_blocking(move || {
            // A chunk never splits a line ending, so restoring per chunk is the
            // same bytes the whole-text restore produced.
            let mut restored = rope.chunks().map(|chunk| ending.restore(chunk));
            let mut chunks = std::iter::from_fn(|| restored.next()).collect::<Vec<_>>();
            let mut bytes = chunks.iter_mut().map(|chunk| chunk.as_bytes());
            let result = fs_host.write_atomic_stream(&target, &mut bytes);
            let _ = tx.send(result);
            redraw.notify_one();
        })
    };

    stoat.pending_save = Some(PendingSave {
        rx,
        _task: task,
        buffer_id,
        path: target,
        version,
    });
}

/// Land a write that has come back from its thread.
///
/// The buffer is marked clean only when it still holds the version that was
/// written. One that moved inside the write window is genuinely dirty against
/// what reached the disk, so it keeps its flag and the next save writes again.
///
/// Returns `true` when an outcome landed this call, which is what tells the run
/// loop to redraw.
pub(crate) fn pump_pending_save(stoat: &mut Stoat) -> bool {
    let Some(pending) = stoat.pending_save.take() else {
        return false;
    };
    let result = match pending.rx.try_recv() {
        Ok(result) => result,
        Err(mpsc::TryRecvError::Empty) => {
            stoat.pending_save = Some(pending);
            return false;
        },
        Err(mpsc::TryRecvError::Disconnected) => {
            // The worker died without answering, which leaves the file in
            // whatever state it reached. Reported as a failure so the buffer
            // stays dirty rather than being marked clean over unknown bytes.
            Err(std::io::Error::other("the save worker stopped"))
        },
    };

    let wrote = match result {
        Ok(()) => {
            finish_pending_save(stoat, &pending);
            true
        },
        Err(err) => {
            tracing::warn!(target: "stoat::file", ?err, path = ?pending.path, "buffer save failed");
            stoat.set_status(format!("save failed: {err}"));
            false
        },
    };

    if std::mem::take(&mut stoat.quit_after_save) {
        stoat.quit_requested = wrote;
    }
    true
}

/// Everything a landed write does beyond the bytes themselves.
fn finish_pending_save(stoat: &mut Stoat, pending: &PendingSave) {
    let held = stoat
        .active_workspace()
        .buffers
        .get(pending.buffer_id)
        .map(|buffer| buffer.read().expect("buffer poisoned").version())
        == Some(pending.version);

    if held && let Some(buffer) = stoat.active_workspace().buffers.get(pending.buffer_id) {
        buffer.write().expect("buffer poisoned").mark_clean();
    }
    if let Some(mtime) = stoat
        .fs_host
        .metadata(&pending.path)
        .ok()
        .flatten()
        .map(|m| m.modified)
    {
        stoat
            .active_workspace_mut()
            .buffers
            .set_disk_mtime(pending.buffer_id, mtime);
    }
    // The file on disk now matches the buffer, so this is the moment the shard
    // a later open warm-loads becomes worth writing.
    if !stoat.persistence_disabled {
        let executor = stoat.executor.clone();
        let index_update_tx = stoat.index_update_tx.clone();
        let redraw_notify = stoat.redraw_notify.clone();
        stoat.active_workspace_mut().enqueue_reindex(
            &executor,
            &index_update_tx,
            &redraw_notify,
            pending.buffer_id,
            true,
        );
    }

    // A buffer that moved inside the write window has text the disk does not
    // hold, and a notification naming it describes a file that never existed.
    // The change the reader made reaches the server as an edit instead.
    if !held {
        return;
    }
    let Some(rope) = stoat
        .active_workspace()
        .buffers
        .get(pending.buffer_id)
        .map(|buffer| buffer.read().expect("buffer poisoned").rope().clone())
    else {
        return;
    };
    notify_did_save(stoat, pending.buffer_id, &pending.path, &rope);
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
        host::{FakeFsOp, FsHost},
        lsp::sync,
        test_harness::{editor, TestHarness},
        Stoat,
    };
    use lsp_types::{
        SaveOptions, ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind,
        TextDocumentSyncOptions, TextDocumentSyncSaveOptions,
    };
    use std::path::{Path, PathBuf};
    use stoat_action::{ForceSaveBuffer, OpenFile, SaveBuffer, WriteQuit};
    use stoatty_protocol::command;

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
        h.settle();

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

    /// The write takes the text it was armed with, so a buffer typed into while
    /// it flies is dirty against what reached the disk and says so.
    #[test]
    fn a_save_leaves_a_buffer_typed_into_mid_write_dirty() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/save-raced");
        let path = open_edited(&mut h, &root, "a.txt", b"original\n");

        dispatch(&mut h.stoat, &SaveBuffer);
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
            .edit(0..0, "later ");
        h.settle();

        assert_eq!(
            on_disk(&h, &path),
            b"edited original\n",
            "the disk holds the text the write was armed with",
        );
        assert!(
            editor::focused_dirty(&h.stoat),
            "and the buffer stays dirty, holding text the disk does not",
        );
    }

    /// A file long enough to span rope chunks, since the line endings are
    /// restored a chunk at a time and a boundary is where that parts from
    /// restoring the whole text at once.
    #[test]
    fn a_multi_chunk_crlf_file_saves_back_as_crlf() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/crlf-chunks");
        let seed: Vec<u8> = (0..400)
            .flat_map(|n| format!("line {n}\r\n").into_bytes())
            .collect();
        let path = open_rs(&mut h, &root, "a.rs", &seed);

        dispatch(&mut h.stoat, &ForceSaveBuffer);
        h.settle();

        assert_eq!(on_disk(&h, &path), seed, "every terminator comes back");
    }

    /// A carriage return with no newline after it is content, so it survives
    /// the open and the save writes it back where it was. Read as a terminator
    /// instead, it hands the file back with a line the user never wrote.
    #[test]
    fn a_lone_carriage_return_saves_back_unchanged() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/lone-cr-roundtrip");
        let path = open_rs(&mut h, &root, "a.rs", b"one\rtwo\n");

        dispatch(&mut h.stoat, &ForceSaveBuffer);
        h.settle();

        assert_eq!(on_disk(&h, &path), b"one\rtwo\n");
    }

    /// The terminator flattens on open and comes back on save, while the
    /// content one either side of it is left alone.
    #[test]
    fn a_crlf_file_holding_a_lone_carriage_return_round_trips() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/mixed-cr-roundtrip");
        let path = open_rs(&mut h, &root, "a.rs", b"one\rtwo\r\nthree\r\n");

        dispatch(&mut h.stoat, &ForceSaveBuffer);
        h.settle();

        assert_eq!(on_disk(&h, &path), b"one\rtwo\r\nthree\r\n");
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
        h.settle();
        assert!(!editor::focused_dirty(&h.stoat), "force save clears dirty");
        let mut written = Vec::new();
        h.fake_fs().read(&path, &mut written).expect("readable");
        assert_eq!(
            written, b"edited original\n",
            "force save overwrites the external edit",
        );
    }

    /// The write streams off the run loop, so the quit waits for it rather than
    /// exiting over a file still on its way to disk.
    #[test]
    fn write_quit_saves_and_quits_the_last_pane() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/wq-save");
        let path = open_edited(&mut h, &root, "a.txt", b"original\n");

        assert_eq!(dispatch(&mut h.stoat, &WriteQuit), UpdateEffect::Redraw);
        assert!(
            !h.stoat.quit_requested,
            "the quit waits for the write it armed"
        );

        h.settle();
        assert!(h.stoat.quit_requested, "and lands once the write does");
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
        h.settle();
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
        h.settle();
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
        h.settle();
        assert!(!editor::focused_dirty(&h.stoat));
    }

    /// Sync options declaring `save`, the field a server sets to ask for save
    /// notifications at all.
    fn save_sync(save: Option<TextDocumentSyncSaveOptions>) -> Option<TextDocumentSyncCapability> {
        Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                save,
                ..TextDocumentSyncOptions::default()
            },
        ))
    }

    /// Save a dirty buffer against a server declaring `sync`, and answer the
    /// text of every save notification it received.
    fn saves_under_sync(sync: Option<TextDocumentSyncCapability>) -> Vec<Option<String>> {
        let mut h = Stoat::test();
        h.fake_lsp().set_capabilities(ServerCapabilities {
            text_document_sync: sync,
            ..ServerCapabilities::default()
        });
        open_edited(&mut h, &PathBuf::from("/did-save-gate"), "a.txt", b"note\n");

        dispatch(&mut h.stoat, &SaveBuffer);
        h.settle();

        h.fake_lsp()
            .observed_saves()
            .into_iter()
            .map(|params| params.text)
            .collect()
    }

    /// The saved text is a whole copy of the file per notification, so it goes
    /// only to a server that set `include_text`. Every other shape of the
    /// capability asks for the notification alone.
    #[test]
    fn did_save_carries_the_text_only_where_the_server_asked_for_it() {
        let include = |flag| {
            save_sync(Some(TextDocumentSyncSaveOptions::SaveOptions(
                SaveOptions { include_text: flag },
            )))
        };
        assert_eq!(
            saves_under_sync(include(Some(true))),
            [Some("edited note\n".to_string())],
            "include_text true carries the saved text",
        );
        assert_eq!(
            saves_under_sync(include(Some(false))),
            [None],
            "include_text false carries no text",
        );
        assert_eq!(
            saves_under_sync(include(None)),
            [None],
            "an absent include_text carries no text",
        );
        assert_eq!(
            saves_under_sync(save_sync(Some(TextDocumentSyncSaveOptions::Supported(
                true
            )))),
            [None],
            "bare save support carries no text",
        );
        assert_eq!(
            saves_under_sync(Some(TextDocumentSyncCapability::Kind(
                TextDocumentSyncKind::FULL
            ))),
            [None],
            "the bare kind form predates the save field and carries no text",
        );
    }

    /// A server asks for save notifications by declaring `save`. One that
    /// declines, or never mentions saves, is left alone entirely.
    #[test]
    fn did_save_skips_a_server_that_asked_for_no_save_notification() {
        assert_eq!(
            saves_under_sync(save_sync(Some(TextDocumentSyncSaveOptions::Supported(
                false
            )))),
            [],
            "a declined save reaches the server not at all",
        );
        assert_eq!(
            saves_under_sync(save_sync(None)),
            [],
            "sync options without a save field ask for no notification",
        );
        assert_eq!(
            saves_under_sync(None),
            [],
            "a server declaring no text sync at all asks for no notification",
        );
    }

    /// A raw path holding a space parses as no URI at all. A notification built
    /// that way reaches no server, and the work a save drives never fires.
    ///
    /// The sync capability is armed because a server that declares none is sent
    /// no save notification at all, which leaves nothing here to name.
    #[test]
    fn a_save_names_the_document_by_the_uri_the_open_registered() {
        let mut h = Stoat::test();
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::FULL);
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
