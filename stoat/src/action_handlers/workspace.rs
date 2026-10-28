use crate::{
    action_handlers::pane::EditorDisposal,
    app::{Stoat, UpdateEffect},
    host::FsHost,
    input_view::{InputView, SubmitTarget},
    pane::View,
    workspace::{
        persist,
        registry::{self, RegistryEntry},
        state_path_for, Workspace, WorkspaceId, WorkspaceUid,
    },
    workspace_picker::WorkspacePicker,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use time::OffsetDateTime;
use tracing::field;

pub(super) fn new_workspace(stoat: &mut Stoat) -> UpdateEffect {
    let parent = Some(stoat.active_workspace().family_root());
    create_workspace(stoat, parent);
    UpdateEffect::Redraw
}

/// Create a workspace rooted where the active one is, tied to the family of
/// `parent`, save the active workspace, and switch to the new one.
///
/// `None` makes a root session, in no family.
fn create_workspace(stoat: &mut Stoat, parent: Option<WorkspaceUid>) -> WorkspaceId {
    let git_root = stoat.active_workspace().git_root.clone();
    stoat.save_workspace(stoat.active_workspace);

    let mut ws = Workspace::new(git_root, &stoat.executor, stoat.redraw_notify.clone());
    ws.parent = parent;
    ws.layout(stoat.size());
    let id = stoat.workspaces.insert(ws);
    stoat.workspaces[id].id = id;
    log_created(&stoat.workspaces[id]);
    switch_active_workspace(stoat, id);
    id
}

pub(super) fn copy_workspace(stoat: &mut Stoat) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();
    let mut state = stoat.active_workspace().to_state();
    // The copy must not inherit any in-flight rebase pointer from its
    // source; it ties to half-applied git state that belongs to the
    // original workspace.
    state.rebase = None;
    state.rebase_active = None;
    state.uid = WorkspaceUid::now(&stoat.executor);
    state.parent = Some(stoat.active_workspace().family_root());

    stoat.save_workspace(stoat.active_workspace);

    // The fresh workspace carries the copy's identity from the start, as a
    // picker activation does, so it shows the copy's name before the replay
    // lands.
    let mut ws = Workspace::new(git_root, &stoat.executor, stoat.redraw_notify.clone());
    ws.uid = state.uid;
    ws.name = state.name.clone();
    ws.parent = state.parent;
    ws.layout(stoat.size());
    let id = stoat.workspaces.insert(ws);
    stoat.workspaces[id].id = id;
    log_created(&stoat.workspaces[id]);
    switch_active_workspace(stoat, id);

    // The buffers replay on the pool. The install respawns the terminal and
    // commits panes, which arrive with the source's ids.
    stoat.spawn_state_restore(id, state);
    UpdateEffect::Redraw
}

fn log_created(ws: &Workspace) {
    tracing::info!(
        target: "stoat::workspace",
        uid = %ws.uid,
        root = %ws.git_root.display(),
        parent = ws.parent.map(field::display),
        "created workspace"
    );
}

pub(super) fn close_workspace(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.workspaces.len() <= 1 {
        // FIXME: surface a user-visible error once we have a status surface
        // for non-badge errors; tracing-only feedback is invisible in the TUI.
        tracing::warn!(target: "stoat::workspace", "refusing to close last workspace");
        return UpdateEffect::None;
    }

    let active_id = stoat.active_workspace;
    tracing::info!(
        target: "stoat::workspace",
        uid = %stoat.workspaces[active_id].uid,
        "closed workspace"
    );
    remove_open_session(stoat, active_id);

    let replacement: WorkspaceId = stoat
        .workspaces
        .keys()
        .find(|k| *k != active_id)
        .expect("non-last workspace has at least one sibling");

    crate::lsp::session::release_documents(stoat);
    release_workspace(stoat, active_id);
    stoat.workspaces.remove(active_id);
    switch_active_workspace(stoat, replacement);
    UpdateEffect::Redraw
}

/// Shared tail of every workspace-switch action. Points [`Stoat::active_workspace`]
/// at `next` and re-layouts the new active workspace to the current terminal size
/// so the first render after the switch shows correctly-sized panes, and watches
/// its root, so a workspace entered after launch hears the writes under it.
///
/// The entered workspace's `last_active` takes the switch's instant, which is
/// what orders the workspace picker's rows by recency.
///
/// The language servers follow the switch. They close the documents of the
/// workspace left and open the entered one's, through
/// [`crate::lsp::session::mirror_workspace`].
pub(crate) fn switch_active_workspace(stoat: &mut Stoat, next: WorkspaceId) {
    let from = stoat.active_workspace;
    // A close removes the workspace it leaves before it switches away.
    let from_uid = stoat.workspaces.get(from).map(|ws| ws.uid);
    stoat.active_workspace = next;
    stoat.active_workspace_mut().last_active = stoat.executor.system_now();
    let size = stoat.size();
    stoat.active_workspace_mut().layout(size);
    stoat.watch_active_root();
    if from != next {
        let ws = stoat.active_workspace();
        tracing::info!(
            target: "stoat::workspace",
            from = from_uid.map(field::display),
            to = %ws.uid,
            name = %ws.display_name(),
            root = %ws.git_root.display(),
            "switched workspace"
        );
    }

    if stoat.active_workspace().remote.is_some() {
        stoat.remote_pending = true;
        crate::ssh::reconnect_when_ready(stoat);
    }
    crate::lsp::session::mirror_workspace(stoat, next);
}

/// Page the workspace picker's selection by half its visible rows in `dir`.
pub(super) fn workspace_picker_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    if let Some(picker) = stoat.workspace_picker.as_mut() {
        picker.page(dir);
    }
    UpdateEffect::Redraw
}

/// Toggle the workspace picker between every session and the active session's
/// family.
pub(super) fn workspace_picker_scope_toggle(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(picker) = stoat.workspace_picker.as_mut() {
        picker.toggle_scope();
    }
    UpdateEffect::Redraw
}

pub(super) fn workspace_picker_complete(stoat: &mut Stoat) -> UpdateEffect {
    let active_idx = stoat.active_workspace;

    let Some(basename) = stoat
        .workspace_picker
        .as_mut()
        .and_then(|picker| picker.complete_selected())
    else {
        return UpdateEffect::None;
    };

    let ws = &mut stoat.workspaces[active_idx];
    if let Some(picker) = stoat.workspace_picker.as_ref() {
        picker.input.replace_text(ws, &basename);
    }
    UpdateEffect::Redraw
}

/// Turn the workspace picker's filter into a rename of the selected session.
///
/// The input takes the row's name, and the filter query waits in the picker
/// until Enter applies the name or Escape cancels. An empty list does nothing.
pub(super) fn workspace_picker_rename_start(stoat: &mut Stoat) -> UpdateEffect {
    let active_idx = stoat.active_workspace;

    let Some(name) = stoat
        .workspace_picker
        .as_mut()
        .and_then(WorkspacePicker::rename_start)
    else {
        return UpdateEffect::None;
    };

    let ws = &mut stoat.workspaces[active_idx];
    if let Some(picker) = stoat.workspace_picker.as_ref() {
        picker.input.replace_text(ws, &name);
    }
    UpdateEffect::Redraw
}

/// Apply the rename field's name to the session under rename, and return the
/// picker to its filter.
///
/// An open row's workspace takes the name at once and saves. A saved row
/// rewrites its state file and meta sidecar on the blocking pool, and logs a
/// failed write. An empty name cancels the rename. The picker stays open.
fn workspace_picker_rename_finish(stoat: &mut Stoat) -> UpdateEffect {
    let active_idx = stoat.active_workspace;
    let Some(picker) = stoat.workspace_picker.as_mut() else {
        return UpdateEffect::None;
    };
    let name = picker
        .input
        .text(&stoat.workspaces[active_idx])
        .trim()
        .to_owned();
    let renamed = match name.is_empty() {
        true => {
            picker.rename_cancel();
            None
        },
        false => picker.rename_finish(&name),
    };
    restore_picker_filter(stoat);

    let Some((id, state_path, old)) = renamed else {
        return UpdateEffect::Redraw;
    };
    match (id, state_path) {
        (Some(id), _) => {
            stoat.workspaces[id].name = name.clone();
            stoat.save_workspace(id);
        },
        (None, Some(state_path)) => {
            let fs = stoat.fs_host.clone();
            let name = name.clone();
            stoat
                .executor
                .spawn_blocking(move || {
                    if let Err(err) = persist::rename_saved_session(&state_path, &name, fs.as_ref())
                    {
                        tracing::error!(
                            target: "stoat::workspace",
                            ?state_path,
                            ?err,
                            "failed to rename saved session"
                        );
                    }
                })
                .detach();
        },
        (None, None) => {},
    }
    tracing::info!(target: "stoat::workspace", old = %old, name = %name, "renamed workspace");
    stoat.set_status(format!("renamed session {old} to {name}"));
    UpdateEffect::Redraw
}

/// Put the filter query that a rename replaced back into the workspace picker's
/// input, and rank the list against it.
fn restore_picker_filter(stoat: &mut Stoat) {
    let active_idx = stoat.active_workspace;
    let Some(picker) = stoat.workspace_picker.as_mut() else {
        return;
    };
    let query = picker.filter_query.clone();
    picker
        .input
        .replace_text(&mut stoat.workspaces[active_idx], &query);
    picker.refilter(&query);
}

