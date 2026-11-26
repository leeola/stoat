use crate::{
    app::{Stoat, UpdateEffect},
    input_view::{InputView, SubmitTarget},
    job_list::{JobList, JobPlace},
    jobs,
    pane::View,
    run::{kind::RunKind, CmdId},
    workspace::Workspace,
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

/// Submit the line at the focused Run pane's prompt as
/// [`super::run::run_submit`] does, and send the block that the submit adds
/// to the Job Tray while it runs, so the pane keeps a Stub of it and the
/// history keeps the line.
///
/// A line that adds no block, a block that ended at once, and a block whose
/// program needs a full terminal act as on `Enter`.
pub(super) fn run_submit_job(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace();
    let View::Run(run) = ws.panes.pane(ws.panes.focus()).view else {
        return UpdateEffect::None;
    };
    let newest = |ws: &Workspace| {
        ws.runs
            .get(run)
            .and_then(|state| state.blocks.last().copied())
    };
    let before = newest(ws);
    let effect = super::run::run_submit(stoat);

    // A stack past its row budget drops an old block as a new one joins, so
    // the newest id tells a new block apart where the length would not.
    let ws = stoat.active_workspace_mut();
    if let Some(id) = newest(ws).filter(|id| Some(*id) != before)
        && ws
            .cmds
            .get(id)
            .is_some_and(|cmd| !cmd.finished && cmd.kind != RunKind::FullTerminal)
    {
        jobs::push(ws, id);
    }
    effect
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

/// Run the ended Cmd that a Job key acts on again in its place, a Job in the
/// tray and a block in its Run pane. A Cmd that still runs stays as it is,
/// and the status says so.
///
/// The Cmd is the selected row's while the Job List is open, and else the
/// Glance's, as [`acted_cmd`] picks it.
pub(super) fn job_rerun(stoat: &mut Stoat) -> UpdateEffect {
    let Some(id) = acted_cmd(stoat) else {
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

/// Interrupt the running Cmd that a Job key acts on and mark the run as one
/// that the user stopped, keeping the Glance or the Job List open. On an
/// ended Cmd the key closes the modal, as `Ctrl-c` closes each modal.
pub(super) fn job_interrupt(stoat: &mut Stoat) -> UpdateEffect {
    let Some(id) = acted_cmd(stoat) else {
        return UpdateEffect::None;
    };
    let running = stoat
        .active_workspace_mut()
        .cmds
        .get_mut(id)
        .filter(|cmd| !cmd.finished);
    if let Some(cmd) = running {
        if let Some(process) = &cmd.process {
            process.send_interrupt();
            cmd.stopped = true;
        }
        return UpdateEffect::Redraw;
    }
    match stoat.job_list.is_some() {
        true => job_list_close(stoat),
        false => job_glance_close(stoat),
    }
}

/// Dismiss the ended Job that a Job key acts on, as [`jobs::dismiss`] does.
///
/// A row of the Job List in a Run pane stays, and the status says
/// `not in the tray`, because the stack of a Run pane is the record of that
/// pane. The list stays open after a dismiss.
pub(super) fn job_dismiss(stoat: &mut Stoat) -> UpdateEffect {
    let row = stoat
        .job_list
        .as_ref()
        .map(|list| list.selected_entry().map(|entry| (entry.cmd, entry.place)));
    match row {
        Some(Some((_, JobPlace::RunPane(_)))) => {
            stoat.set_status("not in the tray");
            UpdateEffect::Redraw
        },
        Some(Some((id, JobPlace::Tray))) => jobs::dismiss(stoat, id),
        Some(None) => UpdateEffect::None,
        None => match glanced_cmd(stoat) {
            Some(id) => jobs::dismiss(stoat, id),
            None => UpdateEffect::None,
        },
    }
}

/// Open the Job List over the rows of the active workspace, as
/// [`JobList::sync`] lists them.
///
/// The focused mode drops to normal first, because the list's input takes
/// insert, and a submenu such as `Space J` that opened the list must not stay
/// on the view underneath once the list closes. With the list open the key
/// does nothing.
pub(super) fn open_job_list(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.job_list.is_some() {
        return UpdateEffect::None;
    }
    stoat.set_focused_mode("normal".into());
    let input = {
        let executor = stoat.executor.clone();
        InputView::create(
            stoat.active_workspace_mut(),
            executor,
            SubmitTarget::JobList,
            "",
            "insert",
        )
    };
    let mut list = JobList::new(input);
    list.sync(stoat.active_workspace(), "");
    stoat.job_list = Some(list);
    UpdateEffect::Redraw
}

/// Close the Job List and show the Cmd of its selected row.
///
/// A row of the tray opens its Job as [`jobs::open`] does. A row of a Run pane
/// shows its block in its pane as [`jobs::show_block`] shows it. An empty list
/// only closes.
pub(super) fn job_list_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(list) = stoat.job_list.take() else {
        return UpdateEffect::None;
    };
    let row = list.selected_entry().map(|entry| (entry.cmd, entry.place));
    list.dispose(stoat.active_workspace_mut());
    match row {
        Some((id, JobPlace::Tray)) => jobs::open(stoat, id),
        Some((id, JobPlace::RunPane(_))) => {
            jobs::show_block(stoat, id);
            UpdateEffect::Redraw
        },
        None => UpdateEffect::Redraw,
    }
}

pub(super) fn job_list_close(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(list) = stoat.job_list.take() {
        list.dispose(stoat.active_workspace_mut());
    }
    UpdateEffect::Redraw
}

pub(super) fn job_list_scope_toggle(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(list) = stoat.job_list.as_mut() {
        list.toggle_scope();
    }
    UpdateEffect::Redraw
}

/// Rank the open Job List against its input text and the store on the idle
/// path, so typing narrows the rows and a Cmd that starts, ends, or leaves
/// its place shows in them.
pub(crate) fn sync_job_list(stoat: &mut Stoat) {
    let Some(list) = stoat.job_list.as_mut() else {
        return;
    };
    let ws = &stoat.workspaces[stoat.active_workspace];
    let query = list.input.text(ws);
    list.sync(ws, &query);
}

/// The Cmd that a Job key acts on, the selected row's while the Job List is
/// open, and else the Glance's.
fn acted_cmd(stoat: &Stoat) -> Option<CmdId> {
    match &stoat.job_list {
        Some(list) => list.selected_entry().map(|entry| entry.cmd),
        None => glanced_cmd(stoat),
    }
}

/// The Cmd of the Job that the open Glance shows.
fn glanced_cmd(stoat: &Stoat) -> Option<CmdId> {
    stoat.job_glance.as_ref().map(|glance| glance.cmd)
}
