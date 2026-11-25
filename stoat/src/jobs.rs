//! The Job Tray of a workspace, the Cmds whose placement is the tray rather
//! than a Run pane's stack.
//!
//! A Job is a placement of one Cmd, which stays in [`Workspace::cmds`] with its
//! process, its grid, and its report, so its output and its exit reach it as
//! they reach any Cmd. [`Workspace::jobs`] lists the ids, and
//! [`Cmd::in_jobs`](crate::run::Cmd::in_jobs) marks the block of each one as a
//! Stub of the stack that holds its id. [`push`] and [`take`] are the only
//! writers of the two.

use crate::{
    run::{CmdId, RunId},
    workspace::Workspace,
};

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

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::{
            self,
            pane::{dispose_view, EditorDisposal},
        },
        app::Stoat,
        pane::View,
        run::{CmdId, RunId},
        test_harness::{CmdHandle, TestHarness},
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
