//! The Job Tray of a workspace, the Cmds whose placement is the tray rather
//! than a Run pane's stack.
//!
//! A Job is a placement of one Cmd, which stays in [`Workspace::cmds`] with its
//! process, its grid, and its report, so its output and its exit reach it as
//! they reach any Cmd. [`Workspace::jobs`] lists the ids, and
//! [`Cmd::in_jobs`] marks the block of each one as a Stub of the stack that
//! holds its id. [`push`] and [`take`] are the only writers of the two.

use crate::{
    run::{self, Cmd, CmdId, RunId},
    theme::{scope, Theme},
    workspace::Workspace,
};
use ratatui::style::Style;

/// The state of a Job, one of the four that a Cmd reaches, which colors the
/// Job wherever it shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobState {
    Running,
    Ok,
    Failed,
    Stopped,
}

impl JobState {
    /// The mark that leads the text of the state.
    ///
    /// It is U+25CF while the Cmd runs, and U+2713, U+2717, or U+25A0 for a
    /// Cmd that ended ok, failed, or stopped.
    pub(crate) fn mark(self) -> char {
        match self {
            JobState::Running => '\u{25cf}',
            JobState::Ok => '\u{2713}',
            JobState::Failed => '\u{2717}',
            JobState::Stopped => '\u{25a0}',
        }
    }
}

/// What a Job shows of its Cmd wherever the Job draws, such as its Tray Chip.
pub(crate) struct JobView {
    pub(crate) state: JobState,
    /// The first two words of the Cmd's line, one space apart, which name
    /// the program and its first argument, such as `cargo test`.
    pub(crate) name: String,
    /// The done and the total count of a running Cmd's progress, as
    /// [`Cmd::progress`] reads them.
    pub(crate) progress: Option<(u32, u32)>,
    /// The time of the Cmd, as [`time_text`] reads it.
    pub(crate) time: String,
}

/// Put the Cmd `id` in the tray, and answer whether it went in.
///
/// A Cmd that the store does not hold, or that the tray holds, stays as it is.
pub(crate) fn push(ws: &mut Workspace, id: CmdId) -> bool {
    if ws.jobs.contains(&id) {
        return false;
    }
    let Some(cmd) = ws.cmds.get_mut(id) else {
        return false;
    };
    cmd.in_jobs = true;
    ws.jobs.push(id);
    true
}

/// Take the Cmd `id` out of the tray, and answer whether the tray held it.
pub(crate) fn take(ws: &mut Workspace, id: CmdId) -> bool {
    let Some(at) = ws.jobs.iter().position(|job| *job == id) else {
        return false;
    };
    ws.jobs.remove(at);
    if let Some(cmd) = ws.cmds.get_mut(id) {
        cmd.in_jobs = false;
    }
    true
}

/// Move the newest block of the run `run` that still runs and is not in the
/// tray to the tray, and answer whether one moved.
///
/// A Run pane has no block cursor, so the newest such block is the one the
/// move takes.
pub(crate) fn push_from_run(ws: &mut Workspace, run: RunId) -> bool {
    let newest = ws.runs.get(run).and_then(|run_state| {
        run_state.blocks.iter().rev().copied().find(|id| {
            ws.cmds
                .get(*id)
                .is_some_and(|cmd| !cmd.finished && !cmd.in_jobs)
        })
    });
    newest.is_some_and(|id| push(ws, id))
}

/// Take the newest block of the run `run` that the tray holds back to its
/// stack, running or ended, and answer whether one came back.
pub(crate) fn return_to_run(ws: &mut Workspace, run: RunId) -> bool {
    let newest = ws.runs.get(run).and_then(|run_state| {
        run_state
            .blocks
            .iter()
            .rev()
            .copied()
            .find(|id| ws.cmds.get(*id).is_some_and(|cmd| cmd.in_jobs))
    });
    newest.is_some_and(|id| take(ws, id))
}

