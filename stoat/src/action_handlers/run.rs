use crate::{
    app::{Stoat, UpdateEffect},
    pane::View,
    run::{OutputBlock, RunState},
};

pub(super) fn open_run(stoat: &mut Stoat) -> UpdateEffect {
    let executor = stoat.executor.clone();
    let pty_tx = stoat.pty_tx.clone();
    let host = stoat.terminal_host.clone();
    let ws = stoat.active_workspace_mut();
    let cwd = ws.git_root.clone();
    let diff = ws.env.diff.clone();
    let focused = ws.panes.focus();
    let width = ws.panes.pane(focused).area.width.saturating_sub(2).max(20);

    let state = RunState::new(cwd.clone(), ws, executor.clone());
    let run_id = ws.runs.insert(state);

    if let Ok(handle) =
        crate::run::spawn_shell(&*host, &executor, &cwd, width, pty_tx, run_id, &diff)
        && let Some(run_state) = ws.runs.get_mut(run_id)
    {
        run_state.shell_handle = Some(handle);
    }

    ws.panes.pane_mut(focused).view = View::Run(run_id);
    stoat.transition_mode("insert".into());
    UpdateEffect::Redraw
}

pub(super) fn run_submit(stoat: &mut Stoat) -> UpdateEffect {
    let pty_tx = stoat.pty_tx.clone();
    let executor = stoat.executor.clone();
    let host = stoat.terminal_host.clone();
    let active_idx = stoat.active_workspace;
    let ws = &mut stoat.workspaces[active_idx];
    let diff = ws.env.diff.clone();
    let focused = ws.panes.focus();
    let View::Run(id) = ws.panes.pane(focused).view else {
        return UpdateEffect::None;
    };
    let text = {
        let Some(run_state) = ws.runs.get(id) else {
            return UpdateEffect::None;
        };
        run_state.input.text(ws)
    };
    if text.is_empty() {
        return UpdateEffect::None;
    }

    {
        let Some(run_state) = ws.runs.get_mut(id) else {
            return UpdateEffect::None;
        };
        run_state.history.push(text.clone());
        run_state.history_cursor = None;
    }

    let input_ref = {
        let Some(run_state) = ws.runs.get(id) else {
            return UpdateEffect::None;
        };
        run_state.input.clone()
    };
    input_ref.replace_text(ws, "");

    let pane_area = ws.panes.pane(focused).area;
    // Output no longer sits under a "$ " indent, so the grid fills the pane.
    let width = pane_area.width.max(20);

    let run_state = match ws.runs.get_mut(id) {
        Some(s) => s,
        None => return UpdateEffect::None,
    };
    // Submitting snaps the output back to the prompt, like a terminal.
    run_state.scroll_offset = 0;
    run_state
        .blocks
        .push(OutputBlock::new(text.clone(), run_state.cwd.clone(), width));

    if let Some(handle) = &mut run_state.shell_handle {
        handle.send_command(&text);
    } else if let Ok(handle) =
        crate::run::spawn_shell(&*host, &executor, &run_state.cwd, width, pty_tx, id, &diff)
    {
        run_state.shell_handle = Some(handle);
        if let Some(h) = &mut run_state.shell_handle {
            h.send_command(&text);
        }
    }

    UpdateEffect::Redraw
}

pub(super) fn run_interrupt(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let View::Run(id) = ws.panes.pane(focused).view else {
        return UpdateEffect::None;
    };
    let Some(run_state) = ws.runs.get_mut(id) else {
        return UpdateEffect::None;
    };
    if let Some(handle) = &mut run_state.shell_handle {
        handle.send_interrupt();
    }
    UpdateEffect::Redraw
}

/// Dismiss the finished modal run, dropping its overlay and removing the run.
///
/// Bound under `modal == run`, reachable only once the run has finished
/// because the in-flight run swallows keys before the keymap sees them.
pub(super) fn run_modal_dismiss(stoat: &mut Stoat) -> UpdateEffect {
    let Some(run_id) = stoat.modal_run.take() else {
        return UpdateEffect::None;
    };
    stoat.active_workspace_mut().runs.remove(run_id);
    UpdateEffect::Redraw
}

pub(super) fn run_history_prev(stoat: &mut Stoat) -> UpdateEffect {
    let active_idx = stoat.active_workspace;
    let ws = &mut stoat.workspaces[active_idx];
    let focused = ws.panes.focus();
    let View::Run(id) = ws.panes.pane(focused).view else {
        return UpdateEffect::None;
    };
    let input_state = {
        let Some(run_state) = ws.runs.get(id) else {
            return UpdateEffect::None;
        };
        (
            run_state.history.clone(),
            run_state.history_cursor,
            run_state.input.clone(),
        )
    };
    let (history, cursor, input) = input_state;
    if history.is_empty() {
        return UpdateEffect::None;
    }
    let next = match cursor {
        Some(i) if i > 0 => i - 1,
        Some(_) => return UpdateEffect::None,
        None => history.len() - 1,
    };
    if let Some(run_state) = ws.runs.get_mut(id) {
        run_state.history_cursor = Some(next);
    }
    input.replace_text(ws, &history[next]);
    UpdateEffect::Redraw
}

pub(super) fn run_history_next(stoat: &mut Stoat) -> UpdateEffect {
    let active_idx = stoat.active_workspace;
    let ws = &mut stoat.workspaces[active_idx];
    let focused = ws.panes.focus();
    let View::Run(id) = ws.panes.pane(focused).view else {
        return UpdateEffect::None;
    };
    let input_state = {
        let Some(run_state) = ws.runs.get(id) else {
            return UpdateEffect::None;
        };
        (
            run_state.history.clone(),
            run_state.history_cursor,
            run_state.input.clone(),
        )
    };
    let (history, cursor, input) = input_state;
    let Some(idx) = cursor else {
        return UpdateEffect::None;
    };
    if idx + 1 < history.len() {
        if let Some(run_state) = ws.runs.get_mut(id) {
            run_state.history_cursor = Some(idx + 1);
        }
        input.replace_text(ws, &history[idx + 1]);
    } else {
        if let Some(run_state) = ws.runs.get_mut(id) {
            run_state.history_cursor = None;
        }
        input.replace_text(ws, "");
    }
    UpdateEffect::Redraw
}

pub(super) fn run_command(stoat: &mut Stoat, command: &str) -> UpdateEffect {
    let pty_tx = stoat.pty_tx.clone();
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace();
    let cwd = ws.git_root.clone();
    let diff = ws.env.diff.clone();
    let focused_area = ws.panes.pane(ws.panes.focus()).area;
    let width = focused_area.width.saturating_sub(8).max(20);

    let active_idx = stoat.active_workspace;
    let ws = &mut stoat.workspaces[active_idx];
    let mut state = RunState::new(cwd.clone(), ws, executor);
    state.title = Some(command.to_owned());
    state
        .blocks
        .push(OutputBlock::new(command.to_owned(), cwd.clone(), width));
    let id = ws.runs.insert(state);

    match crate::run::spawn_oneshot(&stoat.executor, command, &cwd, width, pty_tx, id, &diff) {
        Ok(handle) => {
            let ws = stoat.active_workspace_mut();
            if let Some(run_state) = ws.runs.get_mut(id) {
                run_state.shell_handle = Some(handle);
            }
            stoat.modal_run = Some(id);
            UpdateEffect::Redraw
        },
        Err(e) => {
            tracing::warn!("failed to spawn command: {e}");
            let ws = stoat.active_workspace_mut();
            if let Some(state) = ws.runs.remove(id) {
                state.dispose(ws);
            }
            UpdateEffect::None
        },
    }
}
