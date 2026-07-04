use crate::{
    app::{Stoat, UpdateEffect},
    editor_state::{EditorId, EditorState},
    pane::{Axis, Direction, DockSide, DockVisibility, FocusTarget, PaneId, View},
    workspace::Workspace,
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

/// Point a pane at its pre-terminal view, or a fresh scratch editor, after its
/// terminal exits.
///
/// The pane tree refuses to close the final split pane, so a terminal that
/// exits there is repointed rather than removed. The view captured in
/// [`crate::pane::Pane::prev_view`] when the terminal opened is restored when
/// it still resolves against live workspace state. A missing, `Label`, or
/// dangling capture falls back to a fresh scratch buffer so the pane never
/// strands on a dead view.
pub(crate) fn restore_pane_after_term_exit(stoat: &mut Stoat, pane_id: PaneId) {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();

    let prev = ws.panes.pane_mut(pane_id).prev_view.take();
    let restored = prev.filter(|view| match view {
        View::Editor(id) => ws.editors.contains_key(*id),
        View::Run(id) => ws.runs.contains_key(*id),
        View::Agent(id) | View::Terminal(id) => ws.terms.contains_key(*id),
        View::Label(_) => false,
    });

    let view = restored.unwrap_or_else(|| {
        let (buffer_id, buffer) = ws.buffers.new_scratch();
        let editor_id = ws
            .editors
            .insert(EditorState::new(buffer_id, buffer, executor));
        View::Editor(editor_id)
    });
    ws.panes.pane_mut(pane_id).view = view;
}

pub(super) fn split_pane(stoat: &mut Stoat, axis: Axis) -> UpdateEffect {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let new_pane_id = ws.panes.split(axis);
    if let View::Editor(source_editor_id) = ws.panes.pane(new_pane_id).view {
        if let Some(buffer_id) = ws.editors.get(source_editor_id).map(|e| e.buffer_id)
            && let Some(buffer) = ws.buffers.get(buffer_id)
        {
            let new_editor_id = ws
                .editors
                .insert(EditorState::new(buffer_id, buffer, executor));
            ws.panes.pane_mut(new_pane_id).view = View::Editor(new_editor_id);
        }
        clear_split_source_mode(ws, source_editor_id);
    }
    UpdateEffect::Redraw
}

pub(super) fn split_pane_new(stoat: &mut Stoat, axis: Axis) -> UpdateEffect {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let new_pane_id = ws.panes.split(axis);
    let source_editor_id = match ws.panes.pane(new_pane_id).view {
        View::Editor(id) => Some(id),
        _ => None,
    };
    let (buffer_id, buffer) = ws.buffers.new_scratch();
    let new_editor_id = ws
        .editors
        .insert(EditorState::new(buffer_id, buffer, executor));
    ws.panes.pane_mut(new_pane_id).view = View::Editor(new_editor_id);
    if let Some(source_editor_id) = source_editor_id {
        clear_split_source_mode(ws, source_editor_id);
    }
    UpdateEffect::Redraw
}

/// Reset `editor_id` to normal mode after a split moved focus off it.
///
/// A split is issued from a leader sequence whose trailing `SetMode` lands on
/// the newly focused pane, so the source pane never leaves the transient leader
/// mode on its own. Left as-is it would show the leader overlay the next time
/// focus returns to it.
fn clear_split_source_mode(ws: &mut Workspace, editor_id: EditorId) {
    if let Some(editor) = ws.editors.get_mut(editor_id) {
        editor.mode = "normal".into();
    }
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
