use crate::{
    app::{Stoat, UpdateEffect},
    editor_state::EditorState,
    pane::{Axis, Direction, DockSide, DockVisibility, FocusTarget, View},
};

/// Closes the focused pane and disposes its backing view state. Returns
/// `false` when the pane tree refused to close (only one split pane
/// remains); in that case no state is touched so callers can choose to
/// exit the application instead.
pub(super) fn close_focused_pane(stoat: &mut Stoat) -> bool {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let view = ws.panes.pane(focused).view.clone();
    if !ws.panes.close(focused) {
        return false;
    }
    match view {
        View::Editor(id) => {
            ws.editors.remove(id);
        },
        View::Run(id) => {
            if let Some(mut state) = ws.runs.remove(id) {
                if let Some(handle) = &mut state.shell_handle {
                    handle.kill();
                }
                state.dispose(ws);
            }
        },
        View::Label(_) | View::Claude(_) => {},
    }
    true
}

pub(super) fn split_pane(stoat: &mut Stoat, axis: Axis) -> UpdateEffect {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let new_pane_id = ws.panes.split(axis);
    if let View::Editor(old_editor_id) = ws.panes.pane(new_pane_id).view {
        if let Some(old_editor) = ws.editors.get(old_editor_id) {
            let buffer_id = old_editor.buffer_id;
            if let Some(buffer) = ws.buffers.get(buffer_id) {
                let new_editor_id = ws
                    .editors
                    .insert(EditorState::new(buffer_id, buffer, executor));
                ws.panes.pane_mut(new_pane_id).view = View::Editor(new_editor_id);
            }
        }
    }
    UpdateEffect::Redraw
}

pub(super) fn focus_direction(stoat: &mut Stoat, direction: Direction) {
    let ws = stoat.active_workspace_mut();
    match (ws.focus, direction) {
        (FocusTarget::Dock(dock_id), Direction::Left) => {
            if ws
                .docks
                .get(dock_id)
                .is_some_and(|d| d.side == DockSide::Right)
            {
                ws.focus = FocusTarget::SplitPane(ws.panes.focus());
            }
        },
        (FocusTarget::Dock(dock_id), Direction::Right) => {
            if ws
                .docks
                .get(dock_id)
                .is_some_and(|d| d.side == DockSide::Left)
            {
                ws.focus = FocusTarget::SplitPane(ws.panes.focus());
            }
        },
        (FocusTarget::SplitPane(_), Direction::Right) => {
            if !ws.panes.focus_direction(Direction::Right) {
                if let Some((dock_id, _)) = ws.docks.iter().find(|(_, d)| {
                    d.side == DockSide::Right && !matches!(d.visibility, DockVisibility::Hidden)
                }) {
                    ws.focus = FocusTarget::Dock(dock_id);
                }
            }
        },
        (FocusTarget::SplitPane(_), Direction::Left) => {
            if !ws.panes.focus_direction(Direction::Left) {
                if let Some((dock_id, _)) = ws.docks.iter().find(|(_, d)| {
                    d.side == DockSide::Left && !matches!(d.visibility, DockVisibility::Hidden)
                }) {
                    ws.focus = FocusTarget::Dock(dock_id);
                }
            }
        },
        (FocusTarget::SplitPane(_), _) => {
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
