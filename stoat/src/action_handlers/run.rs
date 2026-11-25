use super::jump;
use crate::{
    agent_goto,
    app::{Stoat, UpdateEffect},
    buffer_lifecycle,
    buffer_registry::OpenOrigin,
    host::{FsHost, OffsetEncoding},
    pane::{Axis, FocusTarget, PaneId, View},
    render::paint::style_rgb,
    run::{
        cargo::CmdReport,
        kind::{self, CargoKind, RunKind},
        pty::{self, CmdEnv, CmdSink},
        Cmd, CmdId, RunId, RunState, VtermGrid,
    },
    theme::scope,
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

/// Take an Enter to the Cmd `id`, the newest block of a Run pane's stack or the
/// Cmd that the palette's Run Block shows, and answer a redraw when the Enter
/// acted.
///
/// A finished Cmd whose report located a test failure or a build error goes
/// to the first of them, as [`CmdReport::first_location`] picks it and
/// [`go_to_location`] opens it. rustc counts a column in chars, so the column
/// lands as UTF-32. A finished Cmd that the palette shows with no located
/// failure opens its output, as [`open_cmd_output`] does. Every other Cmd
/// takes the Enter as nothing.
pub(super) fn block_enter(stoat: &mut Stoat, id: CmdId) -> UpdateEffect {
    let found = stoat.active_workspace().cmds.get(id).and_then(|cmd| {
        let location = cmd.finished_report().and_then(CmdReport::first_location)?;
        Some((cmd.cwd.clone(), location.clone()))
    });
    if let Some((cwd, location)) = found {
        go_to_location(
            stoat,
            &cwd,
            &location.path,
            location.line,
            location.column,
            OffsetEncoding::Utf32,
        );
        return UpdateEffect::Redraw;
    }

    let shown = stoat.run_block_cmd() == Some(id);
    let finished = stoat
        .active_workspace()
        .cmds
        .get(id)
        .is_some_and(|cmd| cmd.finished);
    if !(shown && finished) {
        return UpdateEffect::None;
    }
    open_cmd_output(stoat, id);
    UpdateEffect::Redraw
}

/// Show the output of the Cmd `id` in a new scratch buffer, in the pane that
/// [`agent_goto::target_pane`] picks, which takes the focus.
fn open_cmd_output(stoat: &mut Stoat, id: CmdId) {
    let Some(text) = stoat.active_workspace().cmds.get(id).map(|cmd| {
        let grid = &cmd.grid;
        grid.text_in(0..usize::from(grid.width()), 0..grid.rendered_line_count())
    }) else {
        return;
    };
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let pane = agent_goto::target_pane(ws, executor);
    ws.focus = FocusTarget::SplitPane;
    ws.panes.set_focus(pane);
    buffer_lifecycle::show_scratch_with_text(stoat, &text);
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

/// Flip a block between its report and its raw output, the Cmd that the
/// palette's Run Block shows or else the newest block of the focused Run pane.
///
/// Only a block with a [`Cmd::finished_report`] flips, so the key does
/// nothing while the Cmd runs and for a Cmd that keeps no report.
pub(super) fn run_toggle_view(stoat: &mut Stoat) -> UpdateEffect {
    let shown = stoat.run_block_cmd();
    let ws = stoat.active_workspace_mut();
    let newest = || {
        let View::Run(run) = ws.panes.pane(ws.panes.focus()).view else {
            return None;
        };
        ws.runs.get(run)?.blocks.last().copied()
    };
    let Some(id) = shown.or_else(newest) else {
        return UpdateEffect::None;
    };
    let Some(cmd) = ws
        .cmds
        .get_mut(id)
        .filter(|cmd| cmd.finished_report().is_some())
    else {
        return UpdateEffect::None;
    };
    cmd.raw_view = !cmd.raw_view;
    UpdateEffect::Redraw
}

/// Run a finished Cmd again in place, the Cmd of the palette's Run Block when
/// the stage shows, else the newest finished Cmd of the focused Run pane's
/// stack.
///
/// A running Cmd of the Run Block stays as it is, and so does a Run pane with
/// no finished Cmd. The Cmd keeps its place, as [`restart_cmd`] starts it.
pub(super) fn run_again(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace();
    let finished = |id: &CmdId| ws.cmds.get(*id).is_some_and(|cmd| cmd.finished);
    let id = match stoat.run_block_cmd() {
        Some(shown) => Some(shown).filter(finished),
        None => {
            let View::Run(run) = ws.panes.pane(ws.panes.focus()).view else {
                return UpdateEffect::None;
            };
            ws.runs
                .get(run)
                .and_then(|run_state| run_state.blocks.iter().rev().copied().find(finished))
        },
    };
    let Some(id) = id else {
        return UpdateEffect::None;
    };
    restart_cmd(stoat, id);
    UpdateEffect::Redraw
}

/// Start the next run of the Cmd `id` with an empty output, in the directory
/// and in the place where it ran before.
fn restart_cmd(stoat: &mut Stoat, id: CmdId) {
    let Some(cmd) = stoat.active_workspace_mut().cmds.get_mut(id) else {
        return;
    };
    cmd.grid = VtermGrid::new(cmd.grid.width());
    cmd.selection = None;
    start_process(stoat, id);
}

/// Move the Cmd of the palette's Run Block, running or finished, onto a Run
/// pane's stack, and close the palette.
///
/// The Cmd stays in the store and its process runs on. The Run pane, as
/// [`append_to_run_pane`] picks it, takes the focus and its prompt takes
/// typing.
pub(super) fn run_block_to_pane(stoat: &mut Stoat) -> UpdateEffect {
    let Some(cmd) = stoat.run_block_cmd() else {
        return UpdateEffect::None;
    };
    let Some(palette) = stoat.command_palette.take() else {
        return UpdateEffect::None;
    };
    super::palette::dispose_palette(stoat, palette);
    append_to_run_pane(stoat, cmd, true);
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

pub(super) fn run_command(stoat: &mut Stoat, command: &str) -> UpdateEffect {
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
    start_cmd_as(stoat, line, None, None, width)
}

/// Insert a Cmd of `line` in the active workspace's store, with the workspace
/// root as its cwd and a grid `width` cells wide, and start its process.
///
/// A `shell` text runs in place of the line, and the Cmd's Run Kind comes from
/// it, so a config command's line names the command and its run reads the
/// shell line. `source` is the Source Tag that the Cmd's header shows.
pub(crate) fn start_cmd_as(
    stoat: &mut Stoat,
    line: &str,
    shell: Option<&str>,
    source: Option<(&'static str, &'static str)>,
    width: u16,
) -> CmdId {
    let ws = stoat.active_workspace_mut();
    let mut cmd = Cmd::new(line.to_owned(), ws.git_root.clone(), width);
    if let Some(shell) = shell {
        cmd.kind = kind::run_kind(shell);
        cmd.shell = Some(shell.to_owned());
    }
    cmd.source = source;
    let id = ws.cmds.insert(cmd);
    start_process(stoat, id);
    id
}

/// The grid width of a Cmd that [`append_to_run_pane`] places, in cells.
///
/// It is the width of the pane that takes the Cmd, or of the focused pane
/// when a new pane splits it, less the stack's inset and the body's column,
/// and at least 20.
pub(crate) fn run_pane_width(stoat: &Stoat) -> u16 {
    let ws = stoat.active_workspace();
    let pane = run_pane_on_show(ws).map_or_else(|| ws.panes.focus(), |(pane, _)| pane);
    ws.panes.pane(pane).area.width.saturating_sub(3).max(20)
}

/// Push the Cmd `cmd` onto a Run pane's stack, and answer that pane's run.
///
/// The focused split pane takes the Cmd when it shows a Run pane, else the
/// first split pane that shows one, else a new Run pane that splits the
/// focused pane downward, as `SplitDown` does. The stack snaps back to its
/// prompt. With `focus`, the pane takes the focus and its prompt takes
/// typing. Without it, the focus stays where it is.
pub(crate) fn append_to_run_pane(stoat: &mut Stoat, cmd: CmdId, focus: bool) -> RunId {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let (pane, run) = match run_pane_on_show(ws) {
        Some(shown) => shown,
        None => {
            let focused = ws.panes.focus();
            let state = RunState::new(ws, executor);
            let run = ws.runs.insert(state);
            let pane = ws.panes.split(Axis::Horizontal);
            ws.panes.pane_mut(pane).view = View::Run(run);
            ws.panes.set_focus(focused);
            (pane, run)
        },
    };

    let Workspace { runs, cmds, .. } = &mut *ws;
    if let Some(run_state) = runs.get_mut(run) {
        run_state.blocks.push(cmd);
        run_state.scroll_offset = 0;
        run_state.trim_blocks(cmds);
    }
    if focus {
        ws.focus = FocusTarget::SplitPane;
        ws.panes.set_focus(pane);
        stoat.transition_mode("insert".into());
    }
    run
}

/// The split pane that shows a Run pane and its run, the focused pane when it
/// shows one and else the first such pane, or `None` with no Run pane on
/// show.
fn run_pane_on_show(ws: &Workspace) -> Option<(PaneId, RunId)> {
    let focused = ws.panes.focus();
    if let View::Run(run) = ws.panes.pane(focused).view {
        return Some((focused, run));
    }
    ws.panes
        .split_panes()
        .find_map(|(pane, shown)| match shown.view {
            View::Run(run) => Some((pane, run)),
            _ => None,
        })
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
    let muted = style_rgb(stoat.theme.get(scope::UI_TEXT_MUTED).fg);

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
    // cargo's colors and lets cargo erase its own status lines. Cargo draws
    // its progress bar to a dumb terminal only when asked, and only with a
    // width. Each redraw fills the grid's width and ends in a carriage return,
    // so the bar keeps to one row. A test run's progress counts its tests, so
    // it asks for no bar.
    let color = ("CARGO_TERM_COLOR".to_owned(), "always".to_owned());
    let vars = match cmd.kind {
        RunKind::Parsed(CargoKind::Test) => vec![color],
        RunKind::Parsed(CargoKind::Build | CargoKind::Check | CargoKind::Clippy) => vec![
            color,
            ("CARGO_TERM_PROGRESS_WHEN".to_owned(), "always".to_owned()),
            ("CARGO_TERM_PROGRESS_WIDTH".to_owned(), width.to_string()),
        ],
        RunKind::Line | RunKind::Agent => Vec::new(),
    };
    let env = CmdEnv {
        diff: &diff,
        vars: &vars,
    };
    if let Some(shell) = cmd.shell.as_ref().filter(|shell| **shell != cmd.command) {
        let row = match muted {
            Some([r, g, b]) => format!("\x1b[38;2;{r};{g};{b}m{shell}\x1b[0m\r\n"),
            None => format!("{shell}\r\n"),
        };
        cmd.grid.feed(row.as_bytes());
    }
    match pty::spawn_cmd(
        &*host,
        &executor,
        cmd.run_text(),
        &cmd.cwd,
        width,
        sink,
        &env,
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
        host::FakeTerminalSession,
        pane::View,
        render::paint::style_rgb,
        run::{
            cargo::{ERROR_AND_WARNING, ONE_FAILURE, THREE_BINARIES},
            vterm::BodyRow,
            CmdId, GridSelection, PtyNotification,
        },
        test_harness::{editor, CmdHandle, TestHarness},
        theme::scope,
    };
    use futures::FutureExt;
    use std::{path::PathBuf, sync::Arc, time::Duration};
    use stoatty_protocol::command::{self, Command};

    /// The file that the failure and the error of the samples name, under the
    /// root `/work`.
    const CHROME: &str = "/work/stoat/src/render/chrome.rs";

    /// The user command `hello`, which runs `echo hi` under the default
    /// `block` output.
    const HELLO: &str = r#"command hello { run = shell("echo hi"); }"#;

    /// The program, the arguments, the cwd, and the last variable of a spawn of
    /// the fake terminal host.
    type Spawn = (String, Vec<String>, PathBuf, Option<(String, String)>);

    /// A progress bar that the run pane draws, as its left edge, its width,
    /// and its color.
    type ProgressBar = (i16, u16, [u8; 3]);

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

        let (x, width, bars) = progress_bars(&mut h, 4);
        let color = |name| style_rgb(h.stoat.theme.get(name).fg).expect("an RGB color");
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

    /// A running build reads the counts of cargo's progress bar, and a
    /// progress bar on its first body row fills the same share of the body's
    /// width.
    #[test]
    fn a_running_build_reads_its_progress() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("cargo build");
        h.inject_cmd_output(
            handle.cmd,
            b"\x1b[1m\x1b[96m    Building\x1b[0m [=====>      ] 212/240: stoat(build)\r",
        );
        let status = h.cmd_status(handle.cmd);

        let (x, width, bars) = progress_bars(&mut h, 2);
        let color = |name| style_rgb(h.stoat.theme.get(name).fg).expect("an RGB color");
        let fill = u16::try_from(u32::from(width) * 212 / 240).expect("a fill inside the track");
        assert_eq!(
            (status, bars),
            (
                "cargo build \u{25cf} 212 / 240 \u{b7} 0:00".to_owned(),
                vec![
                    (x, width, color(scope::UI_DIVIDER)),
                    (x, fill, color(scope::UI_STATUSLINE_RUN)),
                ],
            ),
        );
    }

    /// The bars two pixels high that the focused Run pane draws on the row
    /// `rows_up` rows above its bottom edge, each as its left edge, its width,
    /// and its color, after the left edge and the width of a full track.
    fn progress_bars(h: &mut TestHarness, rows_up: u16) -> (i16, u16, Vec<ProgressBar>) {
        h.render_composited();
        let area = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        let inner = crate::run::stack_rect(area);
        let y = ((area.bottom() - rows_up) * 16) as i16;
        let bars = command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::Bar(bar) if bar.height == 2 && bar.y == y => {
                    Some((bar.x, bar.width, bar.color))
                },
                _ => None,
            })
            .collect();
        (((inner.x + 1) * 16) as i16, (inner.width - 1) * 16, bars)
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

    /// Run `line` at the focused Run pane's prompt to its end, with `output`
    /// and the exit `code`, and answer its Cmd.
    fn finished_run(h: &mut TestHarness, line: &str, output: &str, code: i32) -> CmdId {
        let handle = h.submit_run(line);
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
        let cmd = finished_run(&mut h, "cargo test", ONE_FAILURE, 101);

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
                    Some("C-r run again \u{b7} Enter go to failure".to_owned()),
                ),
            ),
        );
    }

    #[test]
    fn a_passing_run_shows_no_failed_count_and_no_hint() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_run(&mut h, "cargo test", THREE_BINARIES, 0);

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
        let unbuilt = finished_run(
            &mut h,
            "cargo test",
            "error: could not compile `stoat`\n",
            101,
        );

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
        let cmd = finished_run(&mut h, "cargo test", ONE_FAILURE, 101);
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

    /// Run `line` to its end with `output` and the exit `code` in a Run pane
    /// beside an editor pane, with the root `/work` over a 500-line
    /// [`CHROME`], and press Enter at the empty prompt.
    ///
    /// Answers whether the editor pane took the focus, the file it shows, and
    /// its cursor's row and column, from 0.
    fn enter_after(line: &str, output: &str, code: i32) -> (bool, PathBuf, (u32, u32)) {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.seed_fixture(CHROME, format!("{}\n", "x".repeat(20)).repeat(500));
        let editor_pane = h.stoat.active_workspace().panes.focus();
        h.type_action("SplitRight()");
        h.open_run();
        finished_run(&mut h, line, output, code);

        h.type_keys("enter");
        let point = editor::focused_cursor_point(&mut h.stoat);
        (
            h.stoat.active_workspace().panes.focus() == editor_pane,
            editor::focused_buffer_path(&h.stoat),
            (point.row, point.column),
        )
    }

    /// An Enter at an empty prompt opens the first failure's file in the
    /// editor pane beside the Run pane, which takes the focus, with the cursor
    /// on the failure's line and column. Cargo names the file from the root.
    #[test]
    fn enter_at_an_empty_prompt_goes_to_the_first_failure() {
        assert_eq!(
            enter_after("cargo test", ONE_FAILURE, 101),
            (true, PathBuf::from(CHROME), (411, 8)),
        );
    }

    /// The Enter after a failed build goes to its first error, past the
    /// warning that came before it.
    #[test]
    fn enter_goes_to_the_first_error() {
        assert_eq!(
            enter_after("cargo build", ERROR_AND_WARNING, 101),
            (true, PathBuf::from(CHROME), (411, 19)),
        );
    }

    /// A build asks cargo for its progress bar as wide as the block's grid,
    /// after the color, and a test run asks for no bar.
    #[test]
    fn a_build_cmd_asks_cargo_for_its_progress_bar() {
        let mut h = Stoat::test();
        h.open_run();
        let build = h.submit_run("cargo build");
        h.submit_run("cargo test");

        let width = h.stoat.active_workspace().cmds[build.cmd]
            .grid
            .width()
            .to_string();
        let spawns = h.fake_terminal_host().spawns();
        let (build_env, test_env) = (&spawns[0].env, &spawns[1].env);
        let var = |name: &str, value: &str| (name.to_owned(), value.to_owned());
        assert_eq!(
            (
                build_env[build_env.len().saturating_sub(4)..].to_vec(),
                test_env
                    .iter()
                    .any(|(name, _)| name.starts_with("CARGO_TERM_PROGRESS")),
            ),
            (
                vec![
                    var("TERM", "dumb"),
                    var("CARGO_TERM_COLOR", "always"),
                    var("CARGO_TERM_PROGRESS_WHEN", "always"),
                    var("CARGO_TERM_PROGRESS_WIDTH", &width),
                ],
                false,
            ),
        );
    }

    /// A finished build with an error names its count as its mark, and its
    /// report counts both kinds and lists the errors before the warnings.
    #[test]
    fn a_finished_build_counts_its_errors_and_warnings() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_run(&mut h, "cargo build", ERROR_AND_WARNING, 101);

        let lines = h.stoat.active_workspace().cmds[cmd]
            .grid
            .rendered_line_count();
        assert_eq!(
            (h.cmd_status(cmd), report_and_hint(&h, cmd)),
            (
                "cargo build \u{2717} 1 error 0ms".to_owned(),
                (
                    Some(vec![
                        "1 error  1 warning".to_owned(),
                        "\u{2717} mismatched types   chrome.rs:412".to_owned(),
                        "\u{25b2} unused variable: `width`   chrome.rs:210".to_owned(),
                        format!("\u{22ef} {lines} lines of output \u{b7} t terminal view"),
                    ]),
                    Some("C-r run again \u{b7} Enter go to failure".to_owned()),
                ),
            ),
        );
    }

    /// Each row of a build's report draws its errors in the error color and
    /// its warnings in the warning color, and its locations muted.
    #[test]
    fn a_build_report_colors_errors_and_warnings_apart() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_run(&mut h, "cargo build", ERROR_AND_WARNING, 101);

        let scopes: Vec<Vec<&str>> = h.stoat.active_workspace().cmds[cmd]
            .report_rows()
            .expect("a report")
            .iter()
            .map(|row| row.spans.iter().map(|(_, tone)| tone.scope()).collect())
            .collect();
        assert_eq!(
            scopes,
            [
                vec![scope::UI_ERROR, scope::UI_DIAGNOSTIC_WARNING],
                vec![scope::UI_ERROR, scope::UI_TEXT_MUTED],
                vec![scope::UI_DIAGNOSTIC_WARNING, scope::UI_TEXT_MUTED],
                vec![scope::UI_TEXT_MUTED],
            ],
        );
    }

    /// A build with warnings alone shows its report, with its mark of a
    /// passing run, and offers no Enter, which never goes to a warning.
    #[test]
    fn a_build_with_warnings_alone_shows_its_report() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_run(
            &mut h,
            "cargo build",
            concat!(
                "warning: unused variable: `width`\n",
                "  --> stoat/src/render/chrome.rs:210:9\n",
            ),
            0,
        );

        let lines = h.stoat.active_workspace().cmds[cmd]
            .grid
            .rendered_line_count();
        assert_eq!(
            (h.cmd_status(cmd), report_and_hint(&h, cmd)),
            (
                "cargo build \u{2713} 0ms".to_owned(),
                (
                    Some(vec![
                        "1 warning".to_owned(),
                        "\u{25b2} unused variable: `width`   chrome.rs:210".to_owned(),
                        format!("\u{22ef} {lines} lines of output \u{b7} t terminal view"),
                    ]),
                    None,
                ),
            ),
        );
    }

    /// A build that ends with no error and no warning keeps cargo's own lines.
    #[test]
    fn a_clean_build_keeps_its_raw_output() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_run(
            &mut h,
            "cargo build",
            concat!(
                "   Compiling stoat v0.4.0\n",
                "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.44s\n",
            ),
            0,
        );

        assert_eq!(
            (h.cmd_status(cmd), report_and_hint(&h, cmd)),
            ("cargo build \u{2713} 0ms".to_owned(), (None, None)),
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
        finished_run(&mut h, "cargo test", ONE_FAILURE, 101);

        h.type_keys("enter");
        assert_eq!(editor::focused_buffer_path(&h.stoat), file);
    }

    #[test]
    fn enter_on_a_failure_in_no_file_names_the_path() {
        let mut h = Stoat::test();
        h.open_run();
        finished_run(&mut h, "cargo test", ONE_FAILURE, 101);

        h.type_keys("enter");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no file stoat/src/render/chrome.rs"),
        );
    }

    /// A harness with the user command [`HELLO`] loaded.
    fn with_hello() -> TestHarness {
        let mut h = Stoat::test();
        h.stoat.reload_user_config(HELLO);
        h
    }

    /// Run `hello` from the palette with Enter, with a fresh fake session queued
    /// for its process, and answer the Run Block's Cmd and the session.
    fn enter_hello(h: &mut TestHarness) -> (CmdId, Arc<FakeTerminalSession>) {
        let session = Arc::new(FakeTerminalSession::new());
        h.fake_terminal_host().push_session(session.clone());
        super::super::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
        h.type_text("hello");
        h.type_keys("enter");
        let cmd = h.stoat.run_block_cmd().expect("a Run Block");
        (cmd, session)
    }

    /// The program and the arguments of each spawn of the fake terminal host.
    fn programs(h: &TestHarness) -> Vec<(String, Vec<String>)> {
        spawns(h)
            .into_iter()
            .map(|(program, args, ..)| (program, args))
            .collect()
    }

    /// `Ctrl-r` at a Run pane's prompt runs its finished Cmd again in the same
    /// block with an empty grid, and the block's status names the second run
    /// once it ends.
    #[test]
    fn ctrl_r_runs_a_finished_cmd_again_in_its_block() {
        let mut h = Stoat::test();
        let run = h.open_run();
        let handle = h.submit_run("make");
        h.inject_cmd_output(handle.cmd, b"one\r\n");
        h.inject_cmd_done(handle.cmd, 1);
        h.stoat.active_workspace_mut().cmds[handle.cmd].selection = Some(GridSelection {
            anchor: (0, 0),
            head: (2, 0),
        });
        h.fake_terminal_host()
            .push_session(Arc::new(FakeTerminalSession::new()));
        h.type_keys("ctrl-r");

        let restarted = {
            let cmd = &h.stoat.active_workspace().cmds[handle.cmd];
            (
                cmd.finished,
                cmd.run_count,
                cmd.grid.rendered_line_count(),
                cmd.selection.is_none(),
            )
        };
        h.advance_clock(Duration::from_secs(151));
        h.inject_cmd_done(handle.cmd, 0);
        let make = ("bash".to_owned(), vec!["-c".to_owned(), "make".to_owned()]);
        assert_eq!(
            (
                programs(&h),
                h.stoat.active_workspace().runs[run].blocks.clone(),
                restarted,
                h.cmd_status(handle.cmd),
            ),
            (
                vec![make.clone(), make],
                vec![handle.cmd],
                (false, 2, 0, true),
                "\u{2713} 2:31 \u{b7} run 2".to_owned(),
            ),
        );
    }

    /// `Ctrl-r` leaves a running Cmd alone, at a Run pane's prompt and on the
    /// palette's Run Block.
    #[test]
    fn ctrl_r_leaves_a_running_cmd_alone() {
        let mut pane = Stoat::test();
        pane.open_run();
        let handle = pane.submit_run("sleep 9");
        pane.type_keys("ctrl-r");

        let mut block = with_hello();
        let (cmd, _) = enter_hello(&mut block);
        block.type_keys("ctrl-r");

        let runs = |h: &TestHarness, cmd: CmdId| {
            (
                spawns(h).len(),
                h.stoat.active_workspace().cmds[cmd].run_count,
            )
        };
        assert_eq!([runs(&pane, handle.cmd), runs(&block, cmd)], [(1, 1); 2]);
    }

    /// `Ctrl-r` in a Run pane's normal mode runs the newest finished Cmd of the
    /// stack again, and the older one keeps its run.
    #[test]
    fn ctrl_r_in_normal_mode_runs_the_newest_finished_cmd() {
        let mut h = Stoat::test();
        h.open_run();
        let [older, newer] = ["make", "ls"].map(|line| {
            let handle = h.submit_run(line);
            h.inject_cmd_done(handle.cmd, 0);
            handle.cmd
        });
        h.type_keys("escape ctrl-r");

        let ws = h.stoat.active_workspace();
        assert_eq!([ws.cmds[older].run_count, ws.cmds[newer].run_count], [1, 2]);
    }

    /// Output that the first run's reader sends after the restart never lands
    /// in the second run's grid.
    #[test]
    fn output_of_the_first_run_does_not_reach_the_second() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("make");
        h.inject_cmd_done(handle.cmd, 1);
        h.type_keys("ctrl-r");
        h.stoat.handle_pty_notification(PtyNotification::CmdOutput {
            workspace: h.stoat.active_workspace,
            cmd: handle.cmd,
            run: 1,
            data: b"stale\r\n".to_vec(),
        });

        let cmd = &h.stoat.active_workspace().cmds[handle.cmd];
        assert_eq!((cmd.run_count, cmd.grid.rendered_line_count()), (2, 0));
    }

    /// `Ctrl-r` on the palette's finished Run Block runs its Cmd again, and the
    /// palette stays open on it.
    #[test]
    fn ctrl_r_on_the_palette_block_runs_it_again_in_place() {
        let mut h = with_hello();
        let (cmd, _) = enter_hello(&mut h);
        h.inject_cmd_done(cmd, 0);
        h.type_keys("ctrl-r");

        let shown = &h.stoat.active_workspace().cmds[cmd];
        assert_eq!(
            (h.stoat.run_block_cmd(), shown.finished, shown.run_count),
            (Some(cmd), false, 2),
        );
    }

    /// `Ctrl-r` in the palette's typing stage over a Run pane with a finished
    /// Cmd stays the palette's, so no Cmd runs again.
    #[test]
    fn ctrl_r_in_the_typing_stage_over_a_run_pane_runs_no_cmd() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("make");
        h.inject_cmd_done(handle.cmd, 0);
        super::super::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
        h.type_keys("ctrl-r");
        assert_eq!(
            (
                spawns(&h).len(),
                h.stoat.active_workspace().cmds[handle.cmd].run_count
            ),
            (1, 1),
        );
    }

    /// `Ctrl-o` moves the running Run Block's Cmd onto the stack of a new Run
    /// pane under the editor pane, which takes the focus in insert mode. The
    /// Cmd runs on, its later output reaches it, and the palette's input goes.
    #[test]
    fn ctrl_o_moves_a_running_block_into_a_new_run_pane() {
        let mut h = with_hello();
        let (cmd, session) = enter_hello(&mut h);
        let input = h
            .stoat
            .command_palette
            .as_ref()
            .expect("the palette is open")
            .input
            .editor_id;
        h.type_keys("ctrl-o");
        h.inject_cmd_output(cmd, b"later\r\n");

        let ws = h.stoat.active_workspace();
        let lower = ws
            .panes
            .split_panes()
            .max_by_key(|(_, pane)| pane.area.y)
            .map(|(id, _)| id)
            .expect("a split pane");
        let View::Run(run) = ws.panes.pane(lower).view else {
            panic!("the lower pane shows a Run pane");
        };
        let last_row = {
            let grid = &ws.cmds[cmd].grid;
            let rows = grid.rendered_line_count();
            grid.text_in(0..usize::from(grid.width()), rows - 1..rows)
        };
        assert_eq!(
            (
                h.stoat.command_palette.is_none(),
                ws.editors.contains_key(input),
                ws.panes.split_panes().count(),
                ws.runs[run].blocks.clone(),
                ws.panes.focus() == lower,
                h.stoat.focused_mode(),
                session.was_killed(),
                last_row,
            ),
            (
                true,
                false,
                2,
                vec![cmd],
                true,
                "insert",
                false,
                "later".to_owned()
            ),
        );
    }

    /// `Ctrl-o` puts the Run Block's Cmd on the stack of the Run pane on show,
    /// and no pane opens.
    #[test]
    fn ctrl_o_appends_to_the_run_pane_that_shows() {
        let mut h = with_hello();
        super::super::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        let run = h.open_run();
        h.submit_run("ls");
        super::super::dispatch(&mut h.stoat, &stoat_action::FocusLeft);
        let (cmd, _) = enter_hello(&mut h);
        h.type_keys("ctrl-o");

        let ws = h.stoat.active_workspace();
        let stack = &ws.runs[run].blocks;
        assert_eq!(
            (
                ws.panes.split_panes().count(),
                stack.len(),
                stack.last().copied()
            ),
            (2, 2, Some(cmd)),
        );
    }

    #[test]
    fn ctrl_o_moves_a_finished_block_too() {
        let mut h = with_hello();
        let (cmd, _) = enter_hello(&mut h);
        h.inject_cmd_done(cmd, 0);
        h.type_keys("ctrl-o");

        let ws = h.stoat.active_workspace();
        let stacked: Vec<CmdId> = ws
            .runs
            .values()
            .flat_map(|run| run.blocks.clone())
            .collect();
        assert_eq!(
            (
                h.stoat.command_palette.is_none(),
                stacked,
                ws.cmds[cmd].finished
            ),
            (true, vec![cmd], true),
        );
    }
}
