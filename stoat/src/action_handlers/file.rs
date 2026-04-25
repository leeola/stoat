use crate::{
    action_handlers::read_string_via_host,
    app::Stoat,
    buffer::BufferId,
    editor_state::EditorState,
    pane::{PaneId, View},
};
use std::path::Path;

pub(super) fn open_file(stoat: &mut Stoat, path: &Path) -> Option<BufferId> {
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
    let ws = stoat.active_workspace_mut();

    let (buffer_id, buffer) = ws.buffers.open(&absolute, &content);
    if let Some(lang) = lang {
        if ws.buffers.language_for(buffer_id).is_none() {
            ws.buffers.set_language(buffer_id, lang);
        }
    }

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