/// The state of `cmd`.
///
/// A run that exits 0 is ok even when the user interrupted it, since its
/// program finished. A run that the user interrupted and that exits with
/// another code is stopped, because the exit code of an interrupted process
/// differs from program to program. A run that ended with no exit status
/// is stopped too, and every other ended run failed.
pub(crate) fn state_of(cmd: &Cmd) -> JobState {
    if !cmd.finished {
        return JobState::Running;
    }
    match cmd.exit_status {
        Some(0) => JobState::Ok,
        Some(_) if !cmd.stopped => JobState::Failed,
        _ => JobState::Stopped,
    }
}

/// The style that draws `state`, the foreground of its `ui.job` scope alone.
///
/// A theme that leaves that scope unset draws the state in the foreground of
/// `ui.statusline.run`, `ui.badge.complete`, `ui.badge.error`, or
/// `ui.text.muted`. A Job's text stands on the surface under it, so the style
/// carries no background.
pub(crate) fn state_style(theme: &Theme, state: JobState) -> Style {
    let (job, fallback) = match state {
        JobState::Running => (scope::UI_JOB_RUNNING, scope::UI_STATUSLINE_RUN),
        JobState::Ok => (scope::UI_JOB_OK, scope::UI_BADGE_COMPLETE),
        JobState::Failed => (scope::UI_JOB_FAILED, scope::UI_BADGE_ERROR),
        JobState::Stopped => (scope::UI_JOB_STOPPED, scope::UI_TEXT_MUTED),
    };
    let fg = theme.try_get(job).unwrap_or_else(|| theme.get(fallback)).fg;
    Style {
        fg,
        ..Style::default()
    }
}

/// The time that the text of a Job reads for `cmd`.
///
/// A running Cmd reads the whole seconds it has run, as
/// [`run::format_elapsed`] gives them, and a failed Cmd reads `exit N`. Any
/// other ended Cmd reads its runtime, as [`run::format_runtime`] gives it, or
/// the empty text when it lacks a start or an end.
pub(crate) fn time_text(cmd: &Cmd) -> String {
    match (state_of(cmd), cmd.exit_status) {
        (JobState::Running, _) => run::format_elapsed(cmd.elapsed_secs),
        (JobState::Failed, Some(code)) => format!("exit {code}"),
        _ => cmd
            .started
            .zip(cmd.ended)
            .map(|(started, ended)| run::format_runtime(ended.saturating_duration_since(started)))
            .unwrap_or_default(),
    }
}

/// The view of each Job of `ws`, oldest first, one for each id of the tray
/// that the store holds.
pub(crate) fn views(ws: &Workspace) -> Vec<JobView> {
    ws.jobs
        .iter()
        .filter_map(|id| ws.cmds.get(*id))
        .map(view_of)
        .collect()
}

/// The view of the Job of `cmd`.
fn view_of(cmd: &Cmd) -> JobView {
    JobView {
        state: state_of(cmd),
        name: cmd
            .command
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" "),
        progress: cmd.progress(),
        time: time_text(cmd),
    }
}

#[cfg(test)]
mod tests {
    use super::JobState;
    use crate::{
        action_handlers::{
            self,
            pane::{dispose_view, EditorDisposal},
        },
        app::Stoat,
        pane::View,
        run::{Cmd, CmdId, RunId},
        test_harness::{CmdHandle, TestHarness},
        theme::Theme,
    };
    use ratatui::style::{Color, Style};
    use std::{
        path::PathBuf,
        time::{Duration, Instant},
    };
    use stoat_action::{ClosePane, CloseWorkspace, SplitRight, Terminal};

    /// The request for the alternate screen that a full-screen program writes
    /// first.
    const FULL_SCREEN: &[u8] = b"\x1b[?1049h";

    #[test]
    fn ctrl_j_moves_the_newest_running_block_to_the_tray_and_its_stack_keeps_the_id() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");