/// Delete the session under the workspace picker's selection, with its
/// sub-sessions.
///
/// An inactive row loses the state file and meta sidecar it would restore
/// from. An open background row loses those files and leaves the running
/// instance too. Its shells and runs end, and its hook server stops. The
/// active workspace is refused with a status message, because the picker's
/// own input lives in it and `close_workspace` refuses the last workspace for
/// the same reason.
///
/// Sub-sessions are ephemeral, so a family dies together. The rows of the
/// sessions created inside the selected one go with it, and a family whose
/// rows hold the active workspace is refused whole. An untouched active
/// sub-session has no row and nothing saved, so it stays open.
///
/// An open row's files resolve against the real state directory, so their
/// deletion sits behind [`Stoat::persistence_disabled`], which the test
/// harness sets to keep `$XDG_STATE_HOME` pristine. An inactive row carries
/// its own state path, so that deletion is unconditional.
pub(super) fn workspace_picker_delete(stoat: &mut Stoat) -> UpdateEffect {
    let Some((basename, family, rows)) = stoat.workspace_picker.as_ref().and_then(|picker| {
        let selected = *picker.filtered().get(picker.selected())?;
        let entries = picker.entries();

        let mut family = vec![selected];
        family.extend(picker.sub_sessions_of(selected));
        let rows: Vec<(Option<WorkspaceId>, Option<PathBuf>)> = family
            .iter()
            .map(|&idx| (entries[idx].id, entries[idx].state_path.clone()))
            .collect();
        Some((entries[selected].basename.clone(), family, rows))
    }) else {
        return UpdateEffect::None;
    };

    // The family starts with the selected row, so that row's id leads `rows`.
    let active = Some(stoat.active_workspace);
    if rows[0].0 == active {
        stoat.set_status("cannot delete the active workspace");
        return UpdateEffect::Redraw;
    }
    if rows.iter().any(|&(id, _)| id == active) {
        stoat.set_status("cannot delete: the active workspace is in this family");
        return UpdateEffect::Redraw;
    }

    for (id, state_path) in rows {
        match (id, state_path) {
            (Some(id), _) => {
                stoat.pending_workspace_saves.remove(&id);
                remove_open_session(stoat, id);
                release_workspace(stoat, id);
                stoat.workspaces.remove(id);
            },
            (None, Some(state_path)) => remove_session_files(&*stoat.fs_host, &state_path),
            (None, None) => {},
        }
    }

    if let Some(picker) = stoat.workspace_picker.as_mut() {
        picker.remove_entries(&family);
    }
    stoat.set_status(deleted_status(&basename, family.len() - 1));
    UpdateEffect::Redraw
}

/// The status a picker delete reports, which names the session and counts
/// the sub-sessions that went with it.
fn deleted_status(basename: &str, subs: usize) -> String {
    match subs {
        0 => format!("deleted session {basename}"),
        1 => format!("deleted session {basename} and 1 sub-session"),
        subs => format!("deleted session {basename} and {subs} sub-sessions"),
    }
}

/// Close the open session under the workspace picker's selection and keep it
/// on disk.
///
/// The workspace saves, then leaves the running instance. Its shells and runs
/// end, and its hook server stops. Its row stays in the list as a saved
/// session, which Enter restores. [`workspace_picker_delete`] removes a session
/// from the list and the disk instead.
///
/// The close takes the selected row alone. A sub-session is a workspace of its
/// own, tied to its family only by its parent uid, so it stays open. A
/// workspace with no save path, because persistence is off or the workspace is
/// fresh, leaves no session to list, so its row goes.
///
/// The active row is refused, because the picker's own input lives in it. A
/// saved row is refused, because it has no open session. A failed save keeps
/// the workspace open, because a close after it ends state that no file holds.
pub(super) fn workspace_picker_close_session(stoat: &mut Stoat) -> UpdateEffect {
    let Some((entry_idx, id, basename)) = stoat.workspace_picker.as_ref().and_then(|picker| {
        let entry_idx = *picker.filtered().get(picker.selected())?;
        let entry = &picker.entries()[entry_idx];
        Some((entry_idx, entry.id, entry.basename.clone()))
    }) else {
        return UpdateEffect::None;
    };

    let Some(id) = id else {
        stoat.set_status(format!("session {basename} is not open"));
        return UpdateEffect::Redraw;
    };
    if id == stoat.active_workspace {
        stoat.set_status("cannot close the active workspace");
        return UpdateEffect::Redraw;
    }

    let state_path = stoat.workspace_save_path(&stoat.workspaces[id]);
    if let Some(path) = &state_path
        && let Err(err) = stoat.workspaces[id].save_state(path, &*stoat.fs_host)
    {
        tracing::warn!(target: "stoat::workspace", ?path, ?err, "failed to save closing workspace");
        stoat.set_status(format!("cannot close session {basename}: its save failed"));
        return UpdateEffect::Redraw;
    }

    // The save above took a newer gate number than any save still queued, so
    // a queued write lands nothing.
    stoat.pending_workspace_saves.remove(&id);
    release_workspace(stoat, id);
    stoat.workspaces.remove(id);

    if let Some(picker) = stoat.workspace_picker.as_mut() {
        match state_path {
            Some(path) => picker.mark_closed(entry_idx, path),
            None => picker.remove_entries(&[entry_idx]),
        }
    }
    stoat.set_status(format!("closed session {basename}"));
    UpdateEffect::Redraw
}

/// Remove the session files of the open workspace `id`, after stopping every
/// write still queued for it.
///
/// The blocking pool runs a save it already holds whatever happens to its
/// task, and that write puts the files back unless the gate stops it. The
/// retire and the removal run on the pool too, so nothing here waits. There
/// the retire waits for a write in progress and stops every later one.
///
/// The files resolve against the real state directory, so their removal sits
/// behind [`Stoat::persistence_disabled`].
fn remove_open_session(stoat: &Stoat, id: WorkspaceId) {
    let ws = &stoat.workspaces[id];
    let gate = Arc::clone(&ws.save_gate);
    let path = if stoat.persistence_disabled {
        None
    } else {
        state_path_for(&ws.git_root, ws.uid, &*stoat.fs_host).ok()
    };
    let fs = Arc::clone(&stoat.fs_host);
    stoat
        .executor
        .spawn_blocking(move || {
            gate.retire();
            if let Some(path) = path {
                remove_session_files(&*fs, &path);
            }
        })
        .detach();
}

/// Remove a workspace's state file and its meta sidecar, skipping either if it
/// is already absent and reporting a failed removal to the log.
fn remove_session_files(fs: &dyn FsHost, state_path: &Path) {
    let meta_path = registry::meta_path_for(state_path);
    for target in [state_path, meta_path.as_path()] {
        if fs.exists(target)
            && let Err(err) = fs.remove_file(target)
        {
            tracing::warn!(
                target: "stoat::workspace",
                ?target,
                ?err,
                "failed to delete workspace file"
            );
        }
    }
}

/// End every shell, run, and agent session of the open workspace `id`, and
/// stop its hook server.
///
/// A reader task holds its session, so a workspace removed without this
/// leaves its shells running and its socket bound until exit. Each session
/// ends the way a tab close ends it. The exit of a killed shell then lands
/// for a workspace that no longer exists, and drops.
fn release_workspace(stoat: &mut Stoat, id: WorkspaceId) {
    let executor = stoat.executor.clone();
    let ws = &mut stoat.workspaces[id];

    let terms: Vec<_> = ws.terms.keys().collect();
    for term_id in terms {
        super::pane::dispose_view(
            ws,
            &executor,
            View::Terminal(term_id),
            EditorDisposal::Remove,
        );
    }
    let runs: Vec<_> = ws.runs.keys().collect();
    for run_id in runs {
        super::pane::dispose_view(ws, &executor, View::Run(run_id), EditorDisposal::Remove);
    }

    let uid = ws.uid;
    stoat.agent_servers.remove(&uid);
}

/// Close the workspace picker, or end a rename in it and keep it open.
///
/// Escape during a rename returns the input to the filter query and leaves the
/// row's name as it was.
pub(super) fn workspace_picker_close(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(picker) = stoat
        .workspace_picker
        .as_mut()
        .filter(|picker| picker.renaming())
    {
        picker.rename_cancel();
        restore_picker_filter(stoat);
        return UpdateEffect::Redraw;
    }
    if let Some(picker) = stoat.workspace_picker.take() {
        picker.dispose(stoat.active_workspace_mut());
    }
    UpdateEffect::Redraw
}

/// Open the workspace finder, listing the open workspaces and every saved
/// session the registry knows.
///
/// Reached from `SwitchWorkspace` and from a bare launch, which has no files to
/// show and so asks which project to enter. This handler always opens the
/// finder, even at a lone row, because `SwitchWorkspace` is an explicit ask and
/// deserves an answer. The bare-launch wrapper
/// [`Stoat::open_workspace_picker`] skips the lone-row case instead.
///
/// A fresh active workspace gets no row when another row exists. It holds
/// nothing, and Enter on its row only closes the finder, so the selection
/// starts on a session to enter. The row stays when a saved session carries
/// the workspace's uid, because that workspace is a restore in flight.
///
/// The mode drops to normal first, because the picker's own input takes insert
/// and the editor underneath must not keep a mode it no longer owns.
pub(crate) fn open_workspace_picker(stoat: &mut Stoat) -> UpdateEffect {
    let inactive = registry::list_all(&*stoat.fs_host).unwrap_or_default();
    open_workspace_picker_over(stoat, inactive)
}

/// [`open_workspace_picker`] over an explicit list of saved sessions.
pub(super) fn open_workspace_picker_over(
    stoat: &mut Stoat,
    inactive: Vec<RegistryEntry>,
) -> UpdateEffect {
    let omit_active = {
        let ws = stoat.active_workspace();
        ws.is_fresh() && !inactive.iter().any(|reg| reg.meta.uid == ws.uid)
    };

    stoat.set_focused_mode("normal".into());
    let input = {
        let executor = stoat.executor.clone();
        InputView::create(
            stoat.active_workspace_mut(),
            executor,
            SubmitTarget::WorkspacePicker,
            "",
            "insert",
        )
    };
    let mut picker =
        WorkspacePicker::new(&stoat.workspaces, stoat.active_workspace, inactive, input);
    if omit_active {
        picker.omit_active();
    }
    stoat.workspace_picker = Some(picker);
    UpdateEffect::Redraw
}

/// Refilter the open workspace picker against its input text on the idle path,
/// so typing narrows the list without a dedicated key handler.
pub(crate) fn sync_workspace_picker(stoat: &mut Stoat) {
    let Some(query) = stoat
        .workspace_picker
        .as_ref()
        .map(|picker| picker.input.text(stoat.active_workspace()))
    else {
        return;
    };

    if let Some(picker) = stoat.workspace_picker.as_mut() {
        picker.refilter(&query);
    }
}

