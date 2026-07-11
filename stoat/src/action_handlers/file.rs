use crate::{
    action_handlers::read_string_via_host,
    app::{Stoat, UpdateEffect},
    buffer::{BufferId, SharedBuffer},
    editor_state::EditorState,
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
    task::{Context, Poll},
    time::Duration,
};
use stoat_scheduler::Executor;

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
            stoat.pending_message = Some("nothing to write; use :q to quit".to_string());
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
        stoat.pending_message = Some("file changed on disk; use :w! to overwrite".to_string());
        return SaveFlow::RefusedDiskChanged;
    }

    if format_on_save_enabled(stoat) {
        // A save already formatting drops later ones so a burst does not queue
        // duplicate writes. The in-flight one still lands the latest text.
        if stoat.pending_format_on_save.is_some() {
            return SaveFlow::AlreadyPending;
        }
        arm_format_on_save(stoat, buffer_id, path);
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

fn format_on_save_enabled(stoat: &Stoat) -> bool {
    stoat.settings.format_on_save == Some(true)
        && stoat
            .lsp_host
            .supports_feature(LanguageServerFeature::Format)
}

/// Race a `textDocument/formatting` request against [`FORMAT_ON_SAVE_BUDGET`]
/// and park the outcome in [`Stoat::pending_format_on_save`] for
/// [`pump_format_on_save`]. Writes immediately without formatting when the path
/// has no `file:` URI.
fn arm_format_on_save(stoat: &mut Stoat, buffer_id: BufferId, path: PathBuf) {
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

    let lsp = stoat.lsp_host.clone();
    let executor = stoat.executor.clone();
    let task = stoat.executor.spawn(async move {
        let format = std::pin::pin!(lsp.formatting(params));
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
        stoat.pending_message = Some(format!("save failed: {err}"));
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
    let lsp = stoat.lsp_host.clone();
    stoat
        .executor
        .spawn(async move {
            if let Err(err) = lsp.did_save(params).await {
                tracing::warn!(target: "stoat::lsp", ?err, "did_save notification failed");
            }
        })
        .detach();
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
    let editor_ids: Vec<crate::editor_state::EditorId> = stoat
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
        let lsp = stoat.lsp_host.clone();
        stoat
            .executor
            .spawn(async move {
                if let Err(err) = lsp.did_close(params).await {
                    tracing::warn!(target: "stoat::lsp", ?err, "did_close notification failed");
                }
            })
            .detach();
    }
    UpdateEffect::Redraw
}

pub(crate) fn open_file(stoat: &mut Stoat, path: &Path) -> Option<BufferId> {
    let target = stoat.active_workspace().panes.focus();
    open_file_in_pane(stoat, target, path)
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
    let content = match read_string_via_host(&*stoat.fs_host, &absolute) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            tracing::error!("failed to read {}: {}", absolute.display(), e);
            return None;
        },
    };
    let disk_mtime = stoat
        .fs_host
        .metadata(&absolute)
        .ok()
        .flatten()
        .map(|m| m.modified);

    let lang = stoat.language_registry.for_path(&absolute);
    let executor = stoat.executor.clone();

    let (buffer_id, buffer) = {
        let ws = stoat.active_workspace_mut();
        let (buffer_id, buffer) = ws.buffers.open(&absolute, &content);
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

    super::lsp::notify_buffer_opened(stoat, buffer_id, &absolute, &content);

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
    if let View::Editor(eid) = ws.panes.pane(target).view
        && ws
            .editors
            .get(eid)
            .is_some_and(|e| e.buffer_id == buffer_id)
    {
        return Some(buffer_id);
    }

    let new_editor_id = ws
        .editors
        .insert(EditorState::new(buffer_id, buffer, executor));

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
        host::{FakeFsOp, FsHost},
        test_harness::TestHarness,
        Stoat,
    };
    use std::path::{Path, PathBuf};
    use stoat_action::{CloseBuffer, ForceSaveBuffer, OpenBuffer, OpenFile, SaveBuffer, WriteQuit};

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
    fn snapshot_save_failure_shows_message_row() {
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
        h.assert_snapshot("save_failure_shows_message_row");
    }

    #[test]
    fn snapshot_clean_frame_has_no_message_row() {
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
        h.assert_snapshot("clean_frame_has_no_message_row");
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

    fn focused_buffer_id(stoat: &mut Stoat) -> crate::buffer::BufferId {
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

    fn open_path(h: &mut TestHarness, content: &[u8]) -> (PathBuf, crate::buffer::BufferId) {
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
        assert!(new_buffer.read().expect("poisoned").rope().is_empty());
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
