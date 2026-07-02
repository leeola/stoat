use crate::{
    app::{Stoat, UpdateEffect},
    editor_state::EditorState,
    pane::{Axis, Direction, DockSide, DockVisibility, FocusTarget, PaneId, View},
};

/// Closes the focused pane and disposes its backing view state. Returns
/// `false` when the pane tree refused to close (only one split pane
/// remains); in that case no state is touched so callers can choose to
/// exit the application instead.
pub(super) fn close_focused_pane(stoat: &mut Stoat) -> bool {
    let focused = stoat.active_workspace().panes.focus();
    close_pane_by_id(stoat, focused)
}

/// Closes every split pane except the focused one. Pre-collects the
/// non-focused ids before looping so the tree's in-place mutations don't
/// invalidate the iteration; remaining ids stay keyed to their still-present
/// panes. Focus never moves because [`crate::pane::PaneTree::close`] only
/// reassigns focus when the closed pane *is* the focused one.
pub(super) fn close_other_panes(stoat: &mut Stoat) {
    let ws = stoat.active_workspace();
    let focused = ws.panes.focus();
    let others: Vec<PaneId> = ws
        .panes
        .split_pane_ids()
        .into_iter()
        .filter(|id| *id != focused)
        .collect();

    for id in others {
        close_pane_by_id(stoat, id);
    }
}

/// Closes a specific pane by id and disposes its backing view state. Returns
/// `false` when the pane tree refused to close (only one split pane
/// remains); in that case no state is touched.
pub(crate) fn close_pane_by_id(stoat: &mut Stoat, id: PaneId) -> bool {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let view = ws.panes.pane(id).view.clone();
    if !ws.panes.close(id) {
        return false;
    }
    match view {
        View::Editor(id) => {
            let buffer_id = ws.editors.get(id).map(|editor| editor.buffer_id);
            ws.editors.remove(id);
            if let Some(buffer_id) = buffer_id
                && let Some(done) = ws.editor_bridge_waiters.remove(&buffer_id)
            {
                let _ = done.send(());
            }
        },
        View::Run(id) => {
            if let Some(mut state) = ws.runs.remove(id) {
                if let Some(handle) = &mut state.shell_handle {
                    handle.kill();
                }
                state.dispose(ws);
            }
        },
        View::Agent(id) | View::Terminal(id) => {
            if let Some(term) = ws.terms.remove(id) {
                let session = term.session;
                executor
                    .spawn(async move {
                        if let Err(err) = session.kill().await {
                            tracing::warn!(target: "stoat::agent", %err, "failed to kill agent pty child");
                        }
                    })
                    .detach();
            }
        },
        View::Label(_) => {},
    }
    true
}

pub(super) fn split_pane(stoat: &mut Stoat, axis: Axis) -> UpdateEffect {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let new_pane_id = ws.panes.split(axis);
    if let View::Editor(old_editor_id) = ws.panes.pane(new_pane_id).view
        && let Some(old_editor) = ws.editors.get(old_editor_id)
    {
        let buffer_id = old_editor.buffer_id;
        if let Some(buffer) = ws.buffers.get(buffer_id) {
            let new_editor_id = ws
                .editors
                .insert(EditorState::new(buffer_id, buffer, executor));
            ws.panes.pane_mut(new_pane_id).view = View::Editor(new_editor_id);
        }
    }
    UpdateEffect::Redraw
}

pub(super) fn split_pane_new(stoat: &mut Stoat, axis: Axis) -> UpdateEffect {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let new_pane_id = ws.panes.split(axis);
    let (buffer_id, buffer) = ws.buffers.new_scratch();
    let new_editor_id = ws
        .editors
        .insert(EditorState::new(buffer_id, buffer, executor));
    ws.panes.pane_mut(new_pane_id).view = View::Editor(new_editor_id);
    UpdateEffect::Redraw
}

pub(super) fn focus_direction(stoat: &mut Stoat, direction: Direction) {
    let ws = stoat.active_workspace_mut();
    match (ws.focus, direction) {
        (FocusTarget::Dock(dock_id), Direction::Left)
            if ws
                .docks
                .get(dock_id)
                .is_some_and(|d| d.side == DockSide::Right) =>
        {
            ws.focus = FocusTarget::SplitPane(ws.panes.focus());
        },
        (FocusTarget::Dock(dock_id), Direction::Right)
            if ws
                .docks
                .get(dock_id)
                .is_some_and(|d| d.side == DockSide::Left) =>
        {
            ws.focus = FocusTarget::SplitPane(ws.panes.focus());
        },
        (FocusTarget::SplitPane(_), Direction::Right)
            if !ws.panes.focus_direction(Direction::Right) =>
        {
            if let Some((dock_id, _)) = ws.docks.iter().find(|(_, d)| {
                d.side == DockSide::Right && !matches!(d.visibility, DockVisibility::Hidden)
            }) {
                ws.focus = FocusTarget::Dock(dock_id);
            }
        },
        (FocusTarget::SplitPane(_), Direction::Left)
            if !ws.panes.focus_direction(Direction::Left) =>
        {
            if let Some((dock_id, _)) = ws.docks.iter().find(|(_, d)| {
                d.side == DockSide::Left && !matches!(d.visibility, DockVisibility::Hidden)
            }) {
                ws.focus = FocusTarget::Dock(dock_id);
            }
        },
        (FocusTarget::SplitPane(_), Direction::Up | Direction::Down) => {
            ws.panes.focus_direction(direction);
        },
        _ => {},
    }
}

pub(super) fn toggle_dock(stoat: &mut Stoat, side: DockSide) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    for (dock_id, dock) in &mut ws.docks {
        if dock.side != side {
            continue;
        }
        dock.visibility = match dock.visibility {
            DockVisibility::Open { .. } => DockVisibility::Minimized,
            DockVisibility::Minimized => DockVisibility::Hidden,
            DockVisibility::Hidden => DockVisibility::Open {
                width: dock.default_width,
            },
        };
        if matches!(dock.visibility, DockVisibility::Hidden)
            && matches!(ws.focus, FocusTarget::Dock(id) if id == dock_id)
        {
            ws.focus = FocusTarget::SplitPane(ws.panes.focus());
        }
        return UpdateEffect::Redraw;
    }
    UpdateEffect::None
}
