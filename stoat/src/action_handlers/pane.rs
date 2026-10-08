use crate::{
    app::{Stoat, UpdateEffect},
    buffer_lifecycle,
    editor_state::{EditorId, EditorState},
    pane::{Axis, Direction, DockSide, DockVisibility, FocusTarget, PaneId, Placement, View},
    workspace::Workspace,
};
use ratatui::layout::Rect;
use stoat_scheduler::Executor;

/// Closes the focused pane and disposes its backing view state. Returns
/// `false` when the pane tree refused to close (only one split pane
/// remains); in that case no state is touched so callers can choose to
/// exit the application instead.
pub(super) fn close_focused_pane(stoat: &mut Stoat) -> bool {
    let focused = stoat.active_workspace().panes.focus();
    close_pane_by_id(stoat, focused)
}

/// Detaches the focused editor pane into its own stoatty aux window.
///
/// A no-op with a status message when stoat is not running under stoatty or the
/// focused pane is not an editor, and silently when the tree refuses (the last
/// split pane). On success the pane keeps its size as window-relative
/// coordinates, focus stays on it, and the next aux-window id is consumed.
pub(super) fn detach_focused_pane(stoat: &mut Stoat) {
    if !stoat.window_ipc_connected {
        stoat.set_status("detach needs stoatty");
        return;
    }

    let focused = stoat.active_workspace().panes.focus();
    let window = stoat.next_aux_window;
    let ws = stoat.active_workspace_mut();
    let old = ws.panes.pane(focused).area;
    if !ws.panes.detach(focused, window) {
        return;
    }
    ws.panes.pane_mut(focused).area = Rect::new(0, 0, old.width, old.height);
    ws.panes.set_focus(focused);
    stoat.next_aux_window += 1;
}

/// Reattaches the focused detached pane back into the split layout.
///
/// A no-op unless the focused pane is currently detached into a window.
pub(super) fn reattach_focused_pane(stoat: &mut Stoat) {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    if !matches!(ws.panes.pane(focused).placement, Placement::Window(_)) {
        return;
    }
    ws.panes.attach(focused);
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

/// Toggles the focused pane's full-width widen, reporting the outcome in the
/// status line.
///
/// Unwidens when the focused pane is already widened. Otherwise widens it, or
/// reports that its neighbors' edges do not align to permit a clean cover.
pub(super) fn toggle_pane_widen(stoat: &mut Stoat) {
    let status = {
        let panes = &mut stoat.active_workspace_mut().panes;
        let focused = panes.focus();
        if panes.widened() == Some(focused) {
            panes.unwiden();
            "pane widen off"
        } else if panes.widen(focused) {
            "pane widened"
        } else {
            "cannot widen: pane edges don't align"
        }
    };
    stoat.set_status(status);
}

/// Closes a specific pane by id and disposes its backing view state. Returns
/// `false` when the pane tree refused to close (only one split pane
/// remains); in that case no state is touched.
///
/// A shell the pane covered ends with it, unless another view shows it. A
/// commits list the pane covered ends with it too, since no other pane holds
/// a list.
pub(crate) fn close_pane_by_id(stoat: &mut Stoat, id: PaneId) -> bool {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let (view, covered) = {
        let pane = ws.panes.pane(id);
        (pane.view.clone(), pane.prev_view.clone())
    };
    if !ws.panes.close(id) {
        return false;
    }

    dispose_view(ws, &executor, view, EditorDisposal::Remove);
    match covered {
        Some(View::Terminal(term_id)) if !ws.term_shown(term_id) => dispose_view(
            ws,
            &executor,
            View::Terminal(term_id),
            EditorDisposal::Remove,
        ),
        Some(View::Commits(list)) => {
            dispose_view(ws, &executor, View::Commits(list), EditorDisposal::Remove)
        },
        _ => {},
    }
    true
}

/// Put the shell `id` covers back on screen in place of the view in front of it.
///
/// Returns `false` and changes nothing when the pane covers no live shell. The
/// view in front is disposed as a pane close disposes it, and the pane's record
/// of the shell is cleared.
///
/// When the pane has focus, the editor goes to normal mode, the mode a shell
/// that comes back rests in.
pub(crate) fn restore_covered_terminal(stoat: &mut Stoat, id: PaneId) -> bool {
    let executor = stoat.executor.clone();
    let focused = {
        let ws = stoat.active_workspace_mut();
        let Some(term_id) = super::terminal::covered_terminal(ws, id) else {
            return false;
        };
        let pane = ws.panes.pane_mut(id);
        pane.prev_view = None;
        let front = std::mem::replace(&mut pane.view, View::Terminal(term_id));
        dispose_view(ws, &executor, front, EditorDisposal::Remove);
        matches!(ws.focus, FocusTarget::SplitPane) && ws.panes.focus() == id
    };

    if focused {
        stoat.transition_mode("normal".to_string());
    }
    true
}

/// Return `id` to the shell it covers, or close it when it covers none.
///
/// Returns `false` when it is the last pane and covers no shell, which is when
/// the caller exits. A clean bridged buffer that the quit released is closed
/// with it.
pub(super) fn quit_pane(stoat: &mut Stoat, id: PaneId) -> bool {
    let held = {
        let ws = stoat.active_workspace();
        match ws.panes.pane(id).view {
            View::Editor(editor) => ws
                .editors
                .get(editor)
                .map(|editor| editor.buffer_id)
                .filter(|buffer_id| ws.editor_bridge_waiters.contains_key(buffer_id)),
            _ => None,
        }
    };

    let quit = restore_covered_terminal(stoat, id) || close_pane_by_id(stoat, id);
    if quit && let Some(buffer_id) = held {
        buffer_lifecycle::close_released_buffer(stoat, buffer_id);
    }
    quit
}

/// What to do with the editor behind a closing [`View::Editor`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditorDisposal {
    /// Drop it outright. Correct when a pane closes, since the pane was the
    /// thing showing it.
    Remove,
    /// Drop it only when no pane in any tab still shows it. Correct when a
    /// whole tab closes, because another tab may be showing the same editor.
    GcIfUnreferenced,
}

