//! Per-workspace session state persistence.
//!
//! Each workspace serializes to
//! `<stoat_log::workspace_state_dir()>/<git_root_hash>/<uid>.ron`. When the
//! user passes `--continue`, the binary scans this directory and rehydrates
//! the most-recently-modified file before the first frame renders; a bare
//! launch skips the load so each new session begins in a fresh workspace.
//! Multiple workspaces per git root coexist as sibling files in the same
//! directory. [`crate::workspace::Workspace::is_fresh`] gates the save side
//! so unused fresh workspaces never write a file at all.
//!
//! Coverage is best-effort: see sibling FIXMEs in `multi_buffer.rs`,
//! `review_session.rs`, and `commit_list.rs` for the remaining gaps. Buffer
//! history (dirty content, undo stack, anchor-carrying selections) rehydrates
//! via the op log replay in [`crate::buffer::TextBuffer::from_history`].
//! Anything referencing a live OS resource (PTY-backed `Run`) is out of scope
//! by design.

use crate::{
    buffer_registry::{BufferRegistry, BufferRegistrySnapshot},
    dump::snapshot::ActiveRebaseSnap,
    editor_state::{EditorId, EditorState, EditorStateSnapshot},
    host::FsHost,
    input_history::InputHistory,
    pane::{DockId, DockPanel, FocusTarget, PaneId, PaneTree, View},
    rebase::RebaseState,
    workspace::{Tab, Workspace, WorkspaceUid},
};
use serde::{Deserialize, Serialize};
use slotmap::SlotMap;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};
use stoat_scheduler::Executor;

/// On-disk shape of [`FocusTarget`], preserving the pre-unit-variant wire format
/// so older and newer state.ron files stay mutually readable.
///
/// The live [`FocusTarget::SplitPane`] is a unit variant, but the split pane's
/// id is re-materialized here from [`PaneTree::focus`] on save and discarded on
/// load, since the loaded [`PaneTree`] already carries the real focus.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum FocusTargetSnap {
    SplitPane(PaneId),
    Dock(DockId),
}

/// Versioned on-disk representation of a [`Workspace`]. Fields not covered
/// by this struct are regenerated from defaults on load.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WorkspaceStateV1 {
    /// Stable workspace identifier preserved across saves; doubles as the
    /// on-disk filename. Defaults to zero for pre-field on-disk files; the
    /// loader will treat that as a legacy file (still readable, but its
    /// filename will change the next time it's saved under the new scheme).
    #[serde(default)]
    pub uid: WorkspaceUid,
    pub git_root: PathBuf,
    pub panes: PaneTree,
    pub docks: SlotMap<DockId, DockPanel>,
    pub focus: FocusTargetSnap,
    /// Saved with their pre-shutdown [`EditorId`]s; on load the ids are
    /// remapped to fresh keys in the rehydrated slotmap and pane/dock
    /// `View::Editor` references are rewritten to match.
    pub editors: Vec<(EditorId, EditorStateSnapshot)>,
    pub buffers: BufferRegistrySnapshot,
    pub rebase: Option<RebaseState>,
    pub rebase_active: Option<ActiveRebaseSnap>,
    /// User-facing display name. Empty string on legacy files that predate
    /// the field; restore regenerates a default from `uid` in that case.
    #[serde(default)]
    pub name: String,
    /// Finder scope this workspace last closed in, so `space p` reopens there.
    /// `None` on legacy files that predate the field and whenever no finder has
    /// closed here yet.
    #[serde(default)]
    pub last_finder_scope: Option<String>,
    /// Canonical command lines the palette recalls, oldest first, so history
    /// survives a restart. Empty on legacy files that predate the field.
    #[serde(default)]
    pub palette_history: Vec<String>,
    /// One entry per tab in display order, mirroring the in-memory shape: the
    /// entry at [`Self::active_tab`] is `None` because that tree is saved in
    /// [`Self::panes`]. Empty on legacy files, which restore as a single tab.
    #[serde(default)]
    pub tabs: Vec<Option<PaneTree>>,
    /// Index into [`Self::tabs`] of the tab whose tree is in [`Self::panes`].
    #[serde(default)]
    pub active_tab: usize,
}

/// Resolve the per-git-root directory that holds every workspace persisted
/// against that root. One file per workspace sits in this directory, named
/// by the workspace's [`WorkspaceUid`]. Canonical form of `git_root` is
/// hashed with the stdlib's [`DefaultHasher`] (stable within a Rust release;
/// acceptable here because a hash mismatch just falls back to a fresh session).
pub(crate) fn workspace_dir_for(git_root: &Path, fs: &dyn FsHost) -> io::Result<PathBuf> {
    Ok(anchor_state_dir(
        &stoat_log::workspace_state_dir()?,
        git_root,
        fs,
    ))
}

/// Hash-derived state directory for a single anchor under `state_dir`.
/// Factored so callers (and tests) can supply a custom `state_dir`
/// rather than always going through `stoat_log::workspace_state_dir`.
pub(crate) fn anchor_state_dir(state_dir: &Path, anchor: &Path, fs: &dyn FsHost) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let canon = fs
        .canonicalize(anchor)
        .unwrap_or_else(|_| anchor.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canon.hash(&mut hasher);
    let name = format!("{:016x}", hasher.finish());
    state_dir.join(name)
}

