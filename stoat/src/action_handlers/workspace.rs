use crate::{
    action_handlers::pane::EditorDisposal,
    app::{Stoat, UpdateEffect},
    host::FsHost,
    input_view::{InputView, SubmitTarget},
    pane::View,
    workspace::{
        registry::{self, RegistryEntry},
        state_path_for, Workspace, WorkspaceId, WorkspaceUid,
    },
    workspace_picker::WorkspacePicker,
};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

pub(super) fn new_workspace(stoat: &mut Stoat) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();
    stoat.save_workspace(stoat.active_workspace);

    let mut ws = Workspace::new(git_root, &stoat.executor, stoat.redraw_notify.clone());
    ws.layout(stoat.size());
    let id = stoat.workspaces.insert(ws);
    stoat.workspaces[id].id = id;
    switch_active_workspace(stoat, id);
    UpdateEffect::Redraw
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

    stoat.save_workspace(stoat.active_workspace);

    let mut ws = Workspace::new(git_root, &stoat.executor, stoat.redraw_notify.clone());
    ws.apply_state(state, &stoat.executor);
    ws.layout(stoat.size());
    let id = stoat.workspaces.insert(ws);
    stoat.workspaces[id].id = id;
    switch_active_workspace(stoat, id);

    // The copy round-trips through to_state/apply_state, so any terminal or
    // commits pane arrives with a dead id. Respawn gives the copy its own shells
    // and lists rather than dangling references into the source workspace.
    super::respawn_terminal_panes(stoat);
    super::respawn_commits_panes(stoat);
    UpdateEffect::Redraw
}

