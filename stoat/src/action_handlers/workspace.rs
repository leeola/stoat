use crate::{
    app::{Stoat, UpdateEffect},
    workspace::{Workspace, WorkspaceId, WorkspaceUid},
};

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
    // The copy must not inherit live Claude session identity or any in-flight
    // rebase pointer from its source; those tie to OS-level resources or
    // half-applied git state that belong to the original workspace.
    state.claude_session_id = None;
    state.rebase = None;
    state.rebase_active = None;
    state.uid = WorkspaceUid::now();

    stoat.save_workspace(stoat.active_workspace());

    let mut ws = Workspace::new(git_root, &stoat.executor);
    ws.apply_state(state, &stoat.executor);
    ws.layout(stoat.size());
    let id = stoat.workspaces.insert(ws);
    stoat.workspaces[id].id = id;
    switch_active_workspace(stoat, id);
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
        if let Ok(path) = crate::workspace::state_path_for(&ws.git_root, ws.uid) {
            if path.exists() {
                if let Err(err) = std::fs::remove_file(&path) {
                    tracing::warn!(?path, ?err, "failed to delete workspace state file");
                }
            }
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

pub(super) fn handle_dump(stoat: &Stoat, name: &str) {
    match crate::dump::save(stoat, name) {
        Ok(id) => tracing::info!(id = %id, "dump captured"),
        Err(e) => tracing::error!(error = %e, name = %name, "dump failed"),
    }
}
