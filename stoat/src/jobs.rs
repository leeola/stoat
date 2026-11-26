//! The Job Tray of a workspace, the Cmds whose placement is the tray rather
//! than a Run pane's stack.
//!
//! A Job is a placement of one Cmd, which stays in [`Workspace::cmds`] with its
//! process, its grid, and its report, so its output and its exit reach it as
//! they reach any Cmd. [`Workspace::jobs`] lists the ids, and
//! [`Cmd::in_jobs`] marks the block of each one as a Stub of the stack that
//! holds its id. [`push`] and [`take`] are the only writers of the two.

use crate::{
    action_handlers,
    app::{Stoat, UpdateEffect},
    pane::View,
    run::{self, kind::RunKind, Cmd, CmdId, RunId},
    theme::{scope, Theme},
    workspace::Workspace,
};
use ratatui::style::Style;
use std::ops::Range;

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

    /// The word that names the state, such as `running`.
    pub(crate) fn word(self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Ok => "ok",
            JobState::Failed => "failed",
            JobState::Stopped => "stopped",
        }
    }
}

/// The Glance of a Job, a modal under the Job's Tray Chip that reads the Job
/// at a glance and takes the keys that act on it.
pub(crate) struct Glance {
    /// The Cmd of the Job that the Glance shows.
    pub(crate) cmd: CmdId,
}

/// Where the tab bar drew the Tray Chip of a Job, which a press on the bar and
/// the Glance under the chip read.
pub(crate) struct TraySpan {
    pub(crate) cmd: CmdId,
    /// The columns of the chip on the bar's row.
    pub(crate) cells: Range<u16>,
}

