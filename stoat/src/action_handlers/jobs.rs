use crate::{
    app::{Stoat, UpdateEffect},
    jobs,
    pane::View,
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