/// Switch to the workspace under the picker's selection, saving the current
/// one first.
///
/// An open row switches focus directly. An inactive on-disk row is brought back
/// into the instance via [`activate_inactive_workspace`]. A selection on the
/// already-active workspace, or an empty query with no row, just closes the
/// picker.
///
/// A query with no row names a new root session, in no family, rooted where
/// the active one is, and the picker enters it.
///
/// An untouched active workspace whose row the picker omitted leaves the
/// instance with the switch or the create, since no row shows it and no save
/// keeps it.
///
/// During a rename, Enter applies the name instead, and the picker stays open.
pub(super) fn workspace_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    if stoat
        .workspace_picker
        .as_ref()
        .is_some_and(WorkspacePicker::renaming)
    {
        return workspace_picker_rename_finish(stoat);
    }
    let Some(picker) = stoat.workspace_picker.take() else {
        return UpdateEffect::None;
    };
    let query = picker
        .input
        .text(stoat.active_workspace())
        .trim()
        .to_owned();
    picker.dispose(stoat.active_workspace_mut());

    match picker.selected_entry() {
        None if query.is_empty() => return UpdateEffect::Redraw,
        None => {
            create_workspace(stoat, None);
            stoat.active_workspace_mut().name = query.clone();
            stoat.set_status(format!("created session {query}"));
        },
        Some(entry) => match (entry.id, entry.state_path.clone()) {
            (Some(id), _) => {
                if id == stoat.active_workspace {
                    return UpdateEffect::Redraw;
                }
                stoat.save_workspace(stoat.active_workspace);
                switch_active_workspace(stoat, id);
            },
            (None, Some(state_path)) => {
                let git_root = entry.git_root.clone();
                let uid = entry.uid;
                let name = entry.basename.clone();
                let parent = entry.parent;
                activate_inactive_workspace(stoat, git_root, uid, name, parent, state_path);
            },
            (None, None) => return UpdateEffect::Redraw,
        },
    }

    drop_omitted_fresh_workspace(stoat, &picker);
    UpdateEffect::Redraw
}

/// Create a session named by the picker's query, tied to the active
/// workspace's family, and enter it.
///
/// The family is the one the picker's family scope lists, so the new session
/// shows there. A fresh launch workspace whose row the picker omitted leaves
/// with the create, and a sub-session of it names a parent that no row shows,
/// so from there the create makes a root session.
///
/// An empty query and a rename in progress do nothing, and the picker stays
/// open.
pub(super) fn workspace_picker_create_sub(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.workspace_picker.as_ref() else {
        return UpdateEffect::None;
    };
    if picker.renaming() {
        return UpdateEffect::None;
    }
    let query = picker
        .input
        .text(stoat.active_workspace())
        .trim()
        .to_owned();
    if query.is_empty() {
        return UpdateEffect::None;
    }
    let Some(picker) = stoat.workspace_picker.take() else {
        return UpdateEffect::None;
    };
    picker.dispose(stoat.active_workspace_mut());

    let launch_leaves = picker.omitted_workspace == Some(stoat.active_workspace)
        && stoat.active_workspace().is_fresh();
    let parent = match launch_leaves {
        true => None,
        false => Some(stoat.active_workspace().family_root()),
    };
    create_workspace(stoat, parent);
    stoat.active_workspace_mut().name = query.clone();
    stoat.set_status(match parent {
        Some(_) => format!("created sub-session {query}"),
        None => format!("created session {query}"),
    });
    drop_omitted_fresh_workspace(stoat, &picker);
    UpdateEffect::Redraw
}

/// Close the untouched workspace whose row `picker` omitted, once a switch or
/// a create left it behind.
///
/// The omit rule judged the workspace active when the picker opened, so the
/// drop names that workspace rather than the one active now. Its freshness is
/// read again, because a workspace that holds something is no orphan.
fn drop_omitted_fresh_workspace(stoat: &mut Stoat, picker: &WorkspacePicker) {
    let orphan = picker
        .omitted_workspace
        .filter(|&id| id != stoat.active_workspace)
        .filter(|&id| stoat.workspaces.get(id).is_some_and(Workspace::is_fresh));
    if let Some(id) = orphan {
        stoat.pending_workspace_saves.remove(&id);
        release_workspace(stoat, id);
        stoat.workspaces.remove(id);
    }
}

/// Bring a persisted-but-closed workspace back into the running instance and
/// switch to it, returning the id of the newly inserted workspace.
///
/// The fresh workspace carries the on-disk `uid`, `name`, and `parent` from the
/// start, so a save triggered before the restore lands (a quit mid-restore)
/// rewrites the same state file rather than forking a new one. The outgoing
/// workspace is saved, focus switches, and a restore spawns to replace the fresh
/// panes with the persisted session.
fn activate_inactive_workspace(
    stoat: &mut Stoat,
    git_root: PathBuf,
    uid: WorkspaceUid,
    name: String,
    parent: Option<WorkspaceUid>,
    state_path: PathBuf,
) -> WorkspaceId {
    stoat.save_workspace(stoat.active_workspace);

    let mut ws = Workspace::new(git_root, &stoat.executor, stoat.redraw_notify.clone());
    ws.uid = uid;
    ws.name = name;
    ws.parent = parent;
    ws.layout(stoat.size());

    let id = stoat.workspaces.insert(ws);
    stoat.workspaces[id].id = id;

    switch_active_workspace(stoat, id);
    stoat.spawn_workspace_restore(id, state_path);
    id
}

pub(super) fn handle_dump(stoat: &Stoat, name: &str) {
    match crate::dump::dumps_dir() {
        Ok(dumps) => dump_into(stoat, name, OffsetDateTime::now_utc(), &dumps),
        Err(e) => {
            tracing::error!(target: "stoat::workspace", error = %e, name = %name, "dump failed")
        },
    }
}

/// Snapshot the active workspace for the dump `name` and write its bundle into
/// `dumps` on the blocking pool.
///
/// The snapshot is cheap and taken here, so the bundle holds the workspace as it
/// stood at the command. The write reads every file under the git root, which
/// takes seconds on a large repository, so it runs off the run loop and logs
/// its own outcome.
fn dump_into(stoat: &Stoat, name: &str, at: OffsetDateTime, dumps: &Path) {
    let workspace = stoat.active_workspace();
    let pending = match crate::dump::capture(workspace, stoat.focused_mode(), name, at, dumps) {
        Ok(pending) => pending,
        Err(e) => {
            tracing::error!(target: "stoat::workspace", error = %e, name = %name, "dump failed");
            return;
        },
    };

    let fs = stoat.fs_host.clone();
    let name = name.to_owned();
    stoat
        .executor
        .spawn_blocking(move || match pending.write(fs.as_ref()) {
            Ok(id) => tracing::info!(target: "stoat::workspace", id = %id, "dump captured"),
            Err(e) => {
                tracing::error!(target: "stoat::workspace", error = %e, name = %name, "dump failed")
            },
        })
        .detach();
}

pub(super) fn rename_workspace(stoat: &mut Stoat, name: &str) {
    let ws = stoat.active_workspace_mut();
    let old = ws.display_name().to_owned();
    ws.name = name.to_string();
    tracing::info!(target: "stoat::workspace", old = %old, name = %name, "renamed workspace");
}

/// Set the active workspace's `git_root` to `path`, the root the file finder,
/// diff, and review resolve against.
///
/// A leading `~` or `~/` resolves against `$HOME`, an absolute path is taken
/// as-is, and a relative path resolves against the current root. The new root,
/// or why the change was refused, surfaces as the one-shot bottom-row status
/// message. An empty path, an unresolvable path (including a `~` form with no
/// `$HOME`), or a non-directory leaves the root untouched.
///
/// The new root is watched, so writes under it reach the diff and follow.
///
/// A session file is keyed by its root, so the saved session moves with the
/// workspace. The file under the old root goes, and the workspace saves under
/// the new one.
pub(super) fn set_cwd(stoat: &mut Stoat, path: &str) {
    let path = path.trim();
    if path.is_empty() {
        stoat.set_status("cd: empty path");
        return;
    }

    let candidate = {
        let tilde = if path == "~" {
            Some("")
        } else {
            path.strip_prefix("~/")
        };
        match tilde.zip(stoat.env_host.var("HOME")) {
            Some((rest, home)) => Path::new(&home).join(rest),
            None if Path::new(path).is_absolute() => Path::new(path).to_path_buf(),
            None => stoat.active_workspace().git_root.join(path),
        }
    };

    match stoat.fs_host.canonicalize(&candidate) {
        Ok(abs)
            if stoat
                .fs_host
                .metadata(&abs)
                .ok()
                .flatten()
                .is_some_and(|m| m.is_dir) =>
        {
            let ws_id = stoat.active_workspace;
            let carried = session_to_carry(stoat);
            {
                let ws = stoat.active_workspace_mut();
                ws.git_root = abs;
                // A new root has its own diff to warm, so re-arm the warm pass.
                ws.diff_warmed = false;
                // The old root's direnv diff must never leak into spawns under
                // the new root, so drop it. A reload below repopulates it.
                ws.env = crate::project_env::WorkspaceEnv::default();
            }
            if let Some(old) = carried {
                carry_session(stoat, ws_id, &old);
            }
            stoat.watch_active_root();

            if stoat.env_auto_load && stoat.settings.direnv_load.unwrap_or(true) {
                if stoat.settings.direnv_reload_on_cd.unwrap_or(true) {
                    crate::project_env::spawn_load(stoat, ws_id, false);
                } else {
                    // The next background pump loads an unloaded env, so the
                    // env parks off until an explicit reload.
                    stoat.active_workspace_mut().env.state = crate::project_env::EnvLoadState::Off;
                }
            }

            let root = stoat.active_workspace().git_root.display().to_string();
            tracing::info!(target: "stoat::workspace", root = %root, "workspace root changed");
            stoat.set_status(format!("Current working directory is now {root}"));
        },
        Ok(_) => {
            stoat.set_status(format!("cd: not a directory: {}", candidate.display()));
        },
        Err(e) => {
            stoat.set_status(format!("cd: cannot resolve {}: {e}", candidate.display()));
        },
    }
}