/// What a Job shows of its Cmd wherever the Job draws, such as its Tray Chip.
pub(crate) struct JobView {
    /// The Cmd of the Job.
    pub(crate) id: CmdId,
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

/// Put the Cmd `id` that a start just made in the tray of the active
/// workspace while a `Ctrl-Enter` submit runs, as [`Stoat::job_submit`] marks
/// it, and answer whether it went in.
///
/// A start that answers true gives the Cmd no other place, so no Run Block
/// shows it and no Run pane opens for it. A Cmd whose program needs a full
/// terminal stays out, so it takes the place that `Enter` gives it, and its
/// hand-off to a terminal tab reads that place.
pub(crate) fn claim_for_jobs(stoat: &mut Stoat, id: CmdId) -> bool {
    if !stoat.job_submit {
        return false;
    }
    let ws = stoat.active_workspace_mut();
    let full_terminal = ws
        .cmds
        .get(id)
        .is_some_and(|cmd| cmd.kind == RunKind::FullTerminal);
    !full_terminal && push(ws, id)
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
        .filter_map(|id| ws.cmds.get(*id).map(|cmd| view_of(*id, cmd)))
        .collect()
}

/// The view of the Job of `cmd`, whose id is `id`.
fn view_of(id: CmdId, cmd: &Cmd) -> JobView {
    JobView {
        id,
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

/// Open the Glance of the Job `id`, and remember the Job as the workspace's
/// last Glance.
///
/// The Glance takes no input, so the focused mode goes to normal, where its
/// keys bind, as the git status panel's do.
pub(crate) fn show_glance(stoat: &mut Stoat, id: CmdId) {
    stoat.set_focused_mode("normal".into());
    stoat.job_glance = Some(Glance { cmd: id });
    stoat.active_workspace_mut().glanced = Some(id);
}

/// Take the Job `id` out of the tray, show it as a block of a Run pane that
/// takes the focus, as [`show_block`] shows it, and close the Glance.
pub(crate) fn open(stoat: &mut Stoat, id: CmdId) -> UpdateEffect {
    stoat.job_glance = None;
    take(stoat.active_workspace_mut(), id);
    show_block(stoat, id);
    UpdateEffect::Redraw
}

/// Show the Cmd `id` as a block of a Run pane that takes the focus.
///
/// A Run pane on show whose stack holds the id shows the block in place,
/// scrolled so that the block's last row is the pane's last output row. A
/// stack that no pane shows, as one under a terminal that covers its pane,
/// drops the id, and the Cmd goes to the end of a Run pane's stack as a Cmd
/// with no stack does.
pub(crate) fn show_block(stoat: &mut Stoat, id: CmdId) {
    let ws = stoat.active_workspace_mut();
    let holder = ws
        .runs
        .iter()
        .find(|(_, run)| run.blocks.contains(&id))
        .map(|(run, _)| run);
    let shown = holder.and_then(|run| Some((run, ws.view_location(&View::Run(run))?)));
    match shown {
        Some((run, at)) => {
            scroll_to_block(ws, run, id);
            stoat.reveal_location(at);
        },
        None => {
            if let Some(run) = holder {
                ws.runs[run].blocks.retain(|block| *block != id);
            }
            action_handlers::run::append_to_run_pane(stoat, id, true);
        },
    }
}

/// Take the ended Job `id` out of the tray and close the Glance.
///
/// A stack that holds the id keeps the Cmd as an ended block, because the
/// stack of a Run pane is the record of that pane. With no such stack the
/// Cmd leaves the store. A Job that still runs stays, and the status says so.
pub(crate) fn dismiss(stoat: &mut Stoat, id: CmdId) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    if ws.cmds.get(id).is_some_and(|cmd| !cmd.finished) {
        stoat.set_status("still running");
        return UpdateEffect::Redraw;
    }
    take(ws, id);
    if !ws.runs.values().any(|run| run.blocks.contains(&id)) {
        ws.cmds.remove(id);
    }
    stoat.job_glance = None;
    UpdateEffect::Redraw
}

/// Close the Glance when the tray of the active workspace does not hold its
/// Job, as after the Job comes back to its block or its workspace leaves the
/// screen.
pub(crate) fn sync_glance(stoat: &mut Stoat) {
    let Some(glance) = &stoat.job_glance else {
        return;
    };
    if !stoat.active_workspace().jobs.contains(&glance.cmd) {
        stoat.job_glance = None;
    }
}

/// Scroll the stack of the run `run` so that the last row of the block `id` is
/// its last output row, which is no scroll for the newest block.
fn scroll_to_block(ws: &mut Workspace, run: RunId, id: CmdId) {
    let Some(run_state) = ws.runs.get(run) else {
        return;
    };
    let offset = match run_state.blocks.last() == Some(&id) {
        true => 0,
        false => {
            let through: usize = run_state
                .blocks
                .iter()
                .take_while(|block| **block != id)
                .chain(std::iter::once(&id))
                .filter_map(|block| ws.cmds.get(*block))
                .map(Cmd::rendered_line_span)
                .sum();
            run_state
                .output_line_total(&ws.cmds)
                .saturating_sub(through)
        },
    };
    if let Some(run_state) = ws.runs.get_mut(run) {
        run_state.scroll_offset = offset;
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
        app::{Stoat, UpdateEffect},
        host::FakeTerminalSession,
        keymap_state,
        pane::View,
        run::{Cmd, CmdId, RunId},
        test_harness::{CmdHandle, TestHarness},
        theme::Theme,
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::{Color, Style};
    use std::{
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };
    use stoat_action::{ClosePane, CloseWorkspace, NewTab, RunSubmitJob, SplitRight, Terminal};

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
        let name = |line: &str| {
            super::view_of(
                CmdId::default(),
                &Cmd::new(line.to_owned(), PathBuf::from("/w"), 40),
            )
            .name
        };

        assert_eq!(
            [name("cargo test -p stoat"), name("htop"), name("sleep  30")],
            ["cargo test", "htop", "sleep 30"],
        );
    }

    /// A running Job ticks once a second with its Run pane parked in another
    /// tab, because the Job Tray shows its time.
    #[test]
    fn a_running_job_ticks_once_a_second_with_no_stub_on_show() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        action_handlers::dispatch(&mut h.stoat, &NewTab);
        let read = |h: &mut TestHarness| {
            h.snapshot();
            let time = super::time_text(&h.stoat.active_workspace().cmds[job.cmd]);
            (h.stoat.cmd_tick.is_some(), time)
        };

        let mut reads = vec![read(&mut h)];
        for _ in 0..3 {
            h.advance_clock(Duration::from_secs(1));
            reads.push(read(&mut h));
        }
        assert_eq!(
            reads,
            ["0:00", "0:01", "0:02", "0:03"].map(|time| (true, time.to_owned())),
        );
    }

    /// The tick that a Job with no Stub on show keeps stops once the Job ends.
    #[test]
    fn no_tick_is_armed_after_the_last_job_ends() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        action_handlers::dispatch(&mut h.stoat, &NewTab);
        h.snapshot();
        let running = h.stoat.cmd_tick.is_some();
        h.inject_cmd_done(job.cmd, 0);
        h.snapshot();

        assert_eq!([running, h.stoat.cmd_tick.is_some()], [true, false]);
    }

    /// A Job of a workspace that is not on screen arms no tick, and its time
    /// catches up on the first frame that its workspace shows.
    #[test]
    fn a_job_of_a_workspace_that_is_not_active_arms_no_tick() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        let first = h.stoat.active_workspace;
        let second = h.create_workspace();
        h.set_active_workspace(second);
        h.snapshot();
        let background = h.stoat.cmd_tick.is_some();