/// Resolve the on-disk state file path for a specific workspace.
pub(crate) fn state_path_for(
    git_root: &Path,
    uid: WorkspaceUid,
    fs: &dyn FsHost,
) -> io::Result<PathBuf> {
    Ok(workspace_dir_for(git_root, fs)?.join(format!("{uid}.ron")))
}

/// List every persisted workspace file for a git root, newest first by
/// filesystem mtime. Returns an empty vec (not an error) if the directory
/// does not exist.
pub(crate) fn list_workspace_files(git_root: &Path, fs: &dyn FsHost) -> io::Result<Vec<PathBuf>> {
    list_ron_files_by_mtime_desc(&workspace_dir_for(git_root, fs)?, fs)
}

/// Walk ancestors of `cwd` (cwd itself first) for any directory whose
/// workspace state directory contains persisted `.ron` files, and return
/// the ancestor whose newest file has the highest mtime across all
/// candidates. Returns `None` when no ancestor has any persisted state.
///
/// Backs the binary's `--resume` flag: workspaces are tracked per anchor
/// directory, and `--resume` cascades up so a session run from
/// `~/foo/bar/baz/bang` reopens whichever ancestor's state is most
/// recent. cwd-first iteration means a tie at the same mtime resolves
/// to the deepest ancestor, which is the natural "most specific match"
/// when multiple state files were saved at the same instant.
pub fn find_resume_anchor(cwd: &Path, fs: &dyn FsHost) -> io::Result<Option<PathBuf>> {
    let state_dir = stoat_log::workspace_state_dir()?;
    find_resume_anchor_in(&state_dir, cwd, fs)
}

fn find_resume_anchor_in(
    state_dir: &Path,
    cwd: &Path,
    fs: &dyn FsHost,
) -> io::Result<Option<PathBuf>> {
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for anc in cwd.ancestors() {
        let dir = anchor_state_dir(state_dir, anc, fs);
        if !fs.exists(&dir) {
            continue;
        }
        let mut newest: Option<std::time::SystemTime> = None;
        for entry in fs.list_dir(&dir)? {
            let path = dir.join(entry.name.as_str());
            if path.extension().and_then(|s| s.to_str()) != Some("ron") {
                continue;
            }
            let mtime = fs
                .metadata(&path)
                .ok()
                .flatten()
                .map(|m| m.modified)
                .unwrap_or(UNIX_EPOCH);
            newest = Some(newest.map_or(mtime, |prev| prev.max(mtime)));
        }
        if let Some(mtime) = newest {
            match &best {
                Some((_, prev_mtime)) if *prev_mtime >= mtime => {},
                _ => best = Some((anc.to_path_buf(), mtime)),
            }
        }
    }
    Ok(best.map(|(p, _)| p))
}

/// Underlying directory scan for [`list_workspace_files`]. Factored so tests
/// can exercise it against a tempdir without touching the real XDG path.
/// Entries whose metadata cannot be read are treated as unix-epoch-old so
/// they sort to the bottom rather than dropping out silently.
fn list_ron_files_by_mtime_desc(dir: &Path, fs: &dyn FsHost) -> io::Result<Vec<PathBuf>> {
    if !fs.exists(dir) {
        return Ok(Vec::new());
    }
    let mut entries: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in fs.list_dir(dir)? {
        let path = dir.join(entry.name.as_str());
        if path.extension().and_then(|s| s.to_str()) != Some("ron") {
            continue;
        }
        let mtime = fs
            .metadata(&path)
            .ok()
            .flatten()
            .map(|m| m.modified)
            .unwrap_or(UNIX_EPOCH);
        entries.push((path, mtime));
    }
    entries.sort_by_key(|b| std::cmp::Reverse(b.1));
    Ok(entries.into_iter().map(|(p, _)| p).collect())
}

impl Workspace {
    /// Build a serializable snapshot of everything the workspace currently
    /// knows how to round-trip.
    pub(crate) fn to_state(&self) -> WorkspaceStateV1 {
        let editors: Vec<(EditorId, EditorStateSnapshot)> = self
            .editors
            .iter()
            .map(|(id, state)| (id, state.snapshot()))
            .collect();

        let rebase_active = self
            .rebase_active
            .as_ref()
            .map(|active| ActiveRebaseSnap::from_active(active).snap);

        WorkspaceStateV1 {
            uid: self.uid,
            git_root: self.git_root.clone(),
            panes: clone_pane_tree(&self.panes),
            docks: clone_docks(&self.docks),
            focus: match self.focus {
                FocusTarget::SplitPane => FocusTargetSnap::SplitPane(self.panes.focus()),
                FocusTarget::Dock(id) => FocusTargetSnap::Dock(id),
            },
            editors,
            buffers: self.buffers.snapshot(),
            rebase: self.rebase.clone(),
            rebase_active,
            name: self.name.clone(),
            last_finder_scope: self.last_finder_scope.clone(),
            palette_history: self.palette_history.entries().to_vec(),
            tabs: self
                .tabs
                .iter()
                .map(|tab| tab.parked.as_ref().map(clone_pane_tree))
                .collect(),
            active_tab: self.active_tab,
        }
    }