pub(super) fn close_workspace(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.workspaces.len() <= 1 {
        // FIXME: surface a user-visible error once we have a status surface
        // for non-badge errors; tracing-only feedback is invisible in the TUI.
        tracing::warn!("refusing to close last workspace");
        return UpdateEffect::None;
    }

    let active_id = stoat.active_workspace;
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
/// The language servers follow the switch. They close the documents of the
/// workspace left and open the entered one's, through
/// [`crate::lsp::session::mirror_workspace`].
pub(crate) fn switch_active_workspace(stoat: &mut Stoat, next: WorkspaceId) {
    stoat.active_workspace = next;
    let size = stoat.size();
    stoat.active_workspace_mut().layout(size);
    stoat.watch_active_root();

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

/// Delete the session under the workspace picker's selection.
///
/// An inactive row loses the state file and meta sidecar it would restore
/// from. An open background row loses those files and leaves the running
/// instance too. Its shells and runs end, and its hook server stops. The
/// active workspace is refused with a status message, because the picker's
/// own input lives in it and `close_workspace` refuses the last workspace for
/// the same reason.
///
/// An open row's files resolve against the real state directory, so their
/// deletion sits behind [`Stoat::persistence_disabled`], which the test
/// harness sets to keep `$XDG_STATE_HOME` pristine. An inactive row carries
/// its own state path, so that deletion is unconditional.
pub(super) fn workspace_picker_delete(stoat: &mut Stoat) -> UpdateEffect {
    let Some((id, state_path, basename)) = stoat
        .workspace_picker
        .as_ref()
        .and_then(WorkspacePicker::selected_entry)
        .map(|entry| (entry.id, entry.state_path.clone(), entry.basename.clone()))
    else {
        return UpdateEffect::None;
    };

    if id == Some(stoat.active_workspace) {
        stoat.set_status("cannot delete the active workspace");
        return UpdateEffect::Redraw;
    }

    match (id, state_path) {
        (Some(id), _) => {
            stoat.pending_workspace_saves.remove(&id);
            remove_open_session(stoat, id);
            release_workspace(stoat, id);
            stoat.workspaces.remove(id);
        },
        (None, Some(state_path)) => remove_session_files(&*stoat.fs_host, &state_path),
        (None, None) => return UpdateEffect::None,
    }

    if let Some(picker) = stoat.workspace_picker.as_mut() {
        picker.remove_selected();
    }
    stoat.set_status(format!("deleted session {basename}"));
    UpdateEffect::Redraw
}

/// Remove the session files of the open workspace `id`, after stopping every
/// write still queued for it.
///
/// The blocking pool runs a save it already holds whatever happens to its
/// task, and that write puts the files back unless the gate stops it. Retiring
/// the gate first waits for a write in progress and stops every later one.
///
/// The files resolve against the real state directory, so their removal sits
/// behind [`Stoat::persistence_disabled`].
fn remove_open_session(stoat: &Stoat, id: WorkspaceId) {
    let ws = &stoat.workspaces[id];
    ws.save_gate.lock().expect("save gate poisoned").retire();
    if stoat.persistence_disabled {
        return;
    }
    if let Ok(path) = state_path_for(&ws.git_root, ws.uid, &*stoat.fs_host) {
        remove_session_files(&*stoat.fs_host, &path);
    }
}

/// Remove a workspace's state file and its meta sidecar, skipping either if it
/// is already absent and reporting a failed removal to the log.
fn remove_session_files(fs: &dyn FsHost, state_path: &Path) {
    let meta_path = registry::meta_path_for(state_path);
    for target in [state_path, meta_path.as_path()] {
        if fs.exists(target)
            && let Err(err) = fs.remove_file(target)
        {
            tracing::warn!(?target, ?err, "failed to delete workspace file");
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

pub(super) fn workspace_picker_close(stoat: &mut Stoat) -> UpdateEffect {
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
            1,
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
/// already-active workspace or an empty picker just closes the picker.
///
/// An untouched active workspace whose row the picker omitted leaves the
/// instance with the switch, since no row shows it and no save keeps it.
pub(super) fn workspace_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.workspace_picker.take() else {
        return UpdateEffect::None;
    };
    picker.dispose(stoat.active_workspace_mut());

    let Some(entry) = picker.selected_entry() else {
        return UpdateEffect::Redraw;
    };

    match (entry.id, entry.state_path.clone()) {
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
            activate_inactive_workspace(stoat, git_root, uid, name, state_path);
        },
        (None, None) => return UpdateEffect::Redraw,
    }

    // The omit rule judged the workspace active when the picker opened, so the
    // drop names that workspace rather than the one active now. Its freshness
    // is read again, because a workspace that holds something is no orphan.
    let orphan = picker
        .omitted_workspace
        .filter(|&id| id != stoat.active_workspace)
        .filter(|&id| stoat.workspaces.get(id).is_some_and(Workspace::is_fresh));
    if let Some(id) = orphan {
        stoat.pending_workspace_saves.remove(&id);
        release_workspace(stoat, id);
        stoat.workspaces.remove(id);
    }
    UpdateEffect::Redraw
}

/// Bring a persisted-but-closed workspace back into the running instance and
/// switch to it, returning the id of the newly inserted workspace.
///
/// The fresh workspace carries the on-disk `uid` and `name` from the start, so a
/// save triggered before the restore lands (a quit mid-restore) rewrites the
/// same state file rather than forking a new one. The outgoing workspace is
/// saved, focus switches, and a restore spawns to replace the fresh panes with
/// the persisted session.
fn activate_inactive_workspace(
    stoat: &mut Stoat,
    git_root: PathBuf,
    uid: WorkspaceUid,
    name: String,
    state_path: PathBuf,
) -> WorkspaceId {
    stoat.save_workspace(stoat.active_workspace);

    let mut ws = Workspace::new(git_root, &stoat.executor, stoat.redraw_notify.clone());
    ws.uid = uid;
    ws.name = name;
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
        Err(e) => tracing::error!(error = %e, name = %name, "dump failed"),
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
            tracing::error!(error = %e, name = %name, "dump failed");
            return;
        },
    };

    let fs = stoat.fs_host.clone();
    let name = name.to_owned();
    stoat
        .executor
        .spawn_blocking(move || match pending.write(fs.as_ref()) {
            Ok(id) => tracing::info!(id = %id, "dump captured"),
            Err(e) => tracing::error!(error = %e, name = %name, "dump failed"),
        })
        .detach();
}

pub(super) fn rename_workspace(stoat: &mut Stoat, name: &str) {
    stoat.active_workspace_mut().name = name.to_string();
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

            if stoat.env_auto_load
                && stoat.settings.direnv_reload_on_cd.unwrap_or(true)
                && stoat.settings.direnv_load.unwrap_or(true)
            {
                crate::project_env::spawn_load(stoat, ws_id, false);
            }

            let root = stoat.active_workspace().git_root.display().to_string();
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
/// the removal brings the old file back. Superseding them first, under the
/// gate's lock, waits for a write in progress and skips the rest.
fn carry_session(stoat: &mut Stoat, ws_id: WorkspaceId, old: &Path) {
    let ws = &stoat.workspaces[ws_id];
    let Ok(new) = state_path_for(&ws.git_root, ws.uid, &*stoat.fs_host) else {
        return;
    };
    if new == old {
        return;
    }
    ws.save_gate.lock().expect("save gate poisoned").supersede();
    remove_session_files(&*stoat.fs_host, old);
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
        workspace_picker::WorkspacePicker,
    };
    use std::{
        sync::{Arc, Mutex},
        time::UNIX_EPOCH,
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
            1,
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

    fn saved_session(uid: WorkspaceUid, name: &str) -> RegistryEntry {
        RegistryEntry {
            meta: WorkspaceMeta {
                uid,
                name: name.to_string(),
                git_root: PathBuf::from("/proj"),
                buffer_count: 1,
                remote_host: None,
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
            (active.uid, active.git_root.as_path(), active.name.as_str()),
            (uid, Path::new("/proj"), "proj"),
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
            let queued = gate.lock().expect("save gate poisoned").issue();
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
    fn gate_lands_a_write(gate: &Mutex<SaveGate>, stoat: &Stoat) -> bool {
        let seq = gate.lock().expect("save gate poisoned").issue();
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

    /// Closing a workspace or deleting its open row ends the run and the shell
    /// of that workspace, and stops its hook server.
    #[test]
    fn closing_or_deleting_an_open_workspace_ends_its_sessions() {
        let released_after = |delete: bool| {
            let mut harness = Stoat::test();
            harness.allow_host_swap();
            harness
                .stoat
                .set_agent_socket_dir("/stoat-test-never-served".into());
            harness.stoat.set_serve_agent_sockets(true);
            let first = harness.stoat.active_workspace;
            harness.type_action("NewWorkspace()");

            // The terminal covers the run and keeps it alive. A run opened
            // over a terminal pane ends that shell, so the run goes first.
            let run = install_fake_session(&mut harness);
            harness.open_run();
            let shell = install_fake_session(&mut harness);
            super::super::dispatch(&mut harness.stoat, &stoat_action::Terminal);
            let killed = || [run.was_killed(), shell.was_killed()];
            assert_eq!(killed(), [false, false], "both live before the removal");

            let stoat = &mut harness.stoat;
            if delete {
                stoat.active_workspace = first;
                let input = picker_input(stoat);
                let mut picker = WorkspacePicker::new(&stoat.workspaces, first, Vec::new(), input);
                picker.select_next();
                stoat.workspace_picker = Some(picker);
                workspace_picker_delete(stoat);
            } else {
                close_workspace(stoat);
            }
            harness.run_until_parked();
            (
                harness.stoat.workspaces.len(),
                killed(),
                harness.stoat.agent_servers.len(),
            )
        };

        assert_eq!(
            [released_after(false), released_after(true)],
            [(1, [true, true], 0); 2],
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
