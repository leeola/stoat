use crate::{
    app::{Stoat, UpdateEffect},
    buffer_lifecycle,
    pane::{FocusTarget, View},
    run::{
        pty::{self, CmdSink},
        Cmd, CmdId, RunState,
    },
    workspace::Workspace,
};

pub(super) fn open_run(stoat: &mut Stoat) -> UpdateEffect {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();

    let state = RunState::new(ws, executor.clone());
    let run_id = ws.runs.insert(state);

    buffer_lifecycle::replace_pane_view(ws, &executor, focused, View::Run(run_id));
    stoat.transition_mode("insert".into());
    UpdateEffect::Redraw
}

/// Run `line` in a Run pane, as a submit of the pane's input runs a typed
/// line.
///
/// The first split pane that shows a Run pane takes the line and the focus.
/// With no Run pane on show, a Run pane opens in the focused pane, as
/// `OpenRun` opens one.
pub(crate) fn run_line_in_pane(stoat: &mut Stoat, line: &str) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    ws.focus = FocusTarget::SplitPane;
    let shown = ws
        .panes
        .split_panes()
        .find_map(|(id, pane)| matches!(pane.view, View::Run(_)).then_some(id));
    match shown {
        Some(pane) => ws.panes.set_focus(pane),
        None => {
            open_run(stoat);
        },
    }

    let ws = stoat.active_workspace_mut();
    let View::Run(id) = ws.panes.pane(ws.panes.focus()).view else {
        return UpdateEffect::None;
    };
    let Some(input) = ws.runs.get(id).map(|run| run.input.clone()) else {
        return UpdateEffect::None;
    };
    input.replace_text(ws, line);
    run_submit(stoat)
}

pub(super) fn run_submit(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
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
        run_state.history.reset();
    }

    let input_ref = {
        let Some(run_state) = ws.runs.get(id) else {
            return UpdateEffect::None;
        };
        run_state.input.clone()
    };
    input_ref.replace_text(ws, "");

    // Shell state does not carry from one Cmd's process to the next, so a
    // `cd` moves the workspace root that later Cmds start in.
    if let Some(path) = cd_path(&text) {
        super::workspace::set_cwd(stoat, path);
        return UpdateEffect::Redraw;
    }

    let width = ws.panes.pane(focused).area.width.max(20);
    let cmd = start_cmd(stoat, &text, width);
    let Workspace { runs, cmds, .. } = stoat.active_workspace_mut();
    if let Some(run_state) = runs.get_mut(id) {
        run_state.blocks.push(cmd);
        // Submitting snaps the output back to the prompt, like a terminal.
        run_state.scroll_offset = 0;
        run_state.trim_blocks(cmds);
    }
    tracing::info!(target: "stoat::run", command = %text, "run pane command started");
    UpdateEffect::Redraw
}

