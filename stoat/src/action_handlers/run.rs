use super::jump;
use crate::{
    agent_goto,
    app::{Stoat, UpdateEffect},
    buffer_lifecycle,
    buffer_registry::OpenOrigin,
    host::{FsHost, OffsetEncoding},
    pane::{FocusTarget, View},
    run::{
        cargo::TestReport,
        kind::RunKind,
        pty::{self, CmdEnv, CmdSink},
        Cmd, CmdId, RunState,
    },
    workspace::Workspace,
};
use std::path::{Path, PathBuf};

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

/// Run the line at the focused Run pane's prompt as a new Cmd.
///
/// An Enter at an empty prompt goes to the newest block instead, as
/// [`block_enter`] takes it.
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
        let newest = ws.runs.get(id).and_then(|run| run.blocks.last().copied());
        return match newest {
            Some(cmd) => block_enter(stoat, cmd),
            None => UpdateEffect::None,
        };
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

    // The body draws one column inside the stack, which stands one column
    // inside each side of the pane.
    let width = ws.panes.pane(focused).area.width.saturating_sub(3).max(20);
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

/// Take an Enter at an empty prompt to the Cmd `id`, the newest block of the
/// stack.
///
/// A finished Cmd whose report located a failure goes to the first located
/// failure, as [`go_to_location`] opens it. rustc counts a column in chars,
/// so the column lands as UTF-32. Every other Cmd takes the Enter as nothing.
fn block_enter(stoat: &mut Stoat, id: CmdId) -> UpdateEffect {
    let found = stoat.active_workspace().cmds.get(id).and_then(|cmd| {
        let location = cmd.finished_report().and_then(TestReport::first_location)?;
        Some((cmd.cwd.clone(), location.clone()))
    });
    let Some((cwd, location)) = found else {
        return UpdateEffect::None;
    };
    go_to_location(
        stoat,
        &cwd,
        &location.path,
        location.line,
        location.column,
        OffsetEncoding::Utf32,
    );
    UpdateEffect::Redraw
}

/// Open the file at `path` with the cursor on `line` and `column`, both
/// counted from 1, and answer whether it opened.
///
/// The file opens in the pane that [`agent_goto::target_pane`] picks, since
/// the focused pane shows the Run pane, and that pane takes the focus. Its
/// jumplist records where it was. `column` counts in the units of `encoding`.
///
/// A path that names no file, as [`located_file`] reads it from `base`, sets
/// the status `no file <path>` and opens nothing.
fn go_to_location(
    stoat: &mut Stoat,
    base: &Path,
    path: &str,
    line: u32,
    column: u32,
    encoding: OffsetEncoding,
) -> bool {
    let Some(file) = located_file(&*stoat.fs_host, base, path) else {
        stoat.set_status(format!("no file {path}"));
        return false;
    };

    let target = {
        let executor = stoat.executor.clone();
        let ws = stoat.active_workspace_mut();
        let pane = agent_goto::target_pane(ws, executor);
        ws.focus = FocusTarget::SplitPane;
        ws.panes.set_focus(pane);
        pane
    };
    jump::push_jump(stoat);
    if buffer_lifecycle::open_file_in_pane_now(stoat, target, &file, OpenOrigin::Visited).is_none()
    {
        return false;
    }

    let Some(offset) =
        stoat.offset_for_focused_point(line.saturating_sub(1), column.saturating_sub(1), encoding)
    else {
        return false;
    };
    stoat.collapse_focused_cursor_to(offset);
    true
}

/// The file that a location's `path` names, read from `base`.
///
/// An absolute path names itself. A relative one names the first file it
/// joins to from `base` or from an ancestor of `base`, since cargo names a
/// path from the workspace root and a Cmd can run in a member's directory.
/// `None` when no path it names is a file.
fn located_file(fs: &dyn FsHost, base: &Path, path: &str) -> Option<PathBuf> {
    let is_file =
        |candidate: &Path| matches!(fs.metadata(candidate), Ok(Some(meta)) if !meta.is_dir);
    if Path::new(path).is_absolute() {
        return Some(PathBuf::from(path)).filter(|file| is_file(file));
    }
    base.ancestors()
        .map(|dir| dir.join(path))
        .find(|file| is_file(file))
}

