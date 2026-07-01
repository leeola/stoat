use crate::{
    action_handlers::read_string_via_host,
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    editor_state::EditorState,
    pane::{PaneId, View},
};
use lsp_types::{DidCloseTextDocumentParams, DidSaveTextDocumentParams, TextDocumentIdentifier};
use std::{path::Path, str::FromStr};

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
    save_buffer_inner(stoat, false)
}

/// Save the focused buffer even when it changed on disk since it was opened,
/// overwriting the external edit. Backs the `:w!` command. See [`save_buffer`]
/// for the guarded variant.
pub(super) fn force_save_buffer(stoat: &mut Stoat) -> UpdateEffect {
    save_buffer_inner(stoat, true)
}

fn save_buffer_inner(stoat: &mut Stoat, force: bool) -> UpdateEffect {
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let buffer_id = editor.buffer_id;
    let path = match stoat.active_workspace().buffers.path_for(buffer_id) {
        Some(p) => p.to_path_buf(),
        None => return UpdateEffect::None,
    };
    let buffer = match stoat.active_workspace().buffers.get(buffer_id) {
        Some(b) => b,
        None => return UpdateEffect::None,
    };
    let text = {
        let guard = buffer.read().expect("buffer poisoned");
        guard.rope().to_string()
    };

    if !force && disk_changed_since_open(stoat, buffer_id, &path) {
        stoat.pending_message = Some("file changed on disk; use :w! to overwrite".to_string());
        return UpdateEffect::Redraw;
    }

    if let Err(err) = stoat.fs_host.write_atomic(&path, text.as_bytes()) {
        tracing::warn!(target: "stoat::file", ?err, ?path, "buffer save failed");
        stoat.pending_message = Some(format!("save failed: {err}"));
        return UpdateEffect::Redraw;
    }
    {
        let mut guard = buffer.write().expect("buffer poisoned");
        guard.mark_clean();
    }
    if let Some(mtime) = stoat
        .fs_host
        .metadata(&path)
        .ok()
        .flatten()
        .map(|m| m.modified)
    {
        stoat
            .active_workspace_mut()
            .buffers
            .set_disk_mtime(buffer_id, mtime);
    }
    stoat.persist_saved_shard(buffer_id, &path, &text);
    let path_str = match path.to_str() {
        Some(s) => s,
        None => return UpdateEffect::Redraw,
    };
    let Ok(uri) = lsp_types::Uri::from_str(&format!("file://{path_str}")) else {
        return UpdateEffect::Redraw;
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
    UpdateEffect::Redraw
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
        && let Ok(uri) = lsp_types::Uri::from_str(&format!("file://{path_str}"))
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
        let still_referenced = ws
            .panes
            .split_panes()
            .any(|(_, p)| matches!(p.view, View::Editor(eid) if eid == old_id));
        if !still_referenced {
            ws.editors.remove(old_id);
        }
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
    use stoat_action::{CloseBuffer, ForceSaveBuffer, OpenBuffer, OpenFile, SaveBuffer};

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