/// The session file of the active workspace that a `:cd` carries to the new
/// root, read before the root changes.
///
/// [`None`] with persistence off, and for a fresh workspace. A picker restore
/// into a fresh workspace reads the file under its uid until the restore
/// lands, so the file stays where it is.
fn session_to_carry(stoat: &Stoat) -> Option<PathBuf> {
    let ws = stoat.active_workspace();
    if stoat.persistence_disabled || ws.is_fresh() {
        return None;
    }
    state_path_for(&ws.git_root, ws.uid, &*stoat.fs_host).ok()
}

/// Move the session of workspace `ws_id` from `old` to the file its new root
/// names, so no copy stays under the old root.
///
/// Every write issued before the move names `old`, and one that lands after
/// the removal brings the old file back. The supersede and the removal run on
/// the blocking pool, behind a write in progress, and skip every write through
/// the boundary read here. An older write that runs first writes the old file,
/// and the removal behind the supersede deletes it. The save issued after the
/// boundary takes a higher number, so it lands whether its write or the
/// supersede runs first.
fn carry_session(stoat: &mut Stoat, ws_id: WorkspaceId, old: &Path) {
    let ws = &stoat.workspaces[ws_id];
    let Ok(new) = state_path_for(&ws.git_root, ws.uid, &*stoat.fs_host) else {
        return;
    };
    if new == old {
        return;
    }
    let boundary = ws.save_gate.issued();
    let gate = Arc::clone(&ws.save_gate);
    let fs = Arc::clone(&stoat.fs_host);
    let old = old.to_path_buf();
    stoat
        .executor
        .spawn_blocking(move || {
            gate.supersede_through(boundary);
            remove_session_files(&*fs, &old);
        })
        .detach();
    stoat.save_workspace(ws_id);
}

