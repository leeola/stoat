use crate::{
    app::{Stoat, UpdateEffect},
    jobs,
    pane::View,
    run::CmdId,
};

/// Move the newest running block of the focused Run pane to the Job Tray, or
/// with none, bring the Job of the newest Stub of its stack back.
///
/// A Run pane has no block cursor, so the move takes the newest block that
/// runs and is not in Jobs. A pane with nothing to move either way sets the
/// status `no running command`.
pub(super) fn run_job_toggle(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let View::Run(run) = ws.panes.pane(ws.panes.focus()).view else {
        return UpdateEffect::None;
    };
    if jobs::push_from_run(ws, run) || jobs::return_to_run(ws, run) {
        return UpdateEffect::Redraw;
    }
    stoat.set_status("no running command");
    UpdateEffect::Redraw
}

/// Open the Glance of a Job, the newest one, or the Job before the one of the
/// last Glance while the tray still holds that one.
///
/// After the oldest Job the walk wraps to the newest. With no Job the status
/// says so, and the focused mode goes to normal as it does for a Glance, so
/// the key leaves no submenu open.
pub(super) fn job_glance_open(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace();
    let Some(&newest) = ws.jobs.last() else {
        stoat.set_focused_mode("normal".into());
        stoat.set_status("no jobs");
        return UpdateEffect::Redraw;
    };
    let glanced = ws
        .glanced
        .and_then(|glanced| ws.jobs.iter().position(|id| *id == glanced));
    let id = match glanced {
        Some(0) | None => newest,
        Some(at) => ws.jobs[at - 1],
    };
    jobs::show_glance(stoat, id);
    UpdateEffect::Redraw
}

/// Move the open Glance `delta` Jobs along the tray, toward the newer Jobs
/// for a positive `delta`, wrapping at either end.
pub(super) fn job_glance_step(stoat: &mut Stoat, delta: isize) -> UpdateEffect {
    let Some(glanced) = glanced_cmd(stoat) else {
        return UpdateEffect::None;
    };
    let tray = &stoat.active_workspace().jobs;
    let Some(at) = tray.iter().position(|id| *id == glanced) else {
        return UpdateEffect::None;
    };
    let id = tray[(at as isize + delta).rem_euclid(tray.len() as isize) as usize];
    jobs::show_glance(stoat, id);
    UpdateEffect::Redraw
}

pub(super) fn job_glance_close(stoat: &mut Stoat) -> UpdateEffect {
    stoat.job_glance = None;
    UpdateEffect::Redraw
}

/// Show the Job of the open Glance as a block of a Run pane, as
/// [`jobs::open`] does.
pub(super) fn job_open(stoat: &mut Stoat) -> UpdateEffect {
    match glanced_cmd(stoat) {
        Some(id) => jobs::open(stoat, id),
        None => UpdateEffect::None,
    }
}

/// Run the ended Job of the open Glance again in the tray. A Job that still
/// runs stays as it is, and the status says so.
pub(super) fn job_rerun(stoat: &mut Stoat) -> UpdateEffect {
    let Some(id) = glanced_cmd(stoat) else {
        return UpdateEffect::None;
    };
    let finished = stoat
        .active_workspace()
        .cmds
        .get(id)
        .is_some_and(|cmd| cmd.finished);
    if !finished {
        stoat.set_status("still running");
        return UpdateEffect::Redraw;
    }
    super::run::restart_cmd(stoat, id).merge(UpdateEffect::Redraw)
}

/// Interrupt the running Job of the open Glance and mark the run as one that
/// the user stopped, keeping the Glance. On an ended Job the key closes the
/// Glance, as `Ctrl-c` closes each modal.
pub(super) fn job_interrupt(stoat: &mut Stoat) -> UpdateEffect {
    let Some(id) = glanced_cmd(stoat) else {
        return UpdateEffect::None;
    };
    match stoat.active_workspace_mut().cmds.get_mut(id) {
        Some(cmd) if !cmd.finished => {
            if let Some(process) = &cmd.process {
                process.send_interrupt();
                cmd.stopped = true;
            }
        },
        _ => stoat.job_glance = None,
    }
    UpdateEffect::Redraw
}

/// Dismiss the ended Job of the open Glance, as [`jobs::dismiss`] does.
pub(super) fn job_dismiss(stoat: &mut Stoat) -> UpdateEffect {
    match glanced_cmd(stoat) {
        Some(id) => jobs::dismiss(stoat, id),
        None => UpdateEffect::None,
    }
}

/// The Cmd of the Job that the open Glance shows.
fn glanced_cmd(stoat: &Stoat) -> Option<CmdId> {
    stoat.job_glance.as_ref().map(|glance| glance.cmd)
}
