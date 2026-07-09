use crate::{
    app::{Stoat, UpdateEffect},
    workspace::{Workspace, WorkspaceId, WorkspaceUid},
};
use std::path::Path;

pub(super) fn new_workspace(stoat: &mut Stoat) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();
    stoat.save_workspace(stoat.active_workspace());

    let mut ws = Workspace::new(git_root, &stoat.executor);
    ws.layout(stoat.size());
    let id = stoat.workspaces.insert(ws);
    stoat.workspaces[id].id = id;
    switch_active_workspace(stoat, id);
    UpdateEffect::Redraw
}

pub(super) fn copy_workspace(stoat: &mut Stoat) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();
    let mut state = stoat.active_workspace().to_state();
    // The copy must not inherit any in-flight rebase pointer from its
    // source; it ties to half-applied git state that belongs to the
    // original workspace.
    state.rebase = None;
    state.rebase_active = None;
    state.uid = WorkspaceUid::now(&stoat.executor);

    stoat.save_workspace(stoat.active_workspace());

    let mut ws = Workspace::new(git_root, &stoat.executor);
    ws.apply_state(state, &stoat.executor);
    ws.layout(stoat.size());
    let id = stoat.workspaces.insert(ws);
    stoat.workspaces[id].id = id;
    switch_active_workspace(stoat, id);

    // The copy round-trips through to_state/apply_state, so any terminal pane
    // arrives with a dead session id. Respawn gives the copy its own shells
    // rather than dangling references into the source workspace.
    super::respawn_terminal_panes(stoat);
    UpdateEffect::Redraw
}

pub(super) fn close_workspace(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.workspaces.len() <= 1 {
        // FIXME: surface a user-visible error once we have a status surface
        // for non-badge errors; tracing-only feedback is invisible in the TUI.
        tracing::warn!("refusing to close last workspace");
        return UpdateEffect::None;
    }

    let active_id = stoat.active_workspace;
    if !stoat.persistence_disabled {
        let ws = &stoat.workspaces[active_id];
        stoat.save_workspace(ws);
        if let Ok(path) = crate::workspace::state_path_for(&ws.git_root, ws.uid, &*stoat.fs_host)
            && stoat.fs_host.exists(&path)
            && let Err(err) = stoat.fs_host.remove_file(&path)
        {
            tracing::warn!(?path, ?err, "failed to delete workspace state file");
        }
    }

    let replacement: WorkspaceId = stoat
        .workspaces
        .keys()
        .find(|k| *k != active_id)
        .expect("non-last workspace has at least one sibling");

    stoat.workspaces.remove(active_id);
    switch_active_workspace(stoat, replacement);
    UpdateEffect::Redraw
}

/// Shared tail of every workspace-switch action. Points [`Stoat::active_workspace`]
/// at `next` and re-layouts the new active workspace to the current terminal size
/// so the first render after the switch shows correctly-sized panes.
fn switch_active_workspace(stoat: &mut Stoat, next: WorkspaceId) {
    stoat.active_workspace = next;
    let size = stoat.size();
    stoat.active_workspace_mut().layout(size);
}

pub(super) fn workspace_picker_next(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(picker) = stoat.workspace_picker.as_mut() {
        picker.select_next();
    }
    UpdateEffect::Redraw
}

pub(super) fn workspace_picker_prev(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(picker) = stoat.workspace_picker.as_mut() {
        picker.select_prev();
    }
    UpdateEffect::Redraw
}

pub(super) fn workspace_picker_close(stoat: &mut Stoat) -> UpdateEffect {
    stoat.workspace_picker = None;
    UpdateEffect::Redraw
}

/// Switch to the workspace under the picker's selection, saving the current
/// one first. A selection on the already-active workspace or an empty picker
/// just closes the picker.
pub(super) fn workspace_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.workspace_picker.take() else {
        return UpdateEffect::None;
    };
    let Some(id) = picker.selected_id() else {
        return UpdateEffect::Redraw;
    };
    if id == stoat.active_workspace {
        return UpdateEffect::Redraw;
    }
    stoat.save_workspace(stoat.active_workspace());
    switch_active_workspace(stoat, id);
    UpdateEffect::Redraw
}

pub(super) fn handle_dump(stoat: &Stoat, name: &str) {
    match crate::dump::save(stoat, name, &*stoat.fs_host) {
        Ok(id) => tracing::info!(id = %id, "dump captured"),
        Err(e) => tracing::error!(error = %e, name = %name, "dump failed"),
    }
}

pub(super) fn rename_workspace(stoat: &mut Stoat, name: &str) {
    stoat.active_workspace_mut().name = name.to_string();
}

/// Set the active workspace's `git_root` to `path`, the root the file finder,
/// diff, and review resolve against.
///
/// A relative path resolves against the current root. An empty path, an
/// unresolvable path, or a non-directory leaves the root untouched.
pub(super) fn set_cwd(stoat: &mut Stoat, path: &str) {
    let path = path.trim();
    if path.is_empty() {
        tracing::warn!("cd: empty path");
        return;
    }

    let raw = Path::new(path);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        stoat.active_workspace().git_root.join(raw)
    };

    match stoat.fs_host.canonicalize(&candidate) {
        Ok(abs)
            if stoat
                .fs_host
                .metadata(&abs)
                .ok()
                .flatten()
                .is_some_and(|m| m.is_dir) =>
        {
            let ws = stoat.active_workspace_mut();
            ws.git_root = abs;
            // A new root has its own diff to warm, so re-arm the warm pass.
            ws.diff_warmed = false;
        },
        Ok(_) => tracing::warn!("cd: not a directory: {}", candidate.display()),
        Err(e) => tracing::warn!("cd: cannot resolve {}: {}", candidate.display(), e),
    }
}