/// Report the active workspace's `git_root` as the one-shot bottom-row status
/// message.
///
/// This is the root the file finder, diff, and review resolve against, which
/// [`set_cwd`] moves. It is not the process working directory, which is never
/// changed.
pub(super) fn show_cwd(stoat: &mut Stoat) {
    let root = stoat.active_workspace().git_root.display().to_string();
    stoat.set_status(format!("Current working directory is {root}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        badge::BadgeSource,
        dump::{self, DumpId},
        host::{FakeTerminalHost, FakeTerminalSession},
        input_view::{InputView, SubmitTarget},
        test_harness::TestHarness,
        workspace::{
            self,
            registry::{RegistryEntry, WorkspaceMeta},
            SaveGate,
        },
        workspace_picker::{WorkspacePicker, WorkspaceStatus},
    };
    use std::{
        sync::Arc,
        time::{Duration, UNIX_EPOCH},
    };
    use time::macros::datetime;

    fn picker_input(stoat: &mut Stoat) -> InputView {
        let executor = stoat.executor.clone();
        InputView::create(
            stoat.active_workspace_mut(),
            executor,
            SubmitTarget::WorkspacePicker,
            "",
            "insert",
        )
    }

    /// The picker's rows by display name, so a test asserts the whole list
    /// rather than only the row it expects to have changed.
    fn picker_rows(stoat: &Stoat) -> Vec<String> {
        stoat
            .workspace_picker
            .as_ref()
            .expect("picker open")
            .entries()
            .iter()
            .map(|entry| entry.basename.clone())
            .collect()
    }

    /// The text in the open picker's input.
    fn picker_text(stoat: &Stoat) -> String {
        let picker = stoat.workspace_picker.as_ref().expect("picker open");
        picker.input.text(stoat.active_workspace())
    }

    /// Replace the text in the open picker's input, as typing it does.
    fn replace_picker_text(stoat: &mut Stoat, text: &str) {
        let active = stoat.active_workspace;
        let picker = stoat.workspace_picker.as_ref().expect("picker open");
        picker
            .input
            .replace_text(&mut stoat.workspaces[active], text);
    }

    fn saved_session(uid: WorkspaceUid, name: &str) -> RegistryEntry {
        RegistryEntry {
            meta: WorkspaceMeta {
                uid,
                name: name.to_string(),
                git_root: PathBuf::from("/proj"),
                buffer_count: 1,
                remote_host: None,
                parent: None,
            },
            state_path: PathBuf::from("/state/hash/1.ron"),
            mtime: UNIX_EPOCH,
        }
    }

    #[test]
    fn selecting_inactive_row_activates_it_with_the_metas_uid_and_spawns_a_restore() {
        let mut harness = Stoat::test();
        let stoat = &mut harness.stoat;

        let uid = WorkspaceUid(424242);
        let entry = RegistryEntry {
            meta: WorkspaceMeta {
                uid,
                name: "proj".to_string(),
                git_root: PathBuf::from("/proj"),
                buffer_count: 1,
                remote_host: None,
                parent: Some(WorkspaceUid(7)),
            },
            state_path: PathBuf::from("/state/hash/1.ron"),
            mtime: UNIX_EPOCH,
        };

        let before = stoat.workspaces.len();
        let input = picker_input(stoat);
        let mut picker = WorkspacePicker::new(
            &stoat.workspaces,
            stoat.active_workspace,
            vec![entry],
            input,
        );
        picker.select_next();
        assert_eq!(
            picker.selected_entry().map(|e| (e.id, e.uid)),
            Some((None, uid)),
            "selection sits on the inactive on-disk row"
        );
        stoat.workspace_picker = Some(picker);

        workspace_picker_select(stoat);

        assert_eq!(
            stoat.workspaces.len(),
            before + 1,
            "reactivation inserts a new workspace"
        );
        let active = stoat.active_workspace();
        assert_eq!(
            (
                active.uid,
                active.parent,
                active.git_root.as_path(),
                active.name.as_str()
            ),
            (uid, Some(WorkspaceUid(7)), Path::new("/proj"), "proj"),
            "the new active workspace carries the meta's identity"
        );
        assert!(
            active
                .badges
                .find_by_source(BadgeSource::SessionRestore)
                .is_some(),
            "a session restore was spawned for the reactivated workspace"
        );
    }

    #[test]
    fn entering_a_saved_session_watches_its_root() {
        let mut harness = Stoat::test();
        harness.fake_git().add_repo("/proj");
        harness.fake_fs().insert_file("/proj/src/a.rs", "");
        let input = picker_input(&mut harness.stoat);
        let mut picker = WorkspacePicker::new(
            &harness.stoat.workspaces,
            harness.stoat.active_workspace,
            vec![saved_session(WorkspaceUid(7), "proj")],
            input,
        );
        picker.select_next();
        harness.stoat.workspace_picker = Some(picker);

        workspace_picker_select(&mut harness.stoat);
        harness.run_until_parked();

        assert_eq!(
            watch_counts(&harness, ["/proj", "/proj/src"]),
            [1; 2],
            "the entered workspace's directories are watched",
        );
    }

    #[test]
    fn cd_watches_the_new_root_once() {
        let mut harness = Stoat::test();
        harness.fake_git().add_repo("/other");
        harness.fake_fs().insert_file("/other/lib/b.rs", "");

        for _ in 0..2 {
            set_cwd(&mut harness.stoat, "/other");
            harness.run_until_parked();
        }

        assert_eq!(
            watch_counts(&harness, ["/other", "/other/lib"]),
            [1; 2],
            "a root entered twice is walked once",
        );
    }

    /// A `:cd` carries the saved session to the new root's file and leaves
    /// none under the old root. A save issued before the move, as a queued
    /// autosave is, lands nothing after it, even when no save under the new
    /// root has landed to outrank it.
    ///
    /// Persistence is on, so the paths resolve against the state directory,
    /// but every write goes to the harness's fake filesystem.
    #[test]
    fn cd_carries_the_saved_session_to_the_new_root() {
        let cd = |fail_new_write: bool| {
            let mut harness = Stoat::test();
            harness.stoat.persistence_disabled = false;
            harness.fake_fs().insert_file("/proj/a.txt", "");
            harness.fake_fs().insert_file("/other/b.txt", "");
            harness.stoat.active_workspace_mut().git_root = PathBuf::from("/proj");
            harness.seed_focused_buffer("edited\n");

            let uid = harness.stoat.active_workspace().uid;
            let [old, new] = ["/proj", "/other"].map(|root| {
                state_path_for(Path::new(root), uid, &*harness.stoat.fs_host).expect("state path")
            });
            harness
                .stoat
                .save_workspace_now(harness.stoat.active_workspace());
            let gate = Arc::clone(&harness.stoat.active_workspace().save_gate);
            let queued = gate.issue();
            let (state, meta) = {
                let ws = harness.stoat.active_workspace();
                (ws.to_state(), ws.meta())
            };
            if fail_new_write {
                harness
                    .fake_fs()
                    .fail_writes_to(&new, std::io::ErrorKind::PermissionDenied);
            }

            set_cwd(&mut harness.stoat, "/other");
            harness.run_until_parked();
            let fs = &*harness.stoat.fs_host;
            let late = workspace::write_state_gated(&gate, queued, &state, &meta, &old, fs)
                .expect("the write runs");
            (
                [fs.exists(&old), fs.exists(&registry::meta_path_for(&old))],
                fs.exists(&new),
                late,
            )
        };

        assert_eq!(
            [cd(false), cd(true)],
            [
                ([false, false], true, false),
                ([false, false], false, false)
            ],
        );
    }

    /// A fresh workspace leaves the session file under its uid in place on a
    /// `:cd`, since a picker restore into it reads that file until it lands.
    #[test]
    fn cd_in_a_fresh_workspace_leaves_the_session_file() {
        let mut harness = Stoat::test();
        harness.stoat.persistence_disabled = false;
        harness.fake_fs().insert_file("/other/b.txt", "");
        harness.stoat.active_workspace_mut().git_root = PathBuf::from("/proj");
        let uid = harness.stoat.active_workspace().uid;
        let old =
            state_path_for(Path::new("/proj"), uid, &*harness.stoat.fs_host).expect("state path");
        harness.fake_fs().insert_file(&old, "()");

        set_cwd(&mut harness.stoat, "/other");
        harness.run_until_parked();

        assert!(harness.stoat.fs_host.exists(&old));
    }

    /// How many watches the harness's watcher holds on each of `dirs`.
    fn watch_counts(harness: &TestHarness, dirs: [&str; 2]) -> [usize; 2] {
        dirs.map(|dir| harness.fake_fs_watcher().watch_count(Path::new(dir)))
    }

    /// Persistence stays disabled, the harness default, so the deletion under
    /// test is the inactive row's own `state_path` rather than a path resolved
    /// against the real state directory.
    #[test]
    fn deleting_an_inactive_row_removes_its_state_and_meta_files() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().name = "alpha".to_string();
        let state_path = PathBuf::from("/state/hash/1.ron");
        let meta_path = registry::meta_path_for(&state_path);
        harness.seed_fixture(&state_path, b"()");
        harness.seed_fixture(&meta_path, b"()");

        let stoat = &mut harness.stoat;
        let entry = RegistryEntry {
            meta: WorkspaceMeta {
                uid: WorkspaceUid(424242),
                name: "proj".to_string(),
                git_root: PathBuf::from("/proj"),
                buffer_count: 1,
                remote_host: None,
                parent: None,
            },
            state_path: state_path.clone(),
            mtime: UNIX_EPOCH,
        };

        let input = picker_input(stoat);
        let mut picker = WorkspacePicker::new(
            &stoat.workspaces,
            stoat.active_workspace,
            vec![entry],
            input,
        );
        picker.select_next();
        stoat.workspace_picker = Some(picker);

        assert_eq!(workspace_picker_delete(stoat), UpdateEffect::Redraw);

        assert_eq!(
            (
                stoat.fs_host.exists(&state_path),
                stoat.fs_host.exists(&meta_path)
            ),
            (false, false),
            "the session's state file and its meta sidecar are both gone"
        );
        assert_eq!(
            picker_rows(stoat),
            ["alpha"],
            "only the active workspace's row is left"
        );
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("deleted session proj"),
            "the status names the deleted session"
        );
    }

    /// `other` is a sub-session of another family, so its files and its row
    /// stay.
    #[test]
    fn deleting_a_parent_deletes_its_saved_sub_sessions() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().name = "alpha".to_string();
        let sessions: Vec<RegistryEntry> = [
            (1, "proj", None),
            (2, "notes", Some(1)),
            (3, "logs", Some(1)),
            (4, "other", Some(9)),
        ]
        .into_iter()
        .map(|(uid, name, parent)| {
            let mut entry = saved_session(WorkspaceUid(uid), name);
            entry.meta.parent = parent.map(WorkspaceUid);
            entry.state_path = PathBuf::from(format!("/state/hash/{uid}.ron"));
            entry
        })
        .collect();
        let files: Vec<PathBuf> = sessions
            .iter()
            .flat_map(|s| [s.state_path.clone(), registry::meta_path_for(&s.state_path)])
            .collect();
        for file in &files {
            harness.seed_fixture(file, b"()");
        }

        let stoat = &mut harness.stoat;
        let input = picker_input(stoat);
        stoat.workspace_picker = Some(WorkspacePicker::new(
            &stoat.workspaces,
            stoat.active_workspace,
            sessions,
            input,
        ));

        assert_eq!(workspace_picker_delete(stoat), UpdateEffect::Redraw);

        let kept: Vec<bool> = files.iter().map(|f| stoat.fs_host.exists(f)).collect();
        assert_eq!(
            (kept, picker_rows(stoat), stoat.pending_message.as_deref()),
            (
                vec![false, false, false, false, false, false, true, true],
                vec!["alpha".to_string(), "other".to_string()],
                Some("deleted session proj and 2 sub-sessions"),
            ),
            "the parent goes with both sub-sessions, and only their files and rows"
        );
    }

    #[test]
    fn the_delete_status_counts_sub_sessions() {
        assert_eq!(
            [0, 1, 2].map(|subs| deleted_status("proj", subs)),
            [
                "deleted session proj",
                "deleted session proj and 1 sub-session",
                "deleted session proj and 2 sub-sessions",
            ]
        );
    }

    #[test]
    fn deleting_a_parent_whose_sub_session_is_active_is_refused() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().name = "alpha".to_string();
        harness.stoat.active_workspace_mut().parent = Some(WorkspaceUid(424242));
        let state_path = PathBuf::from("/state/hash/1.ron");
        harness.seed_fixture(&state_path, b"()");

        let stoat = &mut harness.stoat;
        let input = picker_input(stoat);
        stoat.workspace_picker = Some(WorkspacePicker::new(
            &stoat.workspaces,
            stoat.active_workspace,
            vec![saved_session(WorkspaceUid(424242), "proj")],
            input,
        ));

        assert_eq!(workspace_picker_delete(stoat), UpdateEffect::Redraw);

        assert_eq!(
            (
                stoat.fs_host.exists(&state_path),
                picker_rows(stoat),
                stoat.pending_message.as_deref()
            ),
            (
                true,
                vec!["alpha".to_string(), "proj".to_string()],
                Some("cannot delete: the active workspace is in this family"),
            ),
            "the whole family stays"
        );
    }

    #[test]
    fn deleting_the_active_row_is_refused() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().name = "alpha".to_string();
        let stoat = &mut harness.stoat;

        let input = picker_input(stoat);
        let picker =
            WorkspacePicker::new(&stoat.workspaces, stoat.active_workspace, Vec::new(), input);
        stoat.workspace_picker = Some(picker);
        let before = stoat.workspaces.len();

        assert_eq!(workspace_picker_delete(stoat), UpdateEffect::Redraw);

        assert_eq!(stoat.workspaces.len(), before, "no workspace is dropped");
        assert_eq!(
            picker_rows(stoat),
            ["alpha"],
            "the refused row stays in the list"
        );
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("cannot delete the active workspace"),
            "the status explains the refusal"
        );
    }

    #[test]
    fn deleting_an_open_background_row_removes_the_workspace() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().name = "alpha".to_string();
        let stoat = &mut harness.stoat;

        let second = {
            let ws = Workspace::new(
                PathBuf::from("/tmp/beta"),
                &stoat.executor,
                stoat.redraw_notify.clone(),
            );
            let id = stoat.workspaces.insert(ws);
            stoat.workspaces[id].id = id;
            stoat.workspaces[id].name = "beta".to_string();
            id
        };

        let input = picker_input(stoat);
        let mut picker =
            WorkspacePicker::new(&stoat.workspaces, stoat.active_workspace, Vec::new(), input);
        picker.select_next();
        assert_eq!(
            picker.selected_entry().map(|e| e.id),
            Some(Some(second)),
            "selection sits on the open background row"
        );
        stoat.workspace_picker = Some(picker);

        assert_eq!(workspace_picker_delete(stoat), UpdateEffect::Redraw);

        assert!(
            !stoat.workspaces.contains_key(second),
            "the background workspace leaves the running instance"
        );
        assert_eq!(
            picker_rows(stoat),
            ["alpha"],
            "the deleted workspace's row is gone"
        );
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("deleted session beta"),
            "the status names the deleted session"
        );
    }

    /// A picker over the launch workspace `alpha`, edited so it saves, which a
    /// `NewWorkspace` left in the background, with persistence on.
    ///
    /// The fresh active row is omitted, so the selection sits on the launch
    /// row. Returns the launch workspace and the state file its save lands at.
    fn picker_over_an_edited_launch(harness: &mut TestHarness) -> (WorkspaceId, PathBuf) {
        harness.stoat.persistence_disabled = false;
        harness.stoat.active_workspace_mut().name = "alpha".to_string();
        harness.seed_focused_buffer("edited\n");
        let launch = harness.stoat.active_workspace;
        let path = {
            let ws = harness.stoat.active_workspace();
            state_path_for(&ws.git_root, ws.uid, &*harness.stoat.fs_host).expect("state path")
        };

        harness.type_action("NewWorkspace()");
        open_workspace_picker_over(&mut harness.stoat, Vec::new());
        (launch, path)
    }

    /// The id, status, and state path of the picker's selected row.
    fn selected_row(stoat: &Stoat) -> (Option<WorkspaceId>, WorkspaceStatus, Option<PathBuf>) {
        let entry = stoat
            .workspace_picker
            .as_ref()
            .and_then(WorkspacePicker::selected_entry)
            .expect("a selected row");
        (entry.id, entry.status, entry.state_path.clone())
    }

    #[test]
    fn closing_an_open_background_row_saves_it_and_lists_it_inactive() {
        let mut harness = Stoat::test();
        let (launch, path) = picker_over_an_edited_launch(&mut harness);
        let files = [registry::meta_path_for(&path), path.clone()];
        // The switch to the new workspace saved the launch one. Without its
        // files, every file after the close is the close's own save.
        for file in &files {
            harness
                .stoat
                .fs_host
                .remove_file(file)
                .expect("the switch saved the launch workspace");
        }
        let on_disk = |stoat: &Stoat| files.each_ref().map(|file| stoat.fs_host.exists(file));

        assert_eq!(
            workspace_picker_close_session(&mut harness.stoat),
            UpdateEffect::Redraw
        );

        let stoat = &harness.stoat;
        assert_eq!(
            (
                stoat.workspaces.contains_key(launch),
                on_disk(stoat),
                selected_row(stoat),
                stoat.pending_message.as_deref(),
            ),
            (
                false,
                [true; 2],
                (None, WorkspaceStatus::Inactive, Some(path)),
                Some("closed session alpha"),
            ),
            "the session leaves the instance, keeps its files, and lists as saved"
        );
    }

    #[test]
    fn closing_a_row_whose_save_fails_keeps_its_session_open() {
        let mut harness = Stoat::test();
        let (launch, path) = picker_over_an_edited_launch(&mut harness);
        harness
            .fake_fs()
            .fail_writes_to(&path, std::io::ErrorKind::PermissionDenied);

        workspace_picker_close_session(&mut harness.stoat);

        let stoat = &harness.stoat;
        assert_eq!(
            (
                stoat.workspaces.contains_key(launch),
                selected_row(stoat),
                stoat.pending_message.as_deref(),
            ),
            (
                true,
                (Some(launch), WorkspaceStatus::Background, None),
                Some("cannot close session alpha: its save failed"),
            ),
            "a failed save keeps the session open and its row as it was"
        );
    }

    #[test]
    fn closing_a_background_row_with_no_save_path_drops_its_row() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().name = "alpha".to_string();
        let stoat = &mut harness.stoat;
        let second = {
            let ws = Workspace::new(
                PathBuf::from("/tmp/beta"),
                &stoat.executor,
                stoat.redraw_notify.clone(),
            );
            let id = stoat.workspaces.insert(ws);
            stoat.workspaces[id].id = id;
            stoat.workspaces[id].name = "beta".to_string();
            id
        };
        let input = picker_input(stoat);
        let picker =
            WorkspacePicker::new(&stoat.workspaces, stoat.active_workspace, Vec::new(), input);
        stoat.workspace_picker = Some(picker);

        workspace_picker_close_session(stoat);

        assert_eq!(
            (
                stoat.workspaces.contains_key(second),
                picker_rows(stoat),
                stoat.pending_message.as_deref(),
            ),
            (
                false,
                vec!["alpha".to_string()],
                Some("closed session beta")
            ),
            "a session with no save path leaves the list as well as the instance"
        );
    }

    #[test]
    fn closing_the_active_row_is_refused() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().name = "alpha".to_string();
        let stoat = &mut harness.stoat;
        let input = picker_input(stoat);
        let picker =
            WorkspacePicker::new(&stoat.workspaces, stoat.active_workspace, Vec::new(), input);
        stoat.workspace_picker = Some(picker);

        assert_eq!(workspace_picker_close_session(stoat), UpdateEffect::Redraw);

        assert_eq!(
            (
                stoat.workspaces.len(),
                picker_rows(stoat),
                stoat.pending_message.as_deref(),
            ),
            (
                1,
                vec!["alpha".to_string()],
                Some("cannot close the active workspace")
            ),
            "the active workspace stays open and listed"
        );
    }

    #[test]
    fn closing_a_saved_row_is_refused() {
        let mut harness = Stoat::test();
        let stoat = &mut harness.stoat;
        open_workspace_picker_over(stoat, vec![saved_session(WorkspaceUid(424242), "proj")]);

        assert_eq!(workspace_picker_close_session(stoat), UpdateEffect::Redraw);

        assert_eq!(
            (
                stoat.workspaces.len(),
                selected_row(stoat),
                stoat.pending_message.as_deref(),
            ),
            (
                1,
                (
                    None,
                    WorkspaceStatus::Inactive,
                    Some(PathBuf::from("/state/hash/1.ron"))
                ),
                Some("session proj is not open"),
            ),
            "a saved row has no open session to close"
        );
    }

    #[test]
    fn renaming_an_open_background_row_sets_its_name() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().name = "alpha".to_string();
        let stoat = &mut harness.stoat;
        let second = {
            let ws = Workspace::new(
                PathBuf::from("/tmp/beta"),
                &stoat.executor,
                stoat.redraw_notify.clone(),
            );
            let id = stoat.workspaces.insert(ws);
            stoat.workspaces[id].id = id;
            stoat.workspaces[id].name = "beta".to_string();
            id
        };
        let input = picker_input(stoat);
        stoat.workspace_picker = Some(WorkspacePicker::new(
            &stoat.workspaces,
            stoat.active_workspace,
            Vec::new(),
            input,
        ));

        workspace_picker_rename_start(stoat);
        let seeded = picker_text(stoat);
        replace_picker_text(stoat, "gamma");
        workspace_picker_select(stoat);

        assert_eq!(
            (
                seeded.as_str(),
                stoat.workspaces[second].name.as_str(),
                picker_rows(stoat),
                picker_text(stoat),
                stoat.pending_message.as_deref(),
            ),
            (
                "beta",
                "gamma",
                vec!["alpha".to_string(), "gamma".to_string()],
                String::new(),
                Some("renamed session beta to gamma"),
            ),
            "the open workspace takes the name, and the picker returns to its filter"
        );
    }

    /// A restore takes the name from the state file, so the sidecar alone is
    /// not enough. The rewrite keeps the session in its family.
    #[test]
    fn renaming_a_saved_row_rewrites_its_state_and_meta() {
        let mut harness = Stoat::test();
        let state_path = PathBuf::from("/state/hash/1.ron");
        {
            let stoat = &harness.stoat;
            let mut ws = Workspace::new(
                PathBuf::from("/proj"),
                &stoat.executor,
                stoat.redraw_notify.clone(),
            );
            ws.name = "proj".to_string();
            ws.parent = Some(WorkspaceUid(7));
            ws.save_state(&state_path, &*stoat.fs_host)
                .expect("the saved session is seeded");
        }
        let stoat = &mut harness.stoat;
        let input = picker_input(stoat);
        let mut saved = saved_session(WorkspaceUid(424242), "proj");
        saved.state_path = state_path.clone();
        stoat.workspace_picker = Some(WorkspacePicker::new(
            &stoat.workspaces,
            stoat.active_workspace,
            vec![saved],
            input,
        ));

        workspace_picker_rename_start(stoat);
        replace_picker_text(stoat, "renamed");
        workspace_picker_select(stoat);
        harness.run_until_parked();

        let fs = &*harness.stoat.fs_host;
        let read = |path: &Path| {
            let mut buf = Vec::new();
            fs.read(path, &mut buf).expect("the file exists");
            String::from_utf8(buf).expect("the file is UTF-8")
        };
        let state: persist::WorkspaceStateV1 =
            ron::from_str(&read(&state_path)).expect("the state parses");
        let meta: WorkspaceMeta =
            ron::from_str(&read(&registry::meta_path_for(&state_path))).expect("the meta parses");
        assert_eq!(
            (
                state.name.as_str(),
                meta.name.as_str(),
                meta.parent,
                harness.stoat.pending_message.as_deref(),
            ),
            (
                "renamed",
                "renamed",
                Some(WorkspaceUid(7)),
                Some("renamed session proj to renamed")
            ),
        );
    }

    #[test]
    fn enter_on_an_empty_rename_keeps_the_name() {
        let mut harness = Stoat::test();
        let stoat = &mut harness.stoat;
        let input = picker_input(stoat);
        stoat.workspace_picker = Some(WorkspacePicker::new(
            &stoat.workspaces,
            stoat.active_workspace,
            vec![saved_session(WorkspaceUid(7), "proj")],
            input,
        ));

        workspace_picker_rename_start(stoat);
        replace_picker_text(stoat, "  ");
        workspace_picker_select(stoat);

        let picker = stoat.workspace_picker.as_ref();
        assert_eq!(
            (
                picker.map(WorkspacePicker::renaming),
                picker.and_then(|p| p.selected_entry().map(|e| e.basename.clone())),
                stoat.pending_message.as_deref(),
            ),
            (Some(false), Some("proj".to_string()), None),
            "the rename ends and the row keeps its name"
        );
    }

    #[test]
    fn escape_during_a_rename_restores_the_query() {
        let mut harness = Stoat::test();
        let stoat = &mut harness.stoat;
        let input = picker_input(stoat);
        stoat.workspace_picker = Some(WorkspacePicker::new(
            &stoat.workspaces,
            stoat.active_workspace,
            vec![saved_session(WorkspaceUid(7), "proj")],
            input,
        ));
        replace_picker_text(stoat, "pr");
        sync_workspace_picker(stoat);

        workspace_picker_rename_start(stoat);
        workspace_picker_close(stoat);

        let renaming = stoat
            .workspace_picker
            .as_ref()
            .map(WorkspacePicker::renaming);
        assert_eq!(
            (picker_text(stoat), renaming),
            ("pr".to_string(), Some(false)),
            "the picker stays open with the query back in its input"
        );
    }

    /// Closing a workspace or deleting its open row stops every session write
    /// still queued for it, so none brings the removed files back. The
    /// workspace left open keeps writing.
    #[test]
    fn closing_or_deleting_an_open_workspace_retires_its_saves() {
        let writes_after = |delete: bool| {
            let mut harness = Stoat::test();
            let stoat = &mut harness.stoat;
            let first = stoat.active_workspace;
            let second = {
                let ws = Workspace::new(
                    PathBuf::from("/tmp/beta"),
                    &stoat.executor,
                    stoat.redraw_notify.clone(),
                );
                let id = stoat.workspaces.insert(ws);
                stoat.workspaces[id].id = id;
                id
            };
            let gates = [first, second].map(|id| Arc::clone(&stoat.workspaces[id].save_gate));

            if delete {
                let input = picker_input(stoat);
                let mut picker = WorkspacePicker::new(&stoat.workspaces, first, Vec::new(), input);
                picker.select_next();
                stoat.workspace_picker = Some(picker);
                workspace_picker_delete(stoat);
            } else {
                stoat.active_workspace = second;
                close_workspace(stoat);
            }
            gates.map(|gate| gate_lands_a_write(&gate, stoat))
        };

        assert_eq!(
            [writes_after(false), writes_after(true)],
            [[true, false], [true, false]],
        );
    }

    /// Whether a save issued to `gate` writes, measured with a write of the
    /// active workspace to a scratch path.
    fn gate_lands_a_write(gate: &SaveGate, stoat: &Stoat) -> bool {
        let seq = gate.issue();
        let ws = stoat.active_workspace();
        workspace::write_state_gated(
            gate,
            seq,
            &ws.to_state(),
            &ws.meta(),
            Path::new("/probe/state.ron"),
            &*stoat.fs_host,
        )
        .expect("the write runs")
    }

    /// Closing a workspace or deleting its open row removes its session files.
    /// The removal runs on the pool, behind the retire of its saves.
    #[test]
    fn closing_or_deleting_an_open_workspace_removes_its_session_files() {
        let files_left = |delete: bool| {
            let mut harness = Stoat::test();
            harness.stoat.persistence_disabled = false;
            let stoat = &mut harness.stoat;
            let first = stoat.active_workspace;
            let second = {
                let ws = Workspace::new(
                    PathBuf::from("/tmp/beta"),
                    &stoat.executor,
                    stoat.redraw_notify.clone(),
                );
                let id = stoat.workspaces.insert(ws);
                stoat.workspaces[id].id = id;
                id
            };
            let files = {
                let ws = &stoat.workspaces[second];
                let path =
                    state_path_for(&ws.git_root, ws.uid, &*stoat.fs_host).expect("state path");
                [registry::meta_path_for(&path), path]
            };
            for file in &files {
                harness.seed_fixture(file, b"()");
            }

            let stoat = &mut harness.stoat;
            if delete {
                let input = picker_input(stoat);
                let mut picker = WorkspacePicker::new(&stoat.workspaces, first, Vec::new(), input);
                picker.select_next();
                stoat.workspace_picker = Some(picker);
                workspace_picker_delete(stoat);
            } else {
                stoat.active_workspace = second;
                close_workspace(stoat);
            }
            files.map(|file| harness.stoat.fs_host.exists(&file))
        };

        assert_eq!([files_left(false), files_left(true)], [[false; 2]; 2]);
    }

    /// Closing a workspace, deleting its open row, or closing its row's session
    /// ends the run and the shell of that workspace, and stops its hook server.
    #[test]
    fn closing_or_deleting_an_open_workspace_ends_its_sessions() {
        /// Put the launch workspace under a picker whose selection sits on the
        /// background row of the workspace that holds the run and the shell.
        fn pick_second(stoat: &mut Stoat) {
            let first = stoat
                .workspaces
                .keys()
                .find(|&id| id != stoat.active_workspace)
                .expect("the launch workspace");
            stoat.active_workspace = first;
            let input = picker_input(stoat);
            let mut picker = WorkspacePicker::new(&stoat.workspaces, first, Vec::new(), input);
            picker.select_next();
            stoat.workspace_picker = Some(picker);
        }

        let released_after = |remove: fn(&mut Stoat) -> UpdateEffect| {
            let mut harness = Stoat::test();
            harness.allow_host_swap();
            harness
                .stoat
                .set_agent_socket_dir("/stoat-test-never-served".into());
            harness.stoat.set_serve_agent_sockets(true);
            harness.type_action("NewWorkspace()");

            // The terminal covers the run and keeps it alive. A run opened
            // over a terminal pane ends that shell, so the run goes first.
            let run = install_fake_session(&mut harness);
            harness.open_run();
            let shell = install_fake_session(&mut harness);
            super::super::dispatch(&mut harness.stoat, &stoat_action::Terminal);
            let killed = || [run.was_killed(), shell.was_killed()];
            assert_eq!(killed(), [false, false], "both live before the removal");

            remove(&mut harness.stoat);
            harness.run_until_parked();
            (
                harness.stoat.workspaces.len(),
                killed(),
                harness.stoat.agent_servers.len(),
            )
        };

        assert_eq!(
            [
                released_after(close_workspace),
                released_after(|stoat| {
                    pick_second(stoat);
                    workspace_picker_delete(stoat)
                }),
                released_after(|stoat| {
                    pick_second(stoat);
                    workspace_picker_close_session(stoat)
                }),
            ],
            [(1, [true, true], 0); 3],
        );
    }

    /// Every spawn of one fake host shares its session, so each spawn gets a
    /// host of its own to tell its kill apart.
    fn install_fake_session(harness: &mut TestHarness) -> Arc<FakeTerminalSession> {
        let session = Arc::new(FakeTerminalSession::new());
        harness.stoat.terminal_host = Arc::new(FakeTerminalHost::new(Arc::clone(&session)));
        session
    }

    /// An untouched launch workspace that the picker drops stops its hook
    /// server. A shell that exited leaves the workspace untouched but served.
    #[test]
    fn dropping_an_untouched_launch_workspace_stops_its_hook_server() {
        let mut harness = Stoat::test();
        let stoat = &mut harness.stoat;
        stoat.set_agent_socket_dir("/stoat-test-never-served".into());
        stoat.set_serve_agent_sockets(true);
        let uid = stoat.active_workspace().uid();
        stoat.serve_term_session(uid).expect("the socket is served");

        open_workspace_picker_over(stoat, vec![saved_session(WorkspaceUid(424242), "proj")]);
        workspace_picker_select(stoat);

        assert_eq!(
            (stoat.workspaces.len(), stoat.agent_servers.len()),
            (1, 0),
            "the dropped launch workspace takes its hook server with it",
        );
    }

    /// A switch stamps the workspace it enters, so a session entered again
    /// ranks by that entry and not by its creation. The test clock moves only
    /// when told, so it advances before each step.
    #[test]
    fn a_session_entered_again_ranks_by_that_entry() {
        let mut harness = Stoat::test();
        let first = harness.stoat.active_workspace;
        for _ in 0..2 {
            harness.advance_clock(Duration::from_secs(1));
            harness.type_action("NewWorkspace()");
        }
        let third = harness.stoat.active_workspace;
        harness.advance_clock(Duration::from_secs(1));
        switch_active_workspace(&mut harness.stoat, first);
        harness.advance_clock(Duration::from_secs(1));
        switch_active_workspace(&mut harness.stoat, third);

        harness.type_action("SwitchWorkspace()");
        assert_eq!(
            harness
                .stoat
                .workspace_picker
                .as_ref()
                .and_then(WorkspacePicker::selected_id),
            Some(first),
            "the session left last is the one the picker opens on"
        );
    }

    /// A new or copied workspace joins the active family, so a session
    /// created from a sub-session ties to the root and a family never nests.
    /// The copy goes first, from the root, because a copy of a sub-session
    /// inherits the parent of its source either way. The test clock moves only
    /// when told, so it advances before each create to give each workspace its
    /// own uid.
    #[test]
    fn a_workspace_created_in_a_session_joins_its_family() {
        let mut harness = Stoat::test();
        let root = harness.stoat.active_workspace().uid;

        let mut parents = Vec::new();
        for action in ["CopyWorkspace()", "NewWorkspace()", "NewWorkspace()"] {
            harness.advance_clock(Duration::from_secs(1));
            harness.type_action(action);
            parents.push(harness.stoat.active_workspace().parent);
        }

        assert_eq!(parents, [Some(root); 3], "each create ties to the root");
    }

    /// A new workspace joins the active family, so the session outside it is a
    /// saved one.
    #[test]
    fn shift_tab_in_the_picker_narrows_to_the_family() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().name = "root".to_string();
        harness.advance_clock(Duration::from_secs(1));
        harness.type_action("NewWorkspace()");
        harness.stoat.active_workspace_mut().name = "sub".to_string();
        harness.edit_focused(0..0, "x");
        open_workspace_picker_over(
            &mut harness.stoat,
            vec![saved_session(WorkspaceUid(424242), "proj")],
        );
        let shown = |stoat: &Stoat| {
            let picker = stoat.workspace_picker.as_ref().expect("picker open");
            picker
                .filtered()
                .iter()
                .map(|&idx| picker.entries()[idx].basename.clone())
                .collect::<Vec<_>>()
        };

        let mut rows = vec![shown(&harness.stoat)];
        for _ in 0..2 {
            harness.type_keys("backtab");
            rows.push(shown(&harness.stoat));
        }

        assert_eq!(
            rows,
            [
                vec!["sub", "root", "proj"],
                vec!["sub", "root"],
                vec!["sub", "root", "proj"],
            ],
            "Shift-Tab narrows to the family, and a second press lists every session"
        );
    }

    #[test]
    fn a_fresh_active_workspace_gets_no_row() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().name = "alpha".to_string();
        let stoat = &mut harness.stoat;

        open_workspace_picker_over(stoat, vec![saved_session(WorkspaceUid(424242), "proj")]);

        assert_eq!(
            picker_rows(stoat),
            ["proj"],
            "the untouched active workspace has no row"
        );
        assert_eq!(
            stoat
                .workspace_picker
                .as_ref()
                .and_then(WorkspacePicker::selected_entry)
                .map(|entry| entry.uid),
            Some(WorkspaceUid(424242)),
            "the selection starts on the saved session"
        );
    }

    #[test]
    fn a_touched_active_workspace_keeps_its_row() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().name = "alpha".to_string();
        harness.edit_focused(0..0, "x");
        let stoat = &mut harness.stoat;

        open_workspace_picker_over(stoat, vec![saved_session(WorkspaceUid(424242), "proj")]);

        assert_eq!(
            picker_rows(stoat),
            ["alpha", "proj"],
            "an edited workspace keeps its row"
        );
    }

    /// The second session gives the list a second row. Without it, the
    /// lone-row guard keeps the active row and the uid check goes untested.
    #[test]
    fn a_fresh_workspace_under_a_saved_uid_keeps_its_row() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().name = "alpha".to_string();
        let stoat = &mut harness.stoat;
        let uid = stoat.active_workspace().uid;

        open_workspace_picker_over(
            stoat,
            vec![
                saved_session(uid, "shadow"),
                saved_session(WorkspaceUid(424242), "proj"),
            ],
        );

        assert_eq!(
            picker_rows(stoat),
            ["alpha", "proj"],
            "a workspace whose restore is in flight keeps its row"
        );
    }

    #[test]
    fn entering_a_session_from_a_fresh_launch_drops_the_launch_workspace() {
        let mut harness = Stoat::test();
        let stoat = &mut harness.stoat;

        open_workspace_picker_over(stoat, vec![saved_session(WorkspaceUid(424242), "proj")]);
        workspace_picker_select(stoat);

        let active = stoat.active_workspace();
        assert_eq!(
            (
                stoat.workspaces.len(),
                active.uid,
                active.git_root.as_path()
            ),
            (1, WorkspaceUid(424242), Path::new("/proj")),
            "the session replaces the launch workspace rather than joining it"
        );
    }

    #[test]
    fn entering_a_session_from_a_touched_launch_keeps_the_launch_workspace() {
        let mut harness = Stoat::test();
        harness.edit_focused(0..0, "x");
        let stoat = &mut harness.stoat;

        open_workspace_picker_over(stoat, vec![saved_session(WorkspaceUid(424242), "proj")]);
        stoat
            .workspace_picker
            .as_mut()
            .expect("picker open")
            .select_next();
        workspace_picker_select(stoat);

        assert_eq!(
            (stoat.workspaces.len(), stoat.active_workspace().uid),
            (2, WorkspaceUid(424242)),
            "an edited launch workspace stays open behind the session"
        );
    }

    /// The picker omitted the row while the workspace was untouched, and the
    /// content it took while the picker was up makes it worth keeping.
    #[test]
    fn entering_a_session_keeps_a_launch_workspace_edited_behind_the_picker() {
        let mut harness = Stoat::test();
        open_workspace_picker_over(
            &mut harness.stoat,
            vec![saved_session(WorkspaceUid(424242), "proj")],
        );
        harness.edit_focused(0..0, "x");
        workspace_picker_select(&mut harness.stoat);

        assert_eq!(
            (
                harness.stoat.workspaces.len(),
                harness.stoat.active_workspace().uid
            ),
            (2, WorkspaceUid(424242)),
            "a workspace that took content behind the picker stays open"
        );
    }

    #[test]
    fn entering_a_session_keeps_a_fresh_workspace_whose_restore_is_in_flight() {
        let mut harness = Stoat::test();
        let stoat = &mut harness.stoat;
        let uid = stoat.active_workspace().uid;

        open_workspace_picker_over(
            stoat,
            vec![
                saved_session(uid, "shadow"),
                saved_session(WorkspaceUid(424242), "proj"),
            ],
        );
        stoat
            .workspace_picker
            .as_mut()
            .expect("picker open")
            .select_next();
        workspace_picker_select(stoat);

        assert_eq!(
            (stoat.workspaces.len(), stoat.active_workspace().uid),
            (2, WorkspaceUid(424242)),
            "a workspace a saved session restores into stays open"
        );
    }

    #[test]
    fn switching_from_a_fresh_launch_to_an_open_workspace_drops_the_launch_workspace() {
        let mut harness = Stoat::test();
        let other = harness.create_workspace();
        let stoat = &mut harness.stoat;

        open_workspace_picker_over(stoat, Vec::new());
        workspace_picker_select(stoat);

        assert_eq!(
            (stoat.workspaces.len(), stoat.active_workspace),
            (1, other),
            "the switch leaves only the workspace it entered"
        );
    }

    #[test]
    fn enter_on_a_query_with_no_row_creates_a_session_named_by_it() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().name = "alpha".to_string();
        harness.stoat.active_workspace_mut().git_root = PathBuf::from("/proj");
        let before = harness.stoat.workspaces.len();

        harness.type_action("SwitchWorkspace()");
        harness.type_text("notes");
        harness.type_keys("enter");

        let active = harness.stoat.active_workspace();
        assert_eq!(
            (
                harness.stoat.workspaces.len(),
                active.name.as_str(),
                active.git_root.as_path(),
                active.parent,
                harness.stoat.pending_message.as_deref(),
                harness.stoat.workspace_picker.is_none(),
            ),
            (
                before + 1,
                "notes",
                Path::new("/proj"),
                None,
                Some("created session notes"),
                true,
            ),
            "the query names a new root session rooted where the active one is"
        );
    }

    /// Alt-Enter names a sub-session of the active session's family, the one
    /// the picker's family scope lists. The edit keeps the active workspace
    /// from reading as a fresh launch, which leaves with the create.
    #[test]
    fn alt_enter_on_a_query_creates_a_sub_session_of_the_family() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().name = "alpha".to_string();
        harness.edit_focused(0..0, "x");
        let root = harness.stoat.active_workspace().uid;
        let before = harness.stoat.workspaces.len();

        harness.type_action("SwitchWorkspace()");
        harness.type_text("notes");
        harness.type_keys("alt-enter");

        let active = harness.stoat.active_workspace();
        assert_eq!(
            (
                harness.stoat.workspaces.len(),
                active.name.as_str(),
                active.parent,
                harness.stoat.pending_message.as_deref(),
                harness.stoat.workspace_picker.is_none(),
            ),
            (
                before + 1,
                "notes",
                Some(root),
                Some("created sub-session notes"),
                true,
            ),
            "the query names a sub-session of the active family"
        );
    }

    /// An empty query gives Alt-Enter no name, so the picker stays open.
    #[test]
    fn alt_enter_on_an_empty_query_keeps_the_picker_open() {
        let mut harness = Stoat::test();
        let before = harness.stoat.workspaces.len();

        harness.type_action("SwitchWorkspace()");
        harness.type_keys("alt-enter");

        assert_eq!(
            (
                harness.stoat.workspace_picker.is_some(),
                harness.stoat.workspaces.len(),
            ),
            (true, before),
            "the picker stays open and no session appears"
        );
    }

    /// A rename holds the row's new name in the input, so Alt-Enter leaves it
    /// alone and the rename goes on.
    #[test]
    fn alt_enter_during_a_rename_creates_nothing() {
        let mut harness = Stoat::test();
        let stoat = &mut harness.stoat;
        let input = picker_input(stoat);
        stoat.workspace_picker = Some(WorkspacePicker::new(
            &stoat.workspaces,
            stoat.active_workspace,
            vec![saved_session(WorkspaceUid(7), "proj")],
            input,
        ));
        let before = stoat.workspaces.len();

        workspace_picker_rename_start(stoat);
        replace_picker_text(stoat, "renamed");
        let effect = workspace_picker_create_sub(stoat);

        assert_eq!(
            (
                effect,
                stoat
                    .workspace_picker
                    .as_ref()
                    .map(WorkspacePicker::renaming),
                stoat.workspaces.len(),
            ),
            (UpdateEffect::None, Some(true), before),
            "the rename goes on and no session appears"
        );
    }

    /// A fresh launch workspace leaves with the create, and a sub-session of it
    /// names a parent that no row shows. So the create makes a root session.
    #[test]
    fn alt_enter_from_a_fresh_launch_creates_a_root_session() {
        let mut harness = Stoat::test();
        let launch = harness.stoat.active_workspace;
        open_workspace_picker_over(
            &mut harness.stoat,
            vec![saved_session(WorkspaceUid(424242), "proj")],
        );
        harness.type_text("notes");
        harness.type_keys("alt-enter");

        let stoat = &harness.stoat;
        assert_eq!(
            (
                stoat.workspaces.len(),
                stoat.workspaces.contains_key(launch),
                stoat.active_workspace().name.as_str(),
                stoat.active_workspace().parent,
                stoat.pending_message.as_deref(),
            ),
            (1, false, "notes", None, Some("created session notes")),
            "the created session replaces the launch workspace as a root session"
        );
    }

    /// The other workspace's row is deleted first, so the picker holds no row
    /// and the bare Enter has nothing to create from.
    #[test]
    fn enter_on_an_empty_query_with_no_row_only_closes() {
        let mut harness = Stoat::test();
        harness.type_action("NewWorkspace()");
        harness.type_action("SwitchWorkspace()");
        harness.type_keys("delete");
        assert_eq!(
            harness
                .stoat
                .workspace_picker
                .as_ref()
                .map(|picker| picker.entries().len()),
            Some(0),
            "the picker holds no row"
        );
        let before = (
            harness.stoat.workspaces.len(),
            harness.stoat.active_workspace,
        );

        harness.type_keys("enter");

        assert_eq!(
            (
                (
                    harness.stoat.workspaces.len(),
                    harness.stoat.active_workspace
                ),
                harness.stoat.workspace_picker.is_none()
            ),
            (before, true),
            "the picker closes on the workspace it opened over"
        );
    }

    #[test]
    fn creating_a_session_from_a_fresh_launch_drops_the_launch_workspace() {
        let mut harness = Stoat::test();
        let launch = harness.stoat.active_workspace;
        open_workspace_picker_over(
            &mut harness.stoat,
            vec![saved_session(WorkspaceUid(424242), "proj")],
        );
        harness.type_text("notes");
        harness.type_keys("enter");

        let stoat = &harness.stoat;
        assert_eq!(
            (
                stoat.workspaces.len(),
                stoat.workspaces.contains_key(launch),
                stoat.active_workspace().name.as_str(),
            ),
            (1, false, "notes"),
            "the created session replaces the launch workspace rather than joining it"
        );
    }

    /// The active workspace changed while the picker was up, and the workspace
    /// whose row the picker omitted is still the one that leaves.
    #[test]
    fn entering_a_session_drops_the_omitted_workspace_and_not_the_one_active_at_select() {
        let mut harness = Stoat::test();
        let launch = harness.stoat.active_workspace;
        let other = harness.create_workspace();

        open_workspace_picker_over(
            &mut harness.stoat,
            vec![saved_session(WorkspaceUid(424242), "proj")],
        );
        harness.set_active_workspace(other);
        harness
            .stoat
            .workspace_picker
            .as_mut()
            .expect("picker open")
            .select_next();
        workspace_picker_select(&mut harness.stoat);

        let stoat = &harness.stoat;
        assert_eq!(
            (
                stoat.workspaces.contains_key(launch),
                stoat.workspaces.contains_key(other),
                stoat.active_workspace().uid
            ),
            (false, true, WorkspaceUid(424242)),
            "the launch workspace leaves and the one active at the press stays"
        );
    }

    /// The test scheduler runs blocking work inline and counts it, so the one
    /// call below is the tree walk, and the bundle it wrote extracts whole.
    #[test]
    fn a_dump_reads_the_tree_on_the_blocking_pool() {
        let mut harness = Stoat::test();
        harness.stoat.active_workspace_mut().git_root = PathBuf::from("/ws");
        harness.fake_fs().insert_file("/ws/a.rs", "alpha");
        let before = harness.blocking_calls();

        let at = datetime!(2026-04-19 14:23:11 UTC);
        dump_into(&harness.stoat, "t", at, Path::new("/dumps"));

        let id = DumpId::new("t", at).expect("valid name");
        let archive = Path::new("/dumps").join(id.filename());
        dump::read_archive(&archive, Path::new("/out"), harness.fake_fs().as_ref())
            .expect("the bundle extracts");
        let mut extracted = Vec::new();
        harness
            .fake_fs()
            .read(Path::new("/out/a.rs"), &mut extracted)
            .expect("the file came along");

        assert_eq!(
            (harness.blocking_calls() - before, extracted.as_slice()),
            (1, b"alpha".as_slice()),
            "one blocking call wrote a bundle holding the tree"
        );
    }
}