/// Flip the newest block of the focused Run pane between its report and its
/// raw output.
///
/// Only a block with a [`Cmd::finished_report`] flips, so the key does
/// nothing while the Cmd runs and for a Cmd that keeps no report.
pub(super) fn run_toggle_view(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let View::Run(id) = ws.panes.pane(ws.panes.focus()).view else {
        return UpdateEffect::None;
    };
    let Workspace { runs, cmds, .. } = ws;
    let Some(cmd) = runs
        .get(id)
        .and_then(|run_state| run_state.active_block_mut(cmds))
        .filter(|cmd| cmd.finished_report().is_some())
    else {
        return UpdateEffect::None;
    };
    cmd.raw_view = !cmd.raw_view;
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
/// The run counts one more, starts now, and clears the end and the elapsed
/// seconds of the run before it. A spawn that fails ends the run at once with
/// the error line `spawn failed: <error>`.
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
    cmd.elapsed_secs = 0;
    cmd.finished = false;
    cmd.exit_status = None;
    cmd.error = None;
    cmd.reset_report();
    cmd.raw_view = false;
    let sink = CmdSink {
        tx,
        workspace,
        cmd: id,
        run: cmd.run_count,
    };
    let width = cmd.grid.width();
    // TERM=dumb turns cargo's color off, and the block of a Parsed Cmd shows
    // cargo's colors and lets cargo erase its own status lines.
    let vars = match cmd.kind {
        RunKind::Parsed(_) => vec![("CARGO_TERM_COLOR".to_owned(), "always".to_owned())],
        RunKind::Line | RunKind::Agent => Vec::new(),
    };
    let env = CmdEnv {
        diff: &diff,
        vars: &vars,
    };
    match pty::spawn_cmd(&*host, &executor, &cmd.command, &cmd.cwd, width, sink, &env) {
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
        render::paint::style_rgb,
        run::{
            cargo::{ONE_FAILURE, THREE_BINARIES},
            vterm::BodyRow,
            CmdId, PtyNotification,
        },
        test_harness::{editor, CmdHandle, TestHarness},
        theme::scope,
    };
    use futures::FutureExt;
    use std::{path::PathBuf, time::Duration};
    use stoatty_protocol::command::{self, Command};

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

    #[test]
    fn a_running_cmd_ticks_once_a_second() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("sleep 9");
        let mut reads = vec![h.cmd_status(handle.cmd)];
        for _ in 0..3 {
            h.advance_clock(Duration::from_secs(1));
            h.snapshot();
            reads.push(h.cmd_status(handle.cmd));
        }

        assert_eq!(
            reads,
            [
                "\u{25cf} 0:00",
                "\u{25cf} 0:01",
                "\u{25cf} 0:02",
                "\u{25cf} 0:03"
            ],
        );
    }

    /// With no output and no input, the tick wakes the run loop at each second
    /// of a running Cmd, so each second's count reaches the screen.
    #[test]
    fn a_running_cmd_wakes_the_run_loop_each_second() {
        let mut h = Stoat::test();
        h.open_run();
        h.submit_run("sleep 9");
        let notify = h.stoat.redraw_notify.clone();

        let wakes: Vec<bool> = (0..3)
            .map(|_| {
                let _ = notify.notified().now_or_never();
                h.advance_clock(Duration::from_secs(1));
                let woke = notify.notified().now_or_never().is_some();
                h.snapshot();
                woke
            })
            .collect();
        assert_eq!(wakes, [true; 3]);
    }

    /// The tick is armed while a Run pane on show holds a running Cmd, and
    /// dropped before the first Cmd and after the last one ends.
    #[test]
    fn no_tick_is_armed_while_no_cmd_runs() {
        let mut h = Stoat::test();
        h.open_run();
        h.snapshot();
        let idle = h.stoat.cmd_tick.is_some();

        let handle = h.submit_run("sleep 9");
        h.snapshot();
        let running = h.stoat.cmd_tick.is_some();

        h.inject_cmd_done(handle.cmd, 0);
        h.snapshot();
        assert_eq!(
            [idle, running, h.stoat.cmd_tick.is_some()],
            [false, true, false]
        );
    }

    /// A running Cmd in a parked tab arms no tick, and the first frame after
    /// its tab returns reads the seconds that passed while it was parked.
    #[test]
    fn a_cmd_in_a_parked_tab_arms_no_tick() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("sleep 9");
        super::super::dispatch(&mut h.stoat, &stoat_action::NewTab);
        h.snapshot();
        let parked = h.stoat.cmd_tick.is_some();

        h.advance_clock(Duration::from_secs(5));
        super::super::dispatch(&mut h.stoat, &stoat_action::PrevTab);
        h.snapshot();
        assert_eq!(
            (parked, h.cmd_status(handle.cmd)),
            (false, "\u{25cf} 0:05".to_owned())
        );
    }

    /// A submitted `cargo test` line reads its Kind Chip before its elapsed
    /// time, since a Cmd reads its kind from its line when it is made.
    #[test]
    fn a_submitted_cargo_line_leads_its_status_with_its_chip() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("cargo test");

        assert_eq!(h.cmd_status(handle.cmd), "cargo test \u{25cf} 0:00");
    }

    /// A running `cargo test` Cmd counts its result lines against its total,
    /// and a progress bar on its first body row fills the same share of the
    /// body's width.
    #[test]
    fn a_running_cargo_test_counts_its_results() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("cargo test");
        h.inject_cmd_output(
            handle.cmd,
            b"running 4 tests\r\ntest a ... ok\r\ntest b ... ok\r\n",
        );
        let status = h.cmd_status(handle.cmd);
        h.render_composited();

        let area = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        let inner = crate::run::stack_rect(area);
        let (x, y) = (
            ((inner.x + 1) * 16) as i16,
            ((area.bottom() - 4) * 16) as i16,
        );
        let bars: Vec<_> = command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::Bar(bar) if bar.height == 2 && bar.y == y => {
                    Some((bar.x, bar.width, bar.color))
                },
                _ => None,
            })
            .collect();
        let color = |name| style_rgb(h.stoat.theme.get(name).fg).expect("an RGB color");
        let width = (inner.width - 1) * 16;
        assert_eq!(
            (status, bars),
            (
                "cargo test \u{25cf} 2 / 4 \u{b7} 0:00".to_owned(),
                vec![
                    (x, width, color(scope::UI_DIVIDER)),
                    (x, width / 2, color(scope::UI_STATUSLINE_RUN)),
                ],
            ),
        );
    }

    /// A finished `cargo test` Cmd whose report counts a failed test names the
    /// count as its mark, and a run with none reads the plain mark.
    #[test]
    fn a_finished_cargo_test_reads_its_failed_count() {
        let mut h = Stoat::test();
        h.open_run();
        let reads = [(ONE_FAILURE, 101), (THREE_BINARIES, 0)].map(|(output, code)| {
            let handle = h.submit_run("cargo test");
            h.inject_cmd_output(handle.cmd, output.as_bytes());
            h.inject_cmd_done(handle.cmd, code);
            h.cmd_status(handle.cmd)
        });

        assert_eq!(
            reads,
            [
                "cargo test \u{2717} 1 failed 0ms",
                "cargo test \u{2713} 0ms"
            ],
        );
    }

    /// A Parsed Cmd asks cargo for color after `TERM`, which turns it off.
    #[test]
    fn a_parsed_cmd_asks_cargo_for_color() {
        let mut h = Stoat::test();
        h.open_run();
        h.submit_run("cargo test");

        let spawns = h.fake_terminal_host().spawns();
        let env = &spawns.last().expect("a spawn").env;
        assert_eq!(
            env[env.len().saturating_sub(2)..],
            [
                ("TERM".to_owned(), "dumb".to_owned()),
                ("CARGO_TERM_COLOR".to_owned(), "always".to_owned()),
            ],
        );
    }

    /// Run `cargo test` at the focused Run pane's prompt to its end, with
    /// `output` and the exit `code`, and answer its Cmd.
    fn finished_cargo_test(h: &mut TestHarness, output: &str, code: i32) -> CmdId {
        let handle = h.submit_run("cargo test");
        h.inject_cmd_output(handle.cmd, output.as_bytes());
        h.inject_cmd_done(handle.cmd, code);
        handle.cmd
    }

    /// The texts of the rows that the Cmd `cmd` shows in place of its output,
    /// and the text of the hint row of the focused Run pane.
    fn report_and_hint(h: &TestHarness, cmd: CmdId) -> (Option<Vec<String>>, Option<String>) {
        let ws = h.stoat.active_workspace();
        let View::Run(run) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the focused pane shows a Run pane");
        };
        let rows = ws.cmds[cmd]
            .report_rows()
            .map(|rows| rows.iter().map(BodyRow::text).collect());
        (rows, ws.runs[run].hint(&ws.cmds).map(|row| row.text()))
    }

    /// A finished `cargo test` Cmd shows its report in place of its output,
    /// and the newest block's hint names the key that goes to its failure.
    #[test]
    fn a_finished_cargo_test_shows_its_report() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_cargo_test(&mut h, ONE_FAILURE, 101);

        let lines = h.stoat.active_workspace().cmds[cmd]
            .grid
            .rendered_line_count();
        assert_eq!(
            (h.cmd_status(cmd), report_and_hint(&h, cmd)),
            (
                "cargo test \u{2717} 1 failed 0ms".to_owned(),
                (
                    Some(vec![
                        "1 passed  1 failed  3 tests \u{b7} 1 binary".to_owned(),
                        "\u{2717} render::chrome::tests::zoomed_negative_steps   chrome.rs:412"
                            .to_owned(),
                        "  assertion `left == right` failed  left: 18  right: 16".to_owned(),
                        format!("\u{22ef} {lines} lines of output \u{b7} t terminal view"),
                    ]),
                    Some("Enter go to failure".to_owned()),
                ),
            ),
        );
    }

    #[test]
    fn a_passing_run_shows_no_failed_count_and_no_hint() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_cargo_test(&mut h, THREE_BINARIES, 0);

        let lines = h.stoat.active_workspace().cmds[cmd]
            .grid
            .rendered_line_count();
        assert_eq!(
            report_and_hint(&h, cmd),
            (
                Some(vec![
                    "3 passed  3 tests \u{b7} 3 binaries".to_owned(),
                    format!("\u{22ef} {lines} lines of output \u{b7} t terminal view"),
                ]),
                None,
            ),
        );
    }

    /// A `cargo test` Cmd shows its raw output while it runs, and after a run
    /// that started no test binary, such as one that failed to build.
    #[test]
    fn a_running_or_unbuilt_test_run_shows_its_raw_output() {
        let mut h = Stoat::test();
        h.open_run();
        let running = h.submit_run("cargo test");
        h.inject_cmd_output(running.cmd, ONE_FAILURE.as_bytes());
        let unbuilt = finished_cargo_test(&mut h, "error: could not compile `stoat`\n", 101);

        let shows_report = |cmd| h.stoat.active_workspace().cmds[cmd].report_rows().is_some();
        assert_eq!(
            [shows_report(running.cmd), shows_report(unbuilt)],
            [false; 2]
        );
    }

    /// `t` in normal mode turns the newest block's report into its raw output,
    /// which the block lays out as its grid rows, and `t` again turns it back.
    #[test]
    fn t_shows_the_raw_output_and_t_again_the_report() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_cargo_test(&mut h, ONE_FAILURE, 101);
        let read = |h: &TestHarness| {
            let block = &h.stoat.active_workspace().cmds[cmd];
            (block.report_rows().is_some(), block.rendered_line_span())
        };
        let grid_rows = h.stoat.active_workspace().cmds[cmd]
            .grid
            .rendered_line_count();

        let report = read(&h);
        h.type_keys("escape t");
        let raw = read(&h);
        h.type_keys("t");
        assert_eq!(
            [report, raw, read(&h)],
            [(true, 5), (false, 1 + grid_rows), (true, 5)]
        );
    }

    /// `t` flips only a block that has a report to show, so a press while the
    /// run goes on leaves the report to show once it ends.
    #[test]
    fn t_before_the_run_ends_flips_nothing() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("cargo test");
        h.inject_cmd_output(handle.cmd, ONE_FAILURE.as_bytes());
        h.type_keys("escape t");
        h.inject_cmd_done(handle.cmd, 101);

        let cmd = &h.stoat.active_workspace().cmds[handle.cmd];
        assert_eq!((cmd.raw_view, cmd.report_rows().is_some()), (false, true));
    }

    /// An Enter at an empty prompt opens the first failure's file in the
    /// editor pane beside the Run pane, which takes the focus, with the cursor
    /// on the failure's line and column. Cargo names the file from the root.
    #[test]
    fn enter_at_an_empty_prompt_goes_to_the_first_failure() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        let file = PathBuf::from("/work/stoat/src/render/chrome.rs");
        h.seed_fixture(&file, format!("{}\n", "x".repeat(20)).repeat(500));
        let editor_pane = h.stoat.active_workspace().panes.focus();
        h.type_action("SplitRight()");
        h.open_run();
        finished_cargo_test(&mut h, ONE_FAILURE, 101);

        h.type_keys("enter");
        let point = editor::focused_cursor_point(&mut h.stoat);
        assert_eq!(
            (
                h.stoat.active_workspace().panes.focus(),
                editor::focused_buffer_path(&h.stoat),
                (point.row, point.column),
            ),
            (editor_pane, file, (411, 8)),
        );
    }

    /// Cargo names a failure's file from the workspace root, so a Cmd that
    /// ran in a member's directory finds the file under an ancestor of it.
    #[test]
    fn enter_reads_a_failure_path_from_an_ancestor_of_the_cwd() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work/stoat");
        let file = PathBuf::from("/work/stoat/src/render/chrome.rs");
        h.seed_fixture(&file, "x\n".repeat(500));
        h.type_action("SplitRight()");
        h.open_run();
        finished_cargo_test(&mut h, ONE_FAILURE, 101);

        h.type_keys("enter");
        assert_eq!(editor::focused_buffer_path(&h.stoat), file);
    }

    #[test]
    fn enter_on_a_failure_in_no_file_names_the_path() {
        let mut h = Stoat::test();
        h.open_run();
        finished_cargo_test(&mut h, ONE_FAILURE, 101);

        h.type_keys("enter");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no file stoat/src/render/chrome.rs"),
        );
    }
}