        h.advance_clock(Duration::from_secs(5));
        h.set_active_workspace(first);
        h.snapshot();
        let time = super::time_text(&h.stoat.active_workspace().cmds[job.cmd]);
        assert_eq!(
            (background, h.stoat.cmd_tick.is_some(), time),
            (false, true, "0:05".to_owned()),
        );
    }

    /// `Space J k` opens the Glance on the newest Job, a modal with no input
    /// whose keys bind in normal mode.
    #[test]
    fn space_j_k_opens_the_glance_on_the_newest_job() {
        let mut h = Stoat::test();
        h.push_job("sleep 30");
        let newest = h.push_job("sleep 31");
        h.type_keys("escape space J k");

        assert_eq!(
            (
                glanced(&h),
                keymap_state::modal_predicate(&h.stoat),
                h.stoat.focused_mode().to_string(),
            ),
            (Some(newest.cmd), Some("job_glance"), "normal".to_owned()),
        );
    }

    /// After a Glance, `Space J k` opens the Job before the last one glanced
    /// at, and after the oldest Job the newest.
    #[test]
    fn space_j_k_steps_to_the_older_job_after_a_glance() {
        let mut h = Stoat::test();
        let first = h.push_job("sleep 30");
        let second = h.push_job("sleep 31");
        h.type_keys("escape");

        let mut named = Vec::new();
        for _ in 0..3 {
            h.type_keys("space J k");
            named.push(glanced(&h));
            h.type_keys("escape");
        }
        assert_eq!(named, [second.cmd, first.cmd, second.cmd].map(Some));
    }

    /// `h` steps the Glance to the older Job and `l` to the newer, and both
    /// wrap at the ends of the tray.
    #[test]
    fn h_and_l_walk_the_jobs_and_wrap() {
        let mut h = Stoat::test();
        let ids = ["sleep 30", "sleep 31", "sleep 32"].map(|line| h.push_job(line).cmd);
        h.type_keys("escape space J k");

        let mut walk = vec![glanced(&h)];
        for key in ["h", "h", "h", "l"] {
            h.type_keys(key);
            walk.push(glanced(&h));
        }
        assert_eq!(walk, [ids[2], ids[1], ids[0], ids[2], ids[0]].map(Some));
    }

    /// `Enter` takes the Job out of the tray, back to its block in the Run pane
    /// that shows its Stub, and that pane takes the focus from the file beside
    /// it.
    #[test]
    fn enter_returns_a_job_to_its_stub_and_focuses_the_run_pane() {
        let mut h = Stoat::test();
        let editor = h.stoat.active_workspace().panes.focus();
        action_handlers::dispatch(&mut h.stoat, &SplitRight);
        let job = h.push_job("sleep 30");
        let (run_pane, run) = (h.stoat.active_workspace().panes.focus(), focused_run(&h));
        h.stoat.active_workspace_mut().panes.set_focus(editor);
        h.type_keys("space J k");
        h.type_keys("enter");

        let ws = h.stoat.active_workspace();
        let cmd = &ws.cmds[job.cmd];
        assert_eq!(
            (
                ws.jobs.clone(),
                (cmd.in_jobs, cmd.finished),
                ws.runs[run].blocks.clone(),
                ws.panes.focus() == run_pane,
                h.stoat.job_glance.is_none(),
            ),
            (Vec::new(), (false, false), vec![job.cmd], true, true),
        );
    }

    /// `Enter` on a Job whose Run pane closed puts the Cmd at the end of the
    /// stack of a Run pane that opens for it.
    #[test]
    fn enter_appends_a_job_with_no_stub_to_a_run_pane() {
        let mut h = Stoat::test();
        action_handlers::dispatch(&mut h.stoat, &SplitRight);
        let job = h.push_job("sleep 30");
        action_handlers::dispatch(&mut h.stoat, &ClosePane);
        h.type_keys("space J k");
        h.type_keys("enter");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                ws.jobs.clone(),
                ws.runs[focused_run(&h)].blocks.last().copied(),
            ),
            (Vec::new(), Some(job.cmd)),
        );
    }

    /// `Ctrl-r` runs an ended Job again in the tray, and leaves a running one
    /// as it is.
    #[test]
    fn ctrl_r_runs_the_glanced_job_again_in_the_tray() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        h.type_keys("escape space J k");
        let spawned = |h: &TestHarness| {
            h.fake_terminal_host()
                .spawns()
                .iter()
                .filter(|spawn| spawn.args.last().is_some_and(|line| line == "sleep 30"))
                .count()
        };
        h.type_keys("ctrl-r");
        let running = (spawned(&h), h.stoat.pending_message.clone());

        h.inject_cmd_done(job.cmd, 0);
        h.type_keys("ctrl-r");
        let ws = h.stoat.active_workspace();
        let cmd = &ws.cmds[job.cmd];
        assert_eq!(
            (
                running,
                (cmd.finished, cmd.run_count),
                spawned(&h),
                ws.jobs.clone()
            ),
            (
                (1, Some("still running".to_owned())),
                (false, 2),
                2,
                vec![job.cmd]
            ),
        );
    }

    /// `Ctrl-c` interrupts a running Job and keeps the Glance, and on an ended
    /// Job it closes the Glance with no quit.
    #[test]
    fn ctrl_c_interrupts_a_running_job_and_closes_the_glance_of_an_ended_one() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        h.type_keys("escape space J k");
        h.type_keys("ctrl-c");
        let interrupted = (
            job.session.sent_bytes(),
            h.stoat.active_workspace().cmds[job.cmd].stopped,
            h.stoat.job_glance.is_some(),
        );

        h.inject_cmd_done(job.cmd, 130);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let quit = matches!(h.stoat.update(Event::Key(ctrl_c)), UpdateEffect::Quit);
        assert_eq!(
            (interrupted, quit, h.stoat.job_glance.is_none()),
            ((vec![vec![0x03]], true, true), false, true),
        );
    }

    /// `x` leaves a running Job in the tray, and dismisses an ended Job, which
    /// leaves the store too when no stack holds it.
    #[test]
    fn x_dismisses_an_ended_job_and_keeps_a_running_one() {
        let mut h = Stoat::test();
        action_handlers::dispatch(&mut h.stoat, &SplitRight);
        let job = h.push_job("sleep 30");
        action_handlers::dispatch(&mut h.stoat, &ClosePane);
        h.type_keys("space J k");
        h.type_keys("x");
        let running = (
            h.stoat.pending_message.clone(),
            h.stoat.active_workspace().jobs.clone(),
        );

        h.inject_cmd_done(job.cmd, 0);
        h.type_keys("x");
        let ws = h.stoat.active_workspace();
        assert_eq!(
            (running, ws.jobs.len(), ws.cmds.len()),
            ((Some("still running".to_owned()), vec![job.cmd]), 0, 0),
        );
    }

    /// A dismissed Job whose Run pane shows its Stub stays in that stack as an
    /// ended block with its output, because the stack is the pane's record.
    #[test]
    fn a_dismissed_job_with_a_stub_stays_in_its_run_pane() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        let run = focused_run(&h);
        h.inject_cmd_output(job.cmd, b"hello\n");
        h.inject_cmd_done(job.cmd, 0);
        h.type_keys("escape space J k");
        h.type_keys("x");

        let ws = h.stoat.active_workspace();
        let cmd = &ws.cmds[job.cmd];
        assert_eq!(
            (
                ws.runs[run].blocks.clone(),
                cmd.in_jobs,
                cmd.grid.text_in(0..5, 0..1),
                ws.jobs.clone(),
            ),
            (vec![job.cmd], false, "hello".to_owned(), Vec::new()),
        );
    }

    /// The Glance closes once its Job leaves the tray.
    #[test]
    fn the_glance_closes_when_its_job_leaves_the_tray() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        h.type_keys("escape space J k");
        let open = h.stoat.job_glance.is_some();
        super::take(h.stoat.active_workspace_mut(), job.cmd);
        h.settle();

        assert_eq!((open, h.stoat.job_glance.is_none()), (true, true));
    }

    /// `Enter` at the empty prompt of a Run pane opens the Glance of a newest
    /// block in Jobs, which its hint row names.
    #[test]
    fn enter_at_an_empty_prompt_opens_the_glance_of_a_stub() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        let run = focused_run(&h);
        h.type_keys("enter");

        let ws = h.stoat.active_workspace();
        let hint = ws.runs[run].hint(&ws.cmds, &ws.terms).map(|row| row.text());
        assert_eq!(
            (glanced(&h), hint),
            (
                Some(job.cmd),
                Some("Enter go to the Job \u{b7} C-j bring it back here".to_owned()),
            ),
        );
    }

    /// `Space J k` with no Job says so and leaves the submenu for normal mode.
    #[test]
    fn space_j_k_with_no_job_says_so() {
        let mut h = Stoat::test();
        h.type_keys("space J k");

        assert_eq!(
            (
                h.stoat.job_glance.is_none(),
                h.stoat.pending_message.clone(),
                h.stoat.focused_mode().to_string(),
            ),
            (true, Some("no jobs".to_owned()), "normal".to_owned()),
        );
    }

    /// Opening a Job whose Run pane a terminal covers drops its id from that
    /// stack and puts the Cmd at the end of a Run pane on show.
    #[test]
    fn open_moves_a_job_out_of_a_covered_stack() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        let covered = focused_run(&h);
        action_handlers::dispatch(&mut h.stoat, &Terminal);
        super::open(&mut h.stoat, job.cmd);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                ws.runs[covered].blocks.clone(),
                ws.runs[focused_run(&h)].blocks.last().copied(),
                ws.jobs.len(),
            ),
            (Vec::new(), Some(job.cmd), 0),
        );
    }

    /// Opening a Job under a newer block scrolls its stack so that the Job's
    /// block ends on the pane's last output row.
    #[test]
    fn open_scrolls_an_older_block_to_the_bottom_of_its_pane() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        h.submit_run("echo a");
        let run = focused_run(&h);
        super::open(&mut h.stoat, job.cmd);

        assert_eq!(h.stoat.active_workspace().runs[run].scroll_offset, 1);
    }

    /// `Ctrl-:` over the Glance swaps it for the palette, so one modal owns the
    /// keys.
    #[test]
    fn the_palette_key_swaps_the_glance_for_the_palette() {
        let mut h = Stoat::test();
        h.push_job("sleep 30");
        h.type_keys("escape space J k");
        h.type_keys("ctrl-:");

        assert_eq!(
            (
                h.stoat.job_glance.is_none(),
                h.stoat.command_palette.is_some()
            ),
            (true, true),
        );
    }

    /// The Job of the open Glance.
    fn glanced(h: &TestHarness) -> Option<CmdId> {
        h.stoat.job_glance.as_ref().map(|glance| glance.cmd)
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

    /// Open a Run pane and type `line` at its prompt, with a fresh fake session
    /// queued for the process that a submit starts, and answer the pane's run.
    fn typed_at_prompt(h: &mut TestHarness, line: &str) -> RunId {
        let run = h.open_run();
        h.fake_terminal_host()
            .push_session(Arc::new(FakeTerminalSession::new()));
        h.type_text(line);
        run
    }

    /// `Ctrl-Enter` at the Run pane prompt runs the line as `Enter` does and
    /// sends its block to the Job Tray, so the stack keeps a Stub of it and the
    /// history keeps the line.
    #[test]
    fn ctrl_enter_at_the_run_prompt_starts_the_cmd_as_a_job_and_keeps_a_stub() {
        let mut h = Stoat::test();
        let run = typed_at_prompt(&mut h, "sleep 30");
        h.type_keys("ctrl-enter");

        let ws = h.stoat.active_workspace();
        let jobs: Vec<(&str, bool)> = ws
            .jobs
            .iter()
            .map(|id| (ws.cmds[*id].command.as_str(), ws.cmds[*id].finished))
            .collect();
        assert_eq!(
            (
                jobs,
                ws.runs[run].blocks == ws.jobs,
                ws.palette_history.entries().last().map(String::as_str),
            ),
            (vec![("sleep 30", false)], true, Some("r sleep 30")),
        );
    }

    /// A line that starts no Cmd pushes nothing, not even the running block
    /// that the stack held before it.
    #[test]
    fn ctrl_enter_on_a_line_that_starts_no_cmd_pushes_nothing() {
        let mut h = Stoat::test();
        let run = h.open_run();
        let running = h.submit_run("sleep 30").cmd;
        h.type_text("pwd");
        h.type_keys("ctrl-enter");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (ws.jobs.clone(), ws.runs[run].blocks.clone()),
            (Vec::new(), vec![running])
        );
    }

    /// A program that needs a full terminal never enters the tray, and it
    /// hands off as on `Enter`, its Stub on the stack.
    #[test]
    fn ctrl_enter_on_a_listed_program_at_the_run_prompt_hands_off() {
        let mut h = Stoat::test();
        let run = typed_at_prompt(&mut h, "htop");
        action_handlers::dispatch(&mut h.stoat, &RunSubmitJob);
        let at_submit = h.stoat.active_workspace().jobs.clone();
        h.stoat.drive_background();

        let ws = h.stoat.active_workspace();
        let stubs: Vec<bool> = ws.runs[run]
            .blocks
            .iter()
            .map(|id| ws.cmds[*id].handoff.is_some())
            .collect();
        assert_eq!(
            (at_submit, ws.jobs.clone(), stubs),
            (Vec::new(), Vec::new(), vec![true])
        );
    }
}