        let ws = h.stoat.active_workspace();
        let cmd = &ws.cmds[job.cmd];
        assert_eq!(
            (
                ws.jobs.clone(),
                (cmd.in_jobs, cmd.finished, cmd.process.is_some()),
                ws.runs[focused_run(&h)].blocks.clone(),
                job.session.was_killed(),
            ),
            (vec![job.cmd], (true, false, true), vec![job.cmd], false),
        );
    }

    #[test]
    fn ctrl_j_with_no_running_block_returns_the_job_of_the_newest_stub() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        h.type_keys("ctrl-j");
        let returned = {
            let ws = h.stoat.active_workspace();
            (ws.jobs.clone(), ws.cmds[job.cmd].in_jobs)
        };

        job.session.push_output(b"hello\n");
        h.settle();
        let shown = h.stoat.active_workspace().cmds[job.cmd]
            .grid
            .text_in(0..5, 0..1);
        assert_eq!((returned, shown), ((Vec::new(), false), "hello".to_owned()));
    }

    #[test]
    fn ctrl_j_takes_the_newest_running_block_first() {
        let mut h = Stoat::test();
        h.open_run();
        let first = h.submit_run("sleep 30").cmd;
        let second = h.submit_run("sleep 31").cmd;

        let presses = [(); 3].map(|()| {
            h.type_keys("ctrl-j");
            h.stoat.active_workspace().jobs.clone()
        });
        assert_eq!(presses, [vec![second], vec![second, first], vec![first]]);
    }

    #[test]
    fn output_and_exit_reach_a_cmd_in_the_tray() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        job.session.push_output(b"hello\n");
        job.session.finish(0);
        h.settle();

        let ws = h.stoat.active_workspace();
        let cmd = &ws.cmds[job.cmd];
        assert_eq!(
            (
                cmd.grid.text_in(0..5, 0..1),
                (cmd.exit_status, cmd.finished, cmd.process.is_none()),
                ws.jobs.clone(),
            ),
            ("hello".to_owned(), (Some(0), true, true), vec![job.cmd]),
        );
    }

    #[test]
    fn closing_a_run_pane_keeps_the_jobs_it_pushed() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        let other = h.submit_run("sleep 31");
        end_focused_run(&mut h);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                ws.runs.len(),
                ws.cmds.keys().collect::<Vec<_>>(),
                ws.jobs.clone(),
                [&job, &other].map(|handle| handle.session.was_killed()),
            ),
            (0, vec![job.cmd], vec![job.cmd], [false, true]),
        );
    }

    /// The first Job outlives its Run pane and the second shows as a Stub of a
    /// stack, and the close of the workspace ends both.
    #[test]
    fn closing_a_workspace_kills_its_jobs() {
        let mut h = Stoat::test();
        let second = h.create_workspace();
        h.set_active_workspace(second);
        action_handlers::dispatch(&mut h.stoat, &SplitRight);
        let closed = h.push_job("sleep 30");
        action_handlers::dispatch(&mut h.stoat, &ClosePane);
        let shown = h.push_job("sleep 31");
        let killed = || [&closed, &shown].map(|job| job.session.was_killed());
        let before = killed();

        action_handlers::dispatch(&mut h.stoat, &CloseWorkspace);
        assert_eq!((before, killed()), ([false, false], [true, true]));
    }

    /// The Stub of the handoff stays in the stack that holds the Job, and a Job
    /// that no stack holds leaves the store.
    #[test]
    fn a_job_that_asks_for_a_full_terminal_leaves_the_tray() {
        let mut h = Stoat::test();
        let run = h.open_run();
        let job = h.push_job("mytui");
        job.session.push_output(FULL_SCREEN);
        h.settle();
        let ws = h.stoat.active_workspace();
        let stub = &ws.cmds[job.cmd];
        assert_eq!(
            (
                ws.jobs.clone(),
                (stub.in_jobs, stub.handoff.is_some()),
                ws.runs[run].blocks.clone(),
            ),
            (Vec::new(), (false, true), vec![job.cmd]),
        );

        let mut h = Stoat::test();
        let job = h.push_job("mytui");
        end_focused_run(&mut h);
        let stored = h.stoat.active_workspace().cmds.len();
        job.session.push_output(FULL_SCREEN);
        h.settle();
        let ws = h.stoat.active_workspace();
        assert_eq!((stored, ws.cmds.len(), ws.jobs.len()), (1, 0, 0));
    }

    /// A Run pane with no block and one whose only block ended each have
    /// nothing to move.
    #[test]
    fn ctrl_j_with_nothing_to_move_says_so() {
        let toggle = |end_a_block: bool| {
            let mut h = Stoat::test();
            h.open_run();
            if end_a_block {
                h.submit_run("true").session.finish(0);
                h.settle();
            }
            h.type_keys("ctrl-j");
            (
                h.stoat.pending_message.clone(),
                h.stoat.active_workspace().jobs.len(),
            )
        };

        let said = (Some("no running command".to_owned()), 0);
        assert_eq!([toggle(false), toggle(true)], [said.clone(), said]);
    }

    /// An ended Job stays in the tray until `ctrl-j` brings it back, as it
    /// brings back a running one.
    #[test]
    fn ctrl_j_returns_an_ended_job() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        job.session.finish(0);
        h.settle();
        let held = h.stoat.active_workspace().jobs.clone();
        h.type_keys("ctrl-j");

        let ws = h.stoat.active_workspace();
        let cmd = &ws.cmds[job.cmd];
        assert_eq!(
            (held, ws.jobs.clone(), cmd.in_jobs, cmd.finished),
            (vec![job.cmd], Vec::new(), false, true),
        );
    }

    /// Output of a Job repaints while no visible pane shows its stack, whether
    /// a terminal covers its Run pane or the pane closed. Output of a Cmd
    /// outside the tray under a covered Run pane repaints nothing.
    #[test]
    fn output_of_a_job_repaints_with_its_stack_off_screen() {
        let covered = |to_tray: bool| {
            let mut h = Stoat::test();
            h.open_run();
            let cmd = h.submit_run("sleep 30");
            if to_tray {
                h.type_keys("ctrl-j");
            }
            action_handlers::dispatch(&mut h.stoat, &Terminal);
            repaints(&mut h, &cmd)
        };
        let closed = {
            let mut h = Stoat::test();
            let job = h.push_job("sleep 30");
            end_focused_run(&mut h);
            repaints(&mut h, &job)
        };

        assert_eq!([covered(true), covered(false), closed], [true, false, true]);
    }

    /// A second push of one Cmd and a push of an id that the store does not
    /// hold each leave the tray as it was.
    #[test]
    fn a_push_adds_each_cmd_of_the_store_once() {
        let mut h = Stoat::test();
        h.open_run();
        let id = h.submit_run("sleep 30").cmd;

        let ws = h.stoat.active_workspace_mut();
        let pushes = [
            super::push(ws, id),
            super::push(ws, id),
            super::push(ws, CmdId::default()),
        ];
        assert_eq!((pushes, ws.jobs.clone()), ([true, false, false], vec![id]));
    }

    /// A Cmd runs, or ended ok, failed, or stopped, and each state leads with
    /// its mark. An exit of 0 is ok even after an interrupt, and an end with
    /// no exit status is a stop.
    #[test]
    fn a_cmd_takes_one_of_four_states() {
        let state = |finished: bool, exit_status: Option<i32>, stopped: bool| {
            let mut cmd = Cmd::new("sleep 30".to_owned(), PathBuf::from("/w"), 40);
            cmd.finished = finished;
            cmd.exit_status = exit_status;
            cmd.stopped = stopped;
            let state = super::state_of(&cmd);
            (state, state.mark())
        };

        assert_eq!(
            [
                state(false, None, false),
                state(true, Some(0), false),
                state(true, Some(101), false),
                state(true, Some(130), true),
                state(true, None, false),
                state(true, Some(0), true),
            ],
            [
                (JobState::Running, '\u{25cf}'),
                (JobState::Ok, '\u{2713}'),
                (JobState::Failed, '\u{2717}'),
                (JobState::Stopped, '\u{25a0}'),
                (JobState::Stopped, '\u{25a0}'),
                (JobState::Ok, '\u{2713}'),
            ],
        );
    }

    /// A state draws in the foreground of its `ui.job` scope alone, and a
    /// theme that leaves that scope unset draws it in the scope that the state
    /// falls back to.
    #[test]
    fn a_state_draws_in_its_job_scope_or_its_fallback() {
        let theme = |job_scopes: &str| {
            let text = format!(
                r##"theme t {{
                    ui.statusline.run.fg = "#000001";
                    ui.badge.complete.fg = "#000002";
                    ui.badge.error.fg = "#000003";
                    ui.text.muted.fg = "#000004";
                    {job_scopes}
                }}"##
            );
            let (config, _) = stoat_config::parse(&text);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let styles = |theme: &Theme| {
            [
                JobState::Running,
                JobState::Ok,
                JobState::Failed,
                JobState::Stopped,
            ]
            .map(|state| super::state_style(theme, state))
        };
        let fg = |blue: u8| Style::default().fg(Color::Rgb(0, 0, blue));

        let fallback = styles(&theme(""));
        let set = styles(&theme(
            r##"ui.job.running = { fg: "#000011", bg: "#0000ff" };
                ui.job.ok.fg = "#000012";
                ui.job.failed.fg = "#000013";
                ui.job.stopped.fg = "#000014";"##,
        ));
        assert_eq!(
            (fallback, set),
            (
                [fg(1), fg(2), fg(3), fg(4)],
                [fg(0x11), fg(0x12), fg(0x13), fg(0x14)],
            ),
        );
    }

    /// `ctrl-c` writes the interrupt to the newest running Cmd and marks it
    /// stopped, so its exit reads as a stop, and a run again clears the mark.
    #[test]
    fn ctrl_c_marks_the_cmd_it_interrupts_as_stopped() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("sleep 30");
        h.type_keys("ctrl-c");
        let interrupted = (
            handle.session.sent_bytes(),
            h.stoat.active_workspace().cmds[handle.cmd].stopped,
        );

        h.inject_cmd_done(handle.cmd, 130);
        let ended = super::state_of(&h.stoat.active_workspace().cmds[handle.cmd]);
        h.type_keys("ctrl-r");
        let cmd = &h.stoat.active_workspace().cmds[handle.cmd];
        assert_eq!(
            (interrupted, ended, (cmd.finished, cmd.stopped)),
            ((vec![vec![0x03]], true), JobState::Stopped, (false, false)),
        );
    }

    /// A running Job reads the seconds it has run, a failed one its exit
    /// code, and one that ended ok its runtime.
    #[test]
    fn the_time_text_follows_the_state() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        h.advance_clock(Duration::from_secs(41));
        h.snapshot();
        let time = |h: &TestHarness| super::time_text(&h.stoat.active_workspace().cmds[job.cmd]);
        let running = time(&h);
        h.inject_cmd_done(job.cmd, 101);
        let failed = time(&h);

        let mut ok = Cmd::new("true".to_owned(), PathBuf::from("/w"), 40);
        let started = Instant::now();
        ok.started = Some(started);
        ok.ended = Some(started + Duration::from_millis(1500));
        ok.finished = true;
        ok.exit_status = Some(0);
        assert_eq!(
            [running, failed, super::time_text(&ok)],
            ["0:41", "exit 101", "1.5s"],
        );
    }

    /// A Job names its Cmd by the first two words of the line, one space apart.
    #[test]
    fn a_job_view_names_the_first_two_words_of_its_line() {
        let name =
            |line: &str| super::view_of(&Cmd::new(line.to_owned(), PathBuf::from("/w"), 40)).name;

        assert_eq!(
            [name("cargo test -p stoat"), name("htop"), name("sleep  30")],
            ["cargo test", "htop", "sleep 30"],
        );
    }

    /// The run that the focused pane shows.
    fn focused_run(h: &TestHarness) -> RunId {
        let ws = h.stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            View::Run(run) => run,
            _ => panic!("the focused pane shows no run"),
        }
    }

    /// End the run that the focused pane shows, as a close of its pane ends
    /// it.
    fn end_focused_run(h: &mut TestHarness) {
        let executor = h.stoat.executor.clone();
        let run = focused_run(h);
        dispose_view(
            h.stoat.active_workspace_mut(),
            &executor,
            View::Run(run),
            EditorDisposal::Remove,
        );
    }

    /// Whether output of the Cmd of `handle` marks the frame dirty.
    fn repaints(h: &mut TestHarness, handle: &CmdHandle) -> bool {
        h.stoat.pty_dirty = false;
        handle.session.push_output(b"x");
        h.settle();
        h.stoat.pty_dirty
    }
}