pub(super) fn run_interrupt(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let View::Run(id) = ws.panes.pane(focused).view else {
        return UpdateEffect::None;
    };
    let Some(run_state) = ws.runs.get(id) else {
        return UpdateEffect::None;
    };
    let newest_running = run_state
        .blocks
        .iter()
        .rev()
        .filter_map(|id| ws.cmds.get(*id))
        .find(|cmd| !cmd.finished);
    if let Some(process) = newest_running.and_then(|cmd| cmd.process.as_ref()) {
        process.send_interrupt();
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
    stoat.active_workspace_mut().remove_run(run_id);
    UpdateEffect::Redraw
}

/// Recall the previous command in the focused run pane's prompt.
pub(super) fn run_history_prev(stoat: &mut Stoat) -> UpdateEffect {
    walk_focused_run(stoat, RunState::history_prev)
}

/// Recall the next command toward the newest in the focused run pane's prompt.
pub(super) fn run_history_next(stoat: &mut Stoat) -> UpdateEffect {
    walk_focused_run(stoat, RunState::history_next)
}

/// Walk the focused run pane's history with `step` and write the result into
/// its prompt. No-op when focus is not on a run pane.
///
/// The prompt's [`crate::input_view::InputView`] is a pair of ids, so it comes
/// out by clone. That ends the borrow of the run state before the write, which
/// needs the workspace the state lives in.
fn walk_focused_run(
    stoat: &mut Stoat,
    step: impl FnOnce(&mut RunState, &str) -> Option<String>,
) -> UpdateEffect {
    let active_idx = stoat.active_workspace;
    let ws = &mut stoat.workspaces[active_idx];
    let focused = ws.panes.focus();
    let View::Run(id) = ws.panes.pane(focused).view else {
        return UpdateEffect::None;
    };
    let Some(input) = ws.runs.get(id).map(|run_state| run_state.input.clone()) else {
        return UpdateEffect::None;
    };

    let current = input.text(ws);
    let Some(run_state) = ws.runs.get_mut(id) else {
        return UpdateEffect::None;
    };
    if let Some(recalled) = step(run_state, &current) {
        input.replace_text(ws, &recalled);
    }
    UpdateEffect::Redraw
}

pub(crate) fn run_command(stoat: &mut Stoat, command: &str) -> UpdateEffect {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let focused_area = ws.panes.pane(ws.panes.focus()).area;
    let width = focused_area.width.saturating_sub(8).max(20);

    let mut state = RunState::new(ws, executor);
    state.title = Some(command.to_owned());
    let id = ws.runs.insert(state);
    let cmd = start_cmd(stoat, command, width);
    if let Some(run_state) = stoat.active_workspace_mut().runs.get_mut(id) {
        run_state.blocks.push(cmd);
    }
    stoat.modal_run = Some(id);
    tracing::info!(target: "stoat::run", command = %command, ?id, "run started");
    UpdateEffect::Redraw
}

/// Insert a Cmd of `line` in the active workspace's store, with the workspace
/// root as its cwd, and start its process.
fn start_cmd(stoat: &mut Stoat, line: &str, width: u16) -> CmdId {
    let ws = stoat.active_workspace_mut();
    let cmd = Cmd::new(line.to_owned(), ws.git_root.clone(), width);
    let id = ws.cmds.insert(cmd);
    start_process(stoat, id);
    id
}

/// Start the next run of the Cmd `id` of the active workspace, a `bash -c` of
/// its line in its cwd.
///
/// The run counts one more, starts now, and clears the end of the run before
/// it. A spawn that fails ends the run at once with the error line
/// `spawn failed: <error>`.
fn start_process(stoat: &mut Stoat, id: CmdId) {
    let now = stoat.executor.now();
    let executor = stoat.executor.clone();
    let host = stoat.terminal_host.clone();
    let tx = stoat.pty_tx.clone();
    let workspace = stoat.active_workspace;

    let ws = stoat.active_workspace_mut();
    let diff = ws.env.diff.clone();
    let Some(cmd) = ws.cmds.get_mut(id) else {
        return;
    };

    cmd.run_count += 1;
    cmd.started = Some(now);
    cmd.ended = None;
    cmd.finished = false;
    cmd.exit_status = None;
    cmd.error = None;
    let sink = CmdSink {
        tx,
        workspace,
        cmd: id,
        run: cmd.run_count,
    };
    let width = cmd.grid.width();
    match pty::spawn_cmd(
        &*host,
        &executor,
        &cmd.command,
        &cmd.cwd,
        width,
        sink,
        &diff,
    ) {
        Ok(process) => cmd.process = Some(process),
        Err(error) => {
            cmd.error = Some(format!("spawn failed: {error}"));
            cmd.finished = true;
            cmd.ended = Some(now);
        },
    }
}

/// The path of a line whose first word is `cd`, `~` for a bare `cd`, or
/// `None` for any other line.
fn cd_path(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("cd")?;
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    match rest.trim() {
        "" => Some("~"),
        path => Some(path),
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        app::Stoat,
        pane::View,
        run::{CmdId, PtyNotification},
        test_harness::{CmdHandle, TestHarness},
    };
    use std::{path::PathBuf, time::Duration};

    /// The program, the arguments, the cwd, and the last variable of a spawn of
    /// the fake terminal host.
    type Spawn = (String, Vec<String>, PathBuf, Option<(String, String)>);

    #[test]
    fn a_run_drops_the_editor_it_replaces() {
        let mut h = Stoat::test();
        let editor = {
            let ws = h.stoat.active_workspace();
            let View::Editor(editor) = ws.panes.pane(ws.panes.focus()).view else {
                panic!("the focused pane shows an editor");
            };
            editor
        };

        super::super::dispatch(&mut h.stoat, &stoat_action::OpenRun);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                matches!(ws.panes.pane(ws.panes.focus()).view, View::Run(_)),
                ws.editors.contains_key(editor)
            ),
            (true, false),
            "the run takes the pane, and the editor it replaced goes"
        );
    }

    /// Closing a Run pane drops its run and every Cmd of its stack.
    #[test]
    fn closing_a_run_pane_empties_the_cmd_store() {
        let mut h = Stoat::test();
        let run_id = h.open_run();
        for line in ["ls", "pwd"] {
            let input = h.stoat.active_workspace().runs[run_id].input.clone();
            input.replace_text(h.stoat.active_workspace_mut(), line);
            super::super::dispatch(&mut h.stoat, &stoat_action::RunSubmit);
        }
        let submitted = h.stoat.active_workspace().cmds.len();

        let executor = h.stoat.executor.clone();
        super::super::pane::dispose_view(
            h.stoat.active_workspace_mut(),
            &executor,
            View::Run(run_id),
            super::super::pane::EditorDisposal::Remove,
        );

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (submitted, ws.cmds.len(), ws.runs.len()),
            (2, 0, 0),
            "the two submitted Cmds leave the store with their run"
        );
    }

    /// Each spawn of the fake terminal host of `h`, in order.
    fn spawns(h: &TestHarness) -> Vec<Spawn> {
        h.fake_terminal_host()
            .spawns()
            .into_iter()
            .map(|spawn| {
                (
                    spawn.program,
                    spawn.args,
                    spawn.cwd,
                    spawn.env.last().cloned(),
                )
            })
            .collect()
    }

    /// A spawn of `bash -c line` in `cwd` with `TERM=dumb` last.
    fn bash_in(line: &str, cwd: &str) -> Spawn {
        (
            "bash".to_owned(),
            vec!["-c".to_owned(), line.to_owned()],
            PathBuf::from(cwd),
            Some(("TERM".to_owned(), "dumb".to_owned())),
        )
    }

    /// A new Run pane starts no process, and each submitted line starts a
    /// `bash -c` of its own in the workspace root and writes into no session.
    #[test]
    fn each_submitted_line_starts_its_own_bash() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.open_run();
        let opened = h.fake_terminal_host().spawns().len();
        let handles = ["ls", "pwd"].map(|line| h.submit_run(line));

        let writes = [&handles[0].session, &handles[1].session, h.fake_terminal()]
            .map(|session| session.sent_bytes().len());
        assert_eq!(
            (opened, spawns(&h), writes),
            (
                0,
                vec![bash_in("ls", "/work"), bash_in("pwd", "/work")],
                [0, 0, 0]
            ),
        );
    }

    /// Output and the end of each process land on the Cmd that owns it alone.
    #[test]
    fn output_reaches_the_cmd_that_owns_the_process() {
        let mut h = Stoat::test();
        h.open_run();
        let [first, second] = ["one", "two"].map(|line| h.submit_run(line));
        first.session.push_output(b"hello");
        h.settle();
        let text = |h: &TestHarness, handle: &CmdHandle| {
            h.stoat.active_workspace().cmds[handle.cmd]
                .grid
                .text_in(0..5, 0..1)
        };
        let read = [text(&h, &first), text(&h, &second)];

        first.session.finish(3);
        second.session.finish(0);
        h.settle();
        let ws = h.stoat.active_workspace();
        let ends = [&first, &second].map(|handle| {
            let cmd = &ws.cmds[handle.cmd];
            (cmd.exit_status, cmd.ended.is_some(), cmd.process.is_none())
        });
        assert_eq!(
            (read, ends),
            (
                ["hello".to_owned(), String::new()],
                [(Some(3), true, true), (Some(0), true, true)]
            ),
        );
    }

    #[test]
    fn a_cmd_records_when_it_ran() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("sleep 1");
        h.advance_clock(Duration::from_millis(1500));
        handle.session.finish(0);
        h.settle();

        let cmd = &h.stoat.active_workspace().cmds[handle.cmd];
        let ran = cmd
            .ended
            .zip(cmd.started)
            .map(|(ended, started)| ended - started);
        assert_eq!((ran, cmd.run_count), (Some(Duration::from_millis(1500)), 1));
    }

    #[test]
    fn output_of_an_earlier_run_is_dropped() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("ls");
        h.stoat.handle_pty_notification(PtyNotification::CmdOutput {
            workspace: h.stoat.active_workspace,
            cmd: handle.cmd,
            run: 0,
            data: b"stale".to_vec(),
        });

        let cmd = &h.stoat.active_workspace().cmds[handle.cmd];
        assert_eq!(
            (cmd.run_count, cmd.grid.text_in(0..5, 0..1)),
            (1, String::new())
        );
    }

    /// A `cd` line starts no process, since shell state does not carry from
    /// one Cmd to the next. It moves the workspace root that the next line
    /// starts in.
    #[test]
    fn a_cd_line_moves_the_workspace_root_and_starts_no_process() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.fake_fs().insert_file("/work/sub/a.txt", b"");
        let run_id = h.open_run();
        let cd = h.submit_run("cd sub");
        let ws = h.stoat.active_workspace();
        let after_cd = (
            ws.git_root.clone(),
            h.fake_terminal_host().spawns().len(),
            ws.runs[run_id].blocks.len(),
            cd.cmd == CmdId::default(),
        );

        h.submit_run("ls");
        assert_eq!(
            (after_cd, spawns(&h)),
            (
                (PathBuf::from("/work/sub"), 0, 0, true),
                vec![bash_in("ls", "/work/sub")]
            ),
        );
    }

    /// Ctrl-C goes to the newest Cmd that still runs.
    #[test]
    fn ctrl_c_reaches_the_newest_running_cmd() {
        let mut h = Stoat::test();
        h.open_run();
        let [first, second] = ["sleep 9", "sleep 8"].map(|line| h.submit_run(line));
        super::super::dispatch(&mut h.stoat, &stoat_action::RunInterrupt);
        let after_one = [first.session.sent_bytes(), second.session.sent_bytes()];

        second.session.finish(0);
        h.settle();
        super::super::dispatch(&mut h.stoat, &stoat_action::RunInterrupt);
        let after_two = [first.session.sent_bytes(), second.session.sent_bytes()];
        assert_eq!(
            (after_one, after_two),
            (
                [Vec::new(), vec![vec![0x03]]],
                [vec![vec![0x03]], vec![vec![0x03]]]
            ),
        );
    }

    #[test]
    fn closing_a_run_pane_kills_its_running_cmds() {
        let mut h = Stoat::test();
        let run_id = h.open_run();
        let handles = ["sleep 9", "sleep 8"].map(|line| h.submit_run(line));

        let executor = h.stoat.executor.clone();
        super::super::pane::dispose_view(
            h.stoat.active_workspace_mut(),
            &executor,
            View::Run(run_id),
            super::super::pane::EditorDisposal::Remove,
        );
        assert_eq!(
            handles.map(|handle| handle.session.was_killed()),
            [true, true]
        );
    }

    /// A full-screen request ends its own Cmd with the error line and kills
    /// its process, and the other Cmd runs on.
    #[test]
    fn a_full_screen_request_ends_its_own_cmd_alone() {
        let mut h = Stoat::test();
        h.open_run();
        let [first, second] = ["vim", "sleep 9"].map(|line| h.submit_run(line));
        h.inject_cmd_output(first.cmd, b"\x1b[?1049h");

        let ws = h.stoat.active_workspace();
        let state = |handle: &CmdHandle| {
            let cmd = &ws.cmds[handle.cmd];
            (cmd.finished, cmd.error.clone(), handle.session.was_killed())
        };
        assert_eq!(
            [state(&first), state(&second)],
            [
                (
                    true,
                    Some("this command requires a full terminal".to_owned()),
                    true
                ),
                (false, None, false),
            ],
        );
    }

    #[test]
    fn the_run_modal_spawns_through_the_terminal_host() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        super::super::dispatch(
            &mut h.stoat,
            &stoat_action::Run {
                command: "ls".to_owned(),
            },
        );
        let run_id = h.stoat.modal_run.expect("the run modal is open");
        let cmd = h.stoat.active_workspace().runs[run_id].blocks[0];
        h.inject_cmd_done(cmd, 2);

        assert_eq!(
            (spawns(&h), h.stoat.active_workspace().cmds[cmd].exit_status),
            (vec![bash_in("ls", "/work")], Some(2)),
        );
    }

    #[test]
    fn a_finished_block_reads_its_mark_and_runtime() {
        let mut h = Stoat::test();
        h.open_run();
        let ok = h.submit_run("true");
        h.advance_clock(Duration::from_millis(18));
        h.inject_cmd_done(ok.cmd, 0);
        let first = h.cmd_status(ok.cmd);

        let failed = h.submit_run("false");
        h.inject_cmd_done(failed.cmd, 5);
        assert_eq!(
            [first, h.cmd_status(failed.cmd)],
            ["\u{2713} 18ms", "\u{2717} exit 5 0ms"],
        );
    }
}
