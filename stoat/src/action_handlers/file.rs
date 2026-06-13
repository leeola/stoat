use crate::{
    action_handlers::read_string_via_host,
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    editor_state::EditorState,
    pane::{PaneId, View},
};
use lsp_types::{DidCloseTextDocumentParams, DidSaveTextDocumentParams, TextDocumentIdentifier};
use std::{path::Path, str::FromStr};

/// Write the focused buffer's rope text to its backing file via
/// [`crate::host::FsHost::write`], clear the buffer's dirty flag,
/// and notify the LSP server via [`crate::host::LspServer::did_save`].
/// No-op for scratch buffers (no path) or when no editor is
/// focused. Write errors leave the dirty flag set so the user can
/// retry.
pub(super) fn save_buffer(stoat: &mut Stoat) -> UpdateEffect {
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
    if let Err(err) = stoat.fs_host.write(&path, text.as_bytes()) {
        tracing::warn!(target: "stoat::file", ?err, ?path, "buffer save failed");
        return UpdateEffect::None;
    }
    {
        let mut guard = buffer.write().expect("buffer poisoned");
        guard.dirty = false;
    }
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
    let lsp = stoat.lsp_server.clone();
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

/// Drop the focused buffer from the workspace's
/// [`crate::buffer_registry::BufferRegistry`] and notify the LSP
/// server via [`crate::host::LspServer::did_close`]. Editor states
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

    if let Some(path) = path {
        if let Some(path_str) = path.to_str() {
            if let Ok(uri) = lsp_types::Uri::from_str(&format!("file://{path_str}")) {
                let params = DidCloseTextDocumentParams {
                    text_document: TextDocumentIdentifier { uri },
                };
                let lsp = stoat.lsp_server.clone();
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

    let lang = stoat.language_registry.for_path(&absolute);
    let executor = stoat.executor.clone();

    let (buffer_id, buffer) = {
        let ws = stoat.active_workspace_mut();
        let (buffer_id, buffer) = ws.buffers.open(&absolute, &content);
        if let Some(lang) = lang {
            if ws.buffers.language_for(buffer_id).is_none() {
                ws.buffers.set_language(buffer_id, lang);
            }
        }
        (buffer_id, buffer)
    };

    super::lsp::notify_buffer_opened(stoat, buffer_id, &absolute, &content);

    let ws = stoat.active_workspace_mut();
    if let View::Editor(eid) = ws.panes.pane(target).view {
        if ws
            .editors
            .get(eid)
            .is_some_and(|e| e.buffer_id == buffer_id)
        {
            return Some(buffer_id);
        }
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