/// Release whatever `view` owned, which is an editor, a run's shell, a
/// terminal's PTY child, or a commits list with the view it covered. A label
/// owns nothing.
///
/// Shared by pane close and tab close, which differ only in how they treat the
/// editor. Killing a PTY is spawned onto `executor` rather than awaited, so the
/// caller is not blocked on a child that ignores its signal.
///
/// An editor's bridge waiters are released when no editor still holds its
/// buffer, as abandoned when the buffer is dirty.
pub(crate) fn dispose_view(
    ws: &mut Workspace,
    executor: &Executor,
    view: View,
    editors: EditorDisposal,
) {
    match view {
        View::Editor(id) => {
            let buffer_id = ws.editors.get(id).map(|editor| editor.buffer_id);
            match editors {
                EditorDisposal::Remove => {
                    if let Some(editor) = ws.editors.remove(id) {
                        super::drop_unreferenced_scratch(ws, editor.buffer_id);
                    }
                },
                EditorDisposal::GcIfUnreferenced => super::gc_editor_if_unreferenced(ws, id),
            }
            let Some(buffer_id) = buffer_id else {
                return;
            };
            if ws
                .editors
                .values()
                .any(|editor| editor.buffer_id == buffer_id)
            {
                return;
            }

            let outcome = ws.bridge_outcome(buffer_id);
            ws.release_bridge_waiters(buffer_id, outcome);
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
        // A pending page or preview task drops with the state. The view the
        // list covered is reachable through the list alone, so it goes too.
        View::Commits(id) => match ws.commit_lists.remove(id).and_then(|list| list.covered) {
            Some(View::Agent(term_id) | View::Terminal(term_id)) if ws.term_shown(term_id) => {},
            Some(covered) => dispose_view(ws, executor, covered, EditorDisposal::GcIfUnreferenced),
            None => {},
        },
        // An image pane owns nothing to dispose. It holds a path and a size.
        View::Label(_) | View::Image { .. } => {},
    }
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
    let prev = stoat
        .active_workspace_mut()
        .panes
        .pane_mut(pane_id)
        .prev_view
        .take();
    show_view_or_scratch(stoat, pane_id, prev);
}

/// Point pane `pane_id` at `view`, or at a fresh scratch editor when `view` is
/// `None` or no longer resolves against live workspace state.
pub(crate) fn show_view_or_scratch(stoat: &mut Stoat, pane_id: PaneId, view: Option<View>) {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let view = view
        .filter(|view| view_is_live(ws, Some(view)))
        .unwrap_or_else(|| scratch_editor_view(ws, executor));
    ws.panes.pane_mut(pane_id).view = view;
}

/// Whether `view` names state the workspace still holds, so a pane that shows
/// it draws something real.
///
/// `None` and a `Label` count as dead, so a pane never strands on placeholder
/// text.
pub(crate) fn view_is_live(ws: &Workspace, view: Option<&View>) -> bool {
    match view {
        Some(View::Editor(id)) => ws.editors.contains_key(*id),
        Some(View::Run(id)) => ws.runs.contains_key(*id),
        Some(View::Agent(id) | View::Terminal(id)) => ws.terms.contains_key(*id),
        Some(View::Commits(id)) => ws.commit_lists.contains_key(*id),
        // An image pane refers to nothing that goes away.
        Some(View::Image { .. }) => true,
        Some(View::Label(_)) | None => false,
    }
}

/// A fresh scratch editor in `ws`, as the view of a pane that has nothing else
/// to show.
pub(crate) fn scratch_editor_view(ws: &mut Workspace, executor: Executor) -> View {
    let (buffer_id, buffer) = ws.buffers.new_scratch();
    View::Editor(ws.editors.insert(EditorState::new(
        buffer_id,
        buffer,
        executor,
        ws.redraw_notify.clone(),
    )))
}

pub(super) fn split_pane(stoat: &mut Stoat, axis: Axis) -> UpdateEffect {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let new_pane_id = ws.panes.split(axis);
    // A list belongs to one pane, so the split gets an editor of its own. The
    // source list leaves the leader mode the split was issued from, as a
    // source editor does below.
    if let View::Commits(source_list) = ws.panes.pane(new_pane_id).view {
        ws.panes.pane_mut(new_pane_id).view = scratch_editor_view(ws, executor);
        if let Some(list) = ws.commit_lists.get_mut(source_list) {
            list.mode = "normal".into();
        }
        return UpdateEffect::Redraw;
    }
    if let View::Editor(source_editor_id) = ws.panes.pane(new_pane_id).view {
        if let Some(buffer_id) = ws.editors.get(source_editor_id).map(|e| e.buffer_id)
            && let Some(buffer) = ws.buffers.get(buffer_id)
        {
            let mut editor = ws.seeded_editor(buffer_id, buffer, executor);
            if let Some(src) = ws.editors.get(source_editor_id) {
                editor.selections = src.selections.clone();
                editor.scroll_row = src.scroll_row;
                editor.scroll_offset = src.scroll_offset;
            }
            let new_editor_id = ws.editors.insert(editor);
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
    ws.panes.pane_mut(new_pane_id).view = scratch_editor_view(ws, executor);
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
            ws.focus = FocusTarget::SplitPane;
        },
        (FocusTarget::Dock(dock_id), Direction::Right)
            if ws
                .docks
                .get(dock_id)
                .is_some_and(|d| d.side == DockSide::Left) =>
        {
            ws.focus = FocusTarget::SplitPane;
        },
        (FocusTarget::SplitPane, Direction::Right)
            if !ws.panes.focus_direction(Direction::Right) =>
        {
            if let Some((dock_id, _)) = ws.docks.iter().find(|(_, d)| {
                d.side == DockSide::Right && !matches!(d.visibility, DockVisibility::Hidden)
            }) {
                ws.focus = FocusTarget::Dock(dock_id);
            }
        },
        (FocusTarget::SplitPane, Direction::Left) if !ws.panes.focus_direction(Direction::Left) => {
            if let Some((dock_id, _)) = ws.docks.iter().find(|(_, d)| {
                d.side == DockSide::Left && !matches!(d.visibility, DockVisibility::Hidden)
            }) {
                ws.focus = FocusTarget::Dock(dock_id);
            }
        },
        (FocusTarget::SplitPane, Direction::Up | Direction::Down) => {
            ws.panes.focus_direction(direction);
        },
        _ => {},
    }
}

/// Swap the focused pane's content with the split leaf in `direction`,
/// following it there.
///
/// Only a focused split pane can move. A dock focus reports instead. A widened
/// layout is collapsed first so the neighbour search reads the real geometry
/// rather than the collapsed neighbours a widen hides.
pub(super) fn move_pane_direction(stoat: &mut Stoat, direction: Direction) {
    if !matches!(stoat.active_workspace().focus, FocusTarget::SplitPane) {
        stoat.set_status("no pane to move");
        return;
    }

    let ws = stoat.active_workspace_mut();
    ws.panes.unwiden();
    if !ws.panes.swap_view_direction(direction) {
        stoat.set_status("no pane in that direction");
    }
}

/// Swap the focused pane's content with the next (`forward`) or previous split
/// leaf in traversal order, wrapping around and following it there. Reports
/// when the workspace holds a single pane or the focus is a dock.
pub(super) fn move_pane_rotate(stoat: &mut Stoat, forward: bool) {
    if !matches!(stoat.active_workspace().focus, FocusTarget::SplitPane) {
        stoat.set_status("no pane to move");
        return;
    }

    let ws = stoat.active_workspace_mut();
    ws.panes.unwiden();
    let moved = if forward {
        ws.panes.swap_view_next()
    } else {
        ws.panes.swap_view_prev()
    };
    if !moved {
        stoat.set_status("no other pane");
    }
}

/// Focus the pane at 1-based `index` in [`crate::pane::PaneTree::split_panes`]
/// layout order, the same order pane-ID badges number panes.
///
/// An out-of-range index leaves focus unchanged and sets a status message, so a
/// mistyped pane number is visible rather than a silent no-op.
pub(super) fn focus_pane_by_index(stoat: &mut Stoat, index: usize) {
    let ids = stoat.active_workspace().panes.selectable_pane_ids();
    let Some(id) = index.checked_sub(1).and_then(|i| ids.get(i).copied()) else {
        stoat.set_status(format!("no pane {index}"));
        return;
    };
    stoat.active_workspace_mut().panes.set_focus(id);

    // A detached pane lives in its own OS window. Keys route by the focused
    // pane, so raise the window or they land where the user cannot see them.
    if let Placement::Window(window) = &stoat.active_workspace().panes.pane(id).placement
        && let Some(tx) = &stoat.apc_tx
    {
        let _ = tx.send(stoatty_protocol::command::encode_window_focus(
            &stoatty_protocol::command::WindowFocusCommand { window: *window },
        ));
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
            ws.focus = FocusTarget::SplitPane;
        }
        return UpdateEffect::Redraw;
    }
    UpdateEffect::None
}