    /// Serialize the current workspace state to RON and write it atomically
    /// to `path`. Parent directory is created if missing.
    pub(crate) fn save_state(&self, path: &Path, fs: &dyn FsHost) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs.create_dir_all(parent)?;
        }
        let state = self.to_state();
        let body = ron::ser::to_string_pretty(&state, ron::ser::PrettyConfig::default())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let tmp = path.with_extension("ron.tmp");
        fs.write(&tmp, body.as_bytes())?;
        fs.rename(&tmp, path)?;

        let meta = super::registry::WorkspaceMeta {
            uid: self.uid,
            name: self.name.clone(),
            git_root: self.git_root.clone(),
            buffer_count: self.buffers.len(),
        };
        super::registry::write_meta(&meta, path, fs)?;
        Ok(())
    }

    /// Replace `self` with the persisted state at `path`. Returns an error if
    /// the file cannot be read or parsed.
    ///
    /// The synchronous reference used by the persistence tests. Production
    /// `--continue` restores run off the main thread through
    /// [`read_restore_parts`] and [`Self::install_restored`], so this stays
    /// test-only.
    #[cfg(test)]
    pub(crate) fn restore_state(
        &mut self,
        path: &Path,
        fs: &dyn FsHost,
        executor: &Executor,
    ) -> io::Result<()> {
        let mut buf = Vec::new();
        fs.read(path, &mut buf)?;
        let body = String::from_utf8(buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let state: WorkspaceStateV1 = ron::from_str(&body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        self.apply_state(state, executor);
        Ok(())
    }

    pub(crate) fn apply_state(&mut self, mut state: WorkspaceStateV1, executor: &Executor) {
        self.buffers
            .restore_from(std::mem::take(&mut state.buffers));
        self.install_restored_parts(state, executor);
    }

    /// Install a pre-built buffer `registry` and the remaining restored `state`.
    ///
    /// The async `--continue` path builds the registry off the main thread with
    /// [`read_restore_parts`] and hands it here, where only the cheap remainder
    /// runs. `state.buffers` is ignored in favor of `registry`.
    pub(crate) fn install_restored(
        &mut self,
        registry: BufferRegistry,
        state: WorkspaceStateV1,
        executor: &Executor,
    ) {
        self.buffers = registry;
        self.install_restored_parts(state, executor);
    }

    /// Rehydrate editors against the already-restored buffers, remap pane and
    /// dock editor views to the fresh editor ids, and set the workspace fields.
    ///
    /// Requires `self.buffers` to already hold the restored registry.
    fn install_restored_parts(&mut self, state: WorkspaceStateV1, executor: &Executor) {
        let mut editors: SlotMap<EditorId, EditorState> = SlotMap::with_key();
        let mut editor_id_map: HashMap<EditorId, EditorId> = HashMap::new();
        for (old_id, snap) in state.editors {
            let Some(buffer) = self.buffers.get(snap.buffer_id) else {
                tracing::warn!(
                    buffer_id = ?snap.buffer_id,
                    "buffer missing after restore; dropping editor"
                );
                continue;
            };
            let new_id = editors.insert(EditorState::restore(snap, buffer, executor.clone()));
            editor_id_map.insert(old_id, new_id);
        }

        let mut panes = state.panes;
        rehydrate_tree(&mut panes, &editor_id_map);

        // A parked tree carries the same stale editor ids and detached panes as
        // the active one, so it needs the identical treatment. Skipping it would
        // leave the tab pointing at editors that no longer exist.
        let mut parked: Vec<Option<PaneTree>> = state.tabs;
        for tree in parked.iter_mut().flatten() {
            rehydrate_tree(tree, &editor_id_map);
        }

        let mut docks = state.docks;
        remap_editor_views_in_docks(&mut docks, &editor_id_map);
        sweep_stale_views_in_docks(&mut docks);

        let focus = match state.focus {
            FocusTargetSnap::Dock(id) if docks.contains_key(id) => FocusTarget::Dock(id),
            // A dock that no longer exists, or any split-pane snapshot, resolves
            // to the live split-pane focus carried by the rehydrated pane tree.
            FocusTargetSnap::Dock(_) | FocusTargetSnap::SplitPane(_) => FocusTarget::SplitPane,
        };

        self.panes = panes;
        self.docks = docks;
        self.focus = focus;
        self.editors = editors;
        self.uid = state.uid;
        self.git_root = state.git_root;
        self.rebase = state.rebase;
        self.rebase_active = state.rebase_active.map(ActiveRebaseSnap::into_active);
        self.name = if state.name.is_empty() {
            super::name::default_workspace_name(state.uid)
        } else {
            state.name
        };
        self.last_finder_scope = state.last_finder_scope;
        self.palette_history = InputHistory::from_entries(state.palette_history);

        // Exactly the active slot may be empty, since that tree is in `panes`.
        let coherent = parked.len() > 1
            && state.active_tab < parked.len()
            && parked
                .iter()
                .enumerate()
                .all(|(i, slot)| (i == state.active_tab) == slot.is_none());
        if coherent {
            self.tabs = parked.into_iter().map(|parked| Tab { parked }).collect();
            self.active_tab = state.active_tab;
        } else {
            // The file's tab shape disagrees with itself, and its intent is not
            // recoverable. One tab holding the restored active tree is always a
            // coherent workspace, and is what a legacy file means anyway.
            self.tabs = vec![Tab { parked: None }];
            self.active_tab = 0;
        }
        // Which tab to toggle back to is session state, not layout.
        self.last_tab = None;
    }
}

/// Bring a restored pane tree back into a usable state against the freshly
/// built editor slotmap.
///
/// Aux windows do not survive a restart, so every detached pane reattaches as a
/// split first. The remap and sweep passes walk the split tree, so a pane left
/// windowed would be skipped and orphaned in the slotmap.
fn rehydrate_tree(tree: &mut PaneTree, editor_id_map: &HashMap<EditorId, EditorId>) {
    for (id, _window) in tree.windowed_panes() {
        tree.attach(id);
    }
    remap_editor_views_in_panes(tree, editor_id_map);
    sweep_stale_views_in_panes(tree);
}

/// Read and parse a persisted workspace state, replaying every buffer's op log
/// into a standalone [`BufferRegistry`].
///
/// The heavy half of a restore, split from [`Workspace::restore_state`] so the
/// async `--continue` path can run it on the blocking pool. Returns the built
/// registry and the remaining state, whose `buffers` field is consumed. Pair it
/// with [`Workspace::install_restored`] to finish the restore.
pub(crate) fn read_restore_parts(
    path: &Path,
    fs: &dyn FsHost,
) -> io::Result<(BufferRegistry, WorkspaceStateV1)> {
    let mut buf = Vec::new();
    fs.read(path, &mut buf)?;
    let body = String::from_utf8(buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut state: WorkspaceStateV1 = ron::from_str(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut registry = BufferRegistry::new();
    registry.restore_from(std::mem::take(&mut state.buffers));
    Ok((registry, state))
}

fn remap_editor_views_in_panes(panes: &mut PaneTree, remap: &HashMap<EditorId, EditorId>) {
    for id in panes.split_pane_ids() {
        if let View::Editor(old) = panes.pane(id).view
            && let Some(&new) = remap.get(&old)
        {
            panes.pane_mut(id).view = View::Editor(new);
        }
    }
}

fn remap_editor_views_in_docks(
    docks: &mut SlotMap<DockId, DockPanel>,
    remap: &HashMap<EditorId, EditorId>,
) {
    for dock in docks.values_mut() {
        if let View::Editor(old) = dock.view
            && let Some(&new) = remap.get(&old)
        {
            dock.view = View::Editor(new);
        }
    }
}

fn sweep_stale_views_in_panes(panes: &mut PaneTree) {
    for id in panes.split_pane_ids() {
        let replacement = stale_replacement(&panes.pane(id).view);
        if let Some(view) = replacement {
            panes.pane_mut(id).view = view;
        }
    }
}

fn sweep_stale_views_in_docks(docks: &mut SlotMap<DockId, DockPanel>) {
    for dock in docks.values_mut() {
        if let Some(view) = stale_replacement(&dock.view) {
            dock.view = view;
        }
    }
}

fn stale_replacement(view: &View) -> Option<View> {
    match view {
        View::Run(_) => Some(View::Label("Terminal (closed)".into())),
        View::Agent(_) => Some(View::Label("Agent (closed)".into())),
        // Terminal panes survive the sweep with a dead id. The app respawns a
        // fresh shell for each after restore. See action_handlers::terminal.
        View::Terminal(_) | View::Label(_) | View::Editor(_) => None,
    }
}

/// Workaround for `PaneTree` not being `Clone`: deserialize a freshly
/// serialized copy. Only used on the save path where cloning is cheap and
/// avoids leaking layout internals out of `pane.rs`.
fn clone_pane_tree(tree: &PaneTree) -> PaneTree {
    let body = ron::ser::to_string(tree).expect("pane tree is always serializable");
    ron::from_str(&body).expect("pane tree round-trips through its own serde impl")
}

fn clone_docks(docks: &SlotMap<DockId, DockPanel>) -> SlotMap<DockId, DockPanel> {
    let body = ron::ser::to_string(docks).expect("dock slotmap is always serializable");
    ron::from_str(&body).expect("dock slotmap round-trips through serde")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        host::FakeFs,
        pane::{Axis, DockSide, DockVisibility, Placement},
    };
    use std::{sync::Arc, time::Duration};
    use stoat_scheduler::TestScheduler;

    fn executor() -> Executor {
        Arc::new(TestScheduler::new()).executor()
    }

    /// [`WorkspaceStateV1`] as it stood before tabs, for writing a file that
    /// genuinely lacks the fields rather than one edited to look like it does.
    #[derive(Serialize)]
    struct LegacyState {
        uid: WorkspaceUid,
        git_root: PathBuf,
        panes: PaneTree,
        docks: SlotMap<DockId, DockPanel>,
        focus: FocusTargetSnap,
        editors: Vec<(EditorId, EditorStateSnapshot)>,
        buffers: BufferRegistrySnapshot,
        rebase: Option<RebaseState>,
        rebase_active: Option<ActiveRebaseSnap>,
        name: String,
        last_finder_scope: Option<String>,
        palette_history: Vec<String>,
    }

    impl From<WorkspaceStateV1> for LegacyState {
        fn from(s: WorkspaceStateV1) -> Self {
            Self {
                uid: s.uid,
                git_root: s.git_root,
                panes: s.panes,
                docks: s.docks,
                focus: s.focus,
                editors: s.editors,
                buffers: s.buffers,
                rebase: s.rebase,
                rebase_active: s.rebase_active,
                name: s.name,
                last_finder_scope: s.last_finder_scope,
                palette_history: s.palette_history,
            }
        }
    }

    fn new_laid_out_workspace(git_root: PathBuf, exec: &Executor) -> Workspace {
        let mut ws = Workspace::new(git_root, exec);
        ws.layout(ratatui::layout::Rect::new(0, 0, 120, 40));
        ws
    }

    /// The editor a parked tab shows, or `None` when that tab's focused pane
    /// shows something else.
    fn parked_focus_editor(ws: &Workspace, tab: usize) -> Option<EditorId> {
        let tree = ws.tabs[tab].parked.as_ref()?;
        match tree.pane(tree.focus()).view {
            View::Editor(id) => Some(id),
            _ => None,
        }
    }

    /// Asserting the tab count alone would pass against a restore that dropped
    /// the remap, so this follows the parked tab's view through to an editor
    /// that must actually exist in the rebuilt slotmap, holding the buffer it
    /// had before the save.
    #[test]
    fn round_trip_restores_a_parked_tab_with_remapped_editors() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/tabs");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);

        // Restoring inserts editors into a fresh slotmap, which hands back the
        // very same keys unless the original recycled a slot. Freeing one first
        // makes `editor_a` land in it at a bumped version, so its saved id
        // cannot survive the restore unchanged and a tree that skipped the
        // remap would point at nothing rather than working by luck.
        let (id_hole, buf_hole) = ws.buffers.open(&ws_dir.join("hole.txt"), "hole\n");
        let hole = ws
            .editors
            .insert(EditorState::new(id_hole, buf_hole, exec.clone()));
        ws.editors.remove(hole);

        let (id_a, buf_a) = ws.buffers.open(&ws_dir.join("a.txt"), "alpha\n");
        let editor_a = ws
            .editors
            .insert(EditorState::new(id_a, buf_a, exec.clone()));

        let root = ws.panes.focus();
        ws.panes.pane_mut(root).view = View::Editor(editor_a);

        // A second tab, leaving the first parked behind it.
        ws.new_tab(&exec);
        assert_eq!(ws.active_tab, 1);
        assert_eq!(parked_focus_editor(&ws, 0), Some(editor_a));

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(fresh.tabs.len(), 2, "both tabs survive");
        assert_eq!(fresh.active_tab, 1);
        assert!(
            fresh.tabs[1].parked.is_none(),
            "the active tab's tree is in ws.panes, not parked"
        );
        assert_eq!(fresh.last_tab, None, "the toggle target does not persist");

        let restored = parked_focus_editor(&fresh, 0).expect("the parked tab shows an editor");
        assert_ne!(
            restored, editor_a,
            "the gap did its job: a stale id would not have resolved by luck"
        );
        let state = fresh
            .editors
            .get(restored)
            .expect("the parked tab's editor id resolves after the remap");
        assert_eq!(
            fresh.buffers.path_for(state.buffer_id),
            Some(ws_dir.join("a.txt").as_path()),
            "and still holds the buffer it was saved with"
        );
    }

    /// A pre-tabs stoat wrote neither field, so both have to default rather
    /// than fail the whole parse and lose the session.
    #[test]
    fn a_state_file_without_tab_fields_restores_one_tab() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/legacy");
        let exec = executor();

        let ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let state_path = ws_dir.join("state.ron");

        // Serialized through a struct that never had the fields, which is a
        // truer stand-in for an old file than editing a current one.
        let body = ron::ser::to_string(&LegacyState::from(ws.to_state())).unwrap();
        assert!(!body.contains("active_tab"), "no tab fields on the wire");
        fake.write(&state_path, body.as_bytes()).unwrap();

        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec);
        fresh
            .restore_state(&state_path, &fake, &exec)
            .expect("a legacy file still parses");

        assert_eq!(fresh.tabs.len(), 1);
        assert_eq!(fresh.active_tab, 0);
        assert!(fresh.tabs[0].parked.is_none());
    }

    #[test]
    fn round_trip_preserves_pane_tree_and_focus() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let file_a = ws_dir.join("a.txt");
        let file_b = ws_dir.join("b.txt");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (id_a, buf_a) = ws.buffers.open(&file_a, "alpha\n");
        let (id_b, buf_b) = ws.buffers.open(&file_b, "beta\n");
        let editor_a = ws
            .editors
            .insert(EditorState::new(id_a, buf_a, exec.clone()));
        let editor_b = ws
            .editors
            .insert(EditorState::new(id_b, buf_b, exec.clone()));
        ws.editors[editor_a].scroll_row = 7;
        ws.editors[editor_b].scroll_row = 3;

        let root = ws.panes.focus();
        ws.panes.pane_mut(root).view = View::Editor(editor_a);
        let right = ws.panes.split(Axis::Vertical);
        ws.panes.pane_mut(right).view = View::Editor(editor_b);
        ws.focus = FocusTarget::SplitPane;

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(fresh.git_root, ws_dir);
        assert_eq!(fresh.panes.pane_count(), 2);
        // Three editors: the two file-backed editors we inserted plus the
        // scratch-buffer editor that `Workspace::new` creates by default. The
        // scratch editor is preserved because its buffer's op log round-trips.
        assert_eq!(fresh.editors.len(), 3);

        let FocusTarget::SplitPane = fresh.focus else {
            panic!("focus should be a split pane");
        };
        let focused = fresh.panes.focus();
        let View::Editor(focused_editor) = fresh.panes.pane(focused).view else {
            panic!("focused pane should host an editor");
        };
        assert_eq!(fresh.editors[focused_editor].scroll_row, 3);

        let mut scrolls: Vec<u32> = fresh.editors.values().map(|e| e.scroll_row).collect();
        scrolls.sort();
        assert_eq!(scrolls, vec![0, 3, 7]);

        let mut path_backed: Vec<PathBuf> = fresh
            .editors
            .values()
            .filter_map(|e| fresh.buffers.path_for(e.buffer_id).map(|p| p.to_path_buf()))
            .collect();
        path_backed.sort();
        let mut expected = vec![file_a.clone(), file_b.clone()];
        expected.sort();
        assert_eq!(path_backed, expected);
    }

    #[test]
    fn stale_run_views_collapse_to_labels() {
        use crate::run::RunId;
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let stale_run = RunId::default();

        let root = ws.panes.focus();
        ws.panes.pane_mut(root).view = View::Run(stale_run);
        let second = ws.panes.split(Axis::Vertical);
        ws.panes.pane_mut(second).view = View::Run(stale_run);

        let dock_id = ws.docks.insert(DockPanel {
            view: View::Run(stale_run),
            side: DockSide::Right,
            visibility: DockVisibility::Open { width: 40 },
            default_width: 40,
            area: Default::default(),
        });
        let _ = dock_id;

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        for id in fresh.panes.split_pane_ids() {
            match &fresh.panes.pane(id).view {
                View::Label(s) => {
                    assert!(
                        s.contains("closed"),
                        "expected placeholder label, got {s:?}"
                    );
                },
                other => panic!("stale view should become a label, got {other:?}"),
            }
        }
        for dock in fresh.docks.values() {
            assert!(matches!(&dock.view, View::Label(_)));
        }
    }

    #[test]
    fn terminal_views_survive_restore_for_respawn() {
        use crate::term_session::TermId;
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let dead_term = TermId::default();

        let root = ws.panes.focus();
        ws.panes.pane_mut(root).view = View::Terminal(dead_term);
        ws.docks.insert(DockPanel {
            view: View::Terminal(dead_term),
            side: DockSide::Right,
            visibility: DockVisibility::Open { width: 40 },
            default_width: 40,
            area: Default::default(),
        });

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        for id in fresh.panes.split_pane_ids() {
            assert!(
                matches!(fresh.panes.pane(id).view, View::Terminal(_)),
                "terminal pane must survive restore un-swept for respawn",
            );
        }
        for dock in fresh.docks.values() {
            assert!(
                matches!(dock.view, View::Terminal(_)),
                "terminal dock must survive restore un-swept for respawn",
            );
        }
    }

    #[test]
    fn placement_and_layout_rebuild_on_restore() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.panes.split(Axis::Horizontal);
        ws.panes.split(Axis::Vertical);
        let count_before = ws.panes.pane_count();

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(fresh.panes.pane_count(), count_before);
        for id in fresh.panes.split_pane_ids() {
            assert_eq!(fresh.panes.pane(id).placement, Placement::Split);
        }
    }

    #[test]
    fn detached_pane_reattaches_on_restore() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let detached = ws.panes.split(Axis::Vertical);
        assert!(ws.panes.detach(detached, 1));
        let count_before = ws.panes.pane_count();
        assert_eq!(ws.panes.windowed_panes().len(), 1);

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(fresh.panes.pane_count(), count_before);
        assert!(
            fresh.panes.windowed_panes().is_empty(),
            "no pane stays windowed after restart"
        );
        assert_eq!(fresh.panes.split_pane_ids().len(), count_before);
    }

    fn buffer_text(ws: &Workspace, id: crate::buffer::BufferId) -> String {
        let buffer = ws.buffers.get(id).expect("buffer missing");
        let guard = buffer.read().expect("buffer poisoned");
        guard.rope().to_string()
    }

    fn buffer_is_dirty(ws: &Workspace, id: crate::buffer::BufferId) -> bool {
        let buffer = ws.buffers.get(id).expect("buffer missing");
        let guard = buffer.read().expect("buffer poisoned");
        guard.dirty
    }

    #[test]
    fn dirty_buffer_content_round_trips() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let file = ws_dir.join("scratch.txt");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (id, buffer) = ws.buffers.open(&file, "hello\n");
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(5..5, ", world");
            guard.edit(12..12, "!");
        }
        let expected_text = buffer_text(&ws, id);
        assert_eq!(expected_text, "hello, world!\n");
        assert!(buffer_is_dirty(&ws, id));

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(buffer_text(&fresh, id), expected_text);
        assert!(buffer_is_dirty(&fresh, id));
    }

    #[test]
    fn undo_stack_survives_restart() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let file = ws_dir.join("count.txt");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (id, buffer) = ws.buffers.open(&file, "one\n");
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(3..3, " two");
            guard.edit(7..7, " three");
        }
        assert_eq!(buffer_text(&ws, id), "one two three\n");

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        let restored = fresh.buffers.get(id).expect("buffer missing");
        let mut guard = restored.write().expect("buffer poisoned");
        assert!(guard.undo().is_some(), "undo should succeed after restart");
        assert_eq!(guard.rope().to_string(), "one two\n");
        assert!(guard.undo().is_some(), "second undo should also succeed");
        assert_eq!(guard.rope().to_string(), "one\n");
    }

    #[test]
    fn selections_round_trip_through_anchors() {
        use crate::{multi_buffer::MultiBuffer, selection::SelectionsCollection};
        use stoat_text::{Bias, SelectionGoal};

        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let file = ws_dir.join("code.txt");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (id, buffer) = ws.buffers.open(&file, "abcdefghij\n");
        let editor_id = ws
            .editors
            .insert(EditorState::new(id, buffer.clone(), exec.clone()));

        let multi = MultiBuffer::singleton(id, buffer.clone());
        let multi_snap = multi.snapshot();
        let mut selections = SelectionsCollection::new();
        selections.insert_cursor(
            multi_snap.anchor_at(3, Bias::Right),
            SelectionGoal::None,
            &multi_snap,
        );
        selections.insert_cursor(
            multi_snap.anchor_at(7, Bias::Right),
            SelectionGoal::None,
            &multi_snap,
        );
        ws.editors[editor_id].selections = selections;

        let offsets_before: Vec<usize> = ws.editors[editor_id]
            .selections
            .all_anchors()
            .iter()
            .map(|s| multi_snap.resolve_anchor(&s.start))
            .collect();

        let root = ws.panes.focus();
        ws.panes.pane_mut(root).view = View::Editor(editor_id);

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        let restored_editor_id = fresh
            .panes
            .split_pane_ids()
            .into_iter()
            .find_map(|pid| match fresh.panes.pane(pid).view {
                View::Editor(eid) => Some(eid),
                _ => None,
            })
            .expect("pane with editor view");
        let restored_bid = fresh.editors[restored_editor_id].buffer_id;
        let restored_buffer = fresh.buffers.get(restored_bid).expect("buffer missing");
        let restored_multi = MultiBuffer::singleton(restored_bid, restored_buffer);
        let restored_snap = restored_multi.snapshot();
        let offsets_after: Vec<usize> = fresh.editors[restored_editor_id]
            .selections
            .all_anchors()
            .iter()
            .map(|s| restored_snap.resolve_anchor(&s.start))
            .collect();
        assert_eq!(offsets_after, offsets_before);
    }

    #[test]
    fn clean_buffer_has_single_insert_op() {
        use crate::buffer::BufferOp;

        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let file = ws_dir.join("untouched.txt");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (id, _) = ws.buffers.open(&file, "hello world\n");
        assert!(!buffer_is_dirty(&ws, id));

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(buffer_text(&fresh, id), "hello world\n");
        assert!(!buffer_is_dirty(&fresh, id));

        let buffer = fresh.buffers.get(id).expect("buffer missing");
        let guard = buffer.read().expect("buffer poisoned");
        let history = guard.history();
        assert_eq!(history.ops.len(), 1);
        match &history.ops[0] {
            BufferOp::Edit { old, text } => {
                assert_eq!(*old, 0..0);
                assert_eq!(text, "hello world\n");
            },
            other => panic!("expected single Edit op, got {other:?}"),
        }
    }

    #[test]
    fn scratch_buffer_history_round_trips() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (scratch_id, scratch_buf) = ws.buffers.new_scratch_unseeded();
        {
            let mut guard = scratch_buf.write().expect("buffer poisoned");
            guard.edit(0..0, "notes\n");
            guard.edit(6..6, "more\n");
        }
        assert_eq!(buffer_text(&ws, scratch_id), "notes\nmore\n");

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(buffer_text(&fresh, scratch_id), "notes\nmore\n");
        let buffer = fresh.buffers.get(scratch_id).expect("scratch lost");
        let guard = buffer.read().expect("buffer poisoned");
        assert!(guard.dirty);
    }

    #[test]
    fn list_ron_files_sorts_newest_first() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let older = ws_dir.join("aaaa.ron");
        let newer = ws_dir.join("bbbb.ron");
        fake.insert_file(&older, "old");
        fake.insert_file(&newer, "new");

        let listed = list_ron_files_by_mtime_desc(&ws_dir, &fake).unwrap();
        assert_eq!(listed, vec![newer, older]);
    }

    #[test]
    fn list_ron_files_ignores_non_ron_entries() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        fake.insert_file(ws_dir.join("ok.ron"), "");
        fake.insert_file(ws_dir.join("skip.txt"), "");
        fake.insert_dir(ws_dir.join("subdir"));

        let listed = list_ron_files_by_mtime_desc(&ws_dir, &fake).unwrap();
        assert_eq!(listed, vec![ws_dir.join("ok.ron")]);
    }

    #[test]
    fn list_ron_files_missing_dir_returns_empty() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let missing = ws_dir.join("nope");
        assert!(list_ron_files_by_mtime_desc(&missing, &fake)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn uid_round_trips_through_save_and_restore() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let scheduler = Arc::new(TestScheduler::new());
        let exec = scheduler.executor();

        let ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let original_uid = ws.uid;

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        scheduler.advance_clock(Duration::from_nanos(1));
        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        assert_ne!(fresh.uid, original_uid, "new workspaces get distinct uids");
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(fresh.uid, original_uid);
    }

    #[test]
    fn user_name_round_trips_through_save_and_restore() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.name = "my workspace".to_string();
        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(fresh.name, "my workspace");
    }

    #[test]
    fn last_finder_scope_round_trips_through_save_and_restore() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.last_finder_scope = Some("modified".to_string());
        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        assert_eq!(
            fresh.last_finder_scope, None,
            "fresh workspace remembers nothing"
        );
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(fresh.last_finder_scope, Some("modified".to_string()));
    }

    #[test]
    fn palette_history_round_trips_through_save_and_restore() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.palette_history = InputHistory::from_entries(vec!["cd ~/work".into(), "w".into()]);
        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        assert!(
            fresh.palette_history.entries().is_empty(),
            "fresh workspace remembers nothing"
        );
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(fresh.palette_history.entries().to_vec(), ["cd ~/work", "w"]);
    }

    #[test]
    fn legacy_state_without_palette_history_loads_empty() {
        let exec = executor();
        let ws = new_laid_out_workspace(PathBuf::from("/test"), &exec);

        let body =
            ron::ser::to_string_pretty(&ws.to_state(), ron::ser::PrettyConfig::default()).unwrap();
        let legacy: String = body
            .lines()
            .filter(|line| !line.contains("palette_history"))
            .collect::<Vec<_>>()
            .join("\n");

        let state: WorkspaceStateV1 =
            ron::from_str(&legacy).expect("a file without palette_history still loads");
        assert!(state.palette_history.is_empty());
    }

    #[test]
    fn legacy_empty_name_regenerates_default_from_uid() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.name = String::new();
        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(
            fresh.name,
            crate::workspace::name::default_workspace_name(ws.uid)
        );
    }

    #[test]
    fn multiple_saves_with_same_uid_overwrite() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let path = ws_dir.join(format!("{}.ron", ws.uid));
        ws.save_state(&path, &fake).unwrap();
        ws.save_state(&path, &fake).unwrap();

        let listed = list_ron_files_by_mtime_desc(&ws_dir, &fake).unwrap();
        assert_eq!(listed, vec![path]);
    }

    #[test]
    fn different_uids_sit_side_by_side() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let scheduler = Arc::new(TestScheduler::new());
        let exec = scheduler.executor();

        let ws_a = new_laid_out_workspace(ws_dir.clone(), &exec);
        scheduler.advance_clock(Duration::from_nanos(1));
        let ws_b = new_laid_out_workspace(ws_dir.clone(), &exec);
        assert_ne!(ws_a.uid, ws_b.uid);

        let path_a = ws_dir.join(format!("{}.ron", ws_a.uid));
        let path_b = ws_dir.join(format!("{}.ron", ws_b.uid));
        ws_a.save_state(&path_a, &fake).unwrap();
        ws_b.save_state(&path_b, &fake).unwrap();

        let listed = list_ron_files_by_mtime_desc(&ws_dir, &fake).unwrap();
        assert_eq!(listed.len(), 2);
    }

    fn write_anchor_state(state_dir: &Path, anchor: &Path, fake: &FakeFs, name: &str) -> PathBuf {
        let dir = anchor_state_dir(state_dir, anchor, fake);
        let path = dir.join(name);
        fake.insert_file(&path, "");
        path
    }

    #[test]
    fn find_resume_anchor_no_state_returns_none() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn find_resume_anchor_picks_only_ancestor_with_state() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        let anchor = PathBuf::from("/foo");
        write_anchor_state(&state_dir, &anchor, &fake, "ws.ron");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, Some(anchor));
    }

    #[test]
    fn find_resume_anchor_picks_cwd_when_only_cwd_has_state() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        write_anchor_state(&state_dir, &cwd, &fake, "ws.ron");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, Some(cwd));
    }

    #[test]
    fn find_resume_anchor_prefers_more_recent_anchor() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        write_anchor_state(&state_dir, &PathBuf::from("/foo"), &fake, "old.ron");
        write_anchor_state(&state_dir, &cwd, &fake, "new.ron");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, Some(cwd));
    }

    #[test]
    fn find_resume_anchor_prefers_parent_when_parent_newer() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        write_anchor_state(&state_dir, &cwd, &fake, "old.ron");
        let parent = PathBuf::from("/foo");
        write_anchor_state(&state_dir, &parent, &fake, "new.ron");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, Some(parent));
    }

    #[test]
    fn find_resume_anchor_skips_non_ron_files() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar");
        let parent = PathBuf::from("/foo");
        let parent_dir = anchor_state_dir(&state_dir, &parent, &fake);
        fake.insert_file(parent_dir.join("notes.txt"), "");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(
            result, None,
            "non-.ron files in an ancestor's state dir should be ignored"
        );
    }
}
