use crate::{
    app::{Stoat, UpdateEffect},
    pane::View,
    run::{OutputBlock, RunState},
};

pub(super) fn open_run(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let cwd = ws.git_root.clone();
    let id = ws.runs.insert(RunState::new(cwd));
    let focused = ws.panes.focus();
    ws.panes.pane_mut(focused).view = View::Run(id);
    stoat.mode = "run".into();
    UpdateEffect::Redraw
}

pub(super) fn run_submit(stoat: &mut Stoat) -> UpdateEffect {
    let pty_tx = stoat.pty_tx.clone();
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let View::Run(id) = ws.panes.pane(focused).view else {
        return UpdateEffect::None;
    };
    let Some(run_state) = ws.runs.get_mut(id) else {
        return UpdateEffect::None;
    };
    let text = run_state.input.take();
    if text.is_empty() {
        return UpdateEffect::None;
    }

    run_state.history.push(text.clone());
    run_state.history_cursor = None;

    let pane_area = ws.panes.pane(focused).area;
    let width = pane_area.width.saturating_sub(2).max(20);
    run_state.blocks.push(OutputBlock::new(text.clone(), width));

    if let Some(handle) = &mut run_state.shell_handle {
        let sentinel = format!("__STOAT_{}__", run_state.blocks.len());
        handle.send_command(&text, &sentinel);
    } else if let Ok(handle) = crate::run::spawn_shell(&run_state.cwd, width, pty_tx, id) {
        let sentinel = format!("__STOAT_{}__", run_state.blocks.len());
        run_state.shell_handle = Some(handle);
        if let Some(h) = &mut run_state.shell_handle {
            h.send_command(&text, &sentinel);
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

pub(super) fn run_command(stoat: &mut Stoat, command: &str) -> UpdateEffect {
    let pty_tx = stoat.pty_tx.clone();
    let ws = stoat.active_workspace();
    let cwd = ws.git_root.clone();
    let focused_area = ws.panes.pane(ws.panes.focus()).area;
    let width = focused_area.width.saturating_sub(8).max(20);

    let mut state = RunState::new(cwd.clone());
    state.title = Some(command.to_owned());
    state
        .blocks
        .push(OutputBlock::new(command.to_owned(), width));

    let id = stoat.active_workspace_mut().runs.insert(state);

    match crate::run::spawn_oneshot(command, &cwd, width, pty_tx, id) {
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
            stoat.active_workspace_mut().runs.remove(id);
            UpdateEffect::None
        },
    }
}
