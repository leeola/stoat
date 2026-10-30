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
    action_handlers::search::{LastSearch, SearchDirection},
    badge::{Anchor as BadgeAnchor, Badge, BadgeSource, BadgeState},
    buffer::{BufferId, BufferOp, TextBuffer},
    buffer_registry::{BufferRegistry, BufferRegistrySnapshot},
    commit_list::{CommitListId, CommitListSnap},
    dump::snapshot::ActiveRebaseSnap,
    editor_state::{EditorId, EditorState, EditorStateSnapshot},
    host::FsHost,
    input_history::InputHistory,
    pane::{self, DockId, DockPanel, FocusTarget, PaneId, PaneTree, View},
    rebase::RebaseState,
    ssh::RemoteTarget,
    workspace::{Tab, Workspace, WorkspaceUid},
};
use serde::{Deserialize, Serialize};
use slotmap::SlotMap;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::UNIX_EPOCH,
};
use stoat_scheduler::Executor;
use stoat_text::{Anchor, Bias, LineEnding, Point};

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

/// On-disk shape of [`LastSearch`], the pattern `n` and `N` repeat.
///
/// The compiled regex stays out. A restore compiles it on the first repeat,
/// under the smart-case setting of the restored session.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct LastSearchSnap {
    pub query: String,
    pub direction: SearchDirection,
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
    /// The buffers an editor holds and the buffers with unsaved edits. A buffer
    /// a pane moved off and did not edit is not saved, so a restart starts each
    /// pane's history from what was on screen, as a Helix launch does.
    pub buffers: BufferRegistrySnapshot,
    pub rebase: Option<RebaseState>,
    pub rebase_active: Option<ActiveRebaseSnap>,
    /// User-facing display name. Empty string on legacy files that predate
    /// the field; restore regenerates a default from `uid` in that case.
    #[serde(default)]
    pub name: String,
    /// The family root the session was created from. `None` for a session a
    /// launch opened, and on legacy files that predate the field.
    #[serde(default)]
    pub parent: Option<WorkspaceUid>,
    /// Finder scope this workspace last closed in, so `space p` reopens there.
    /// `None` on legacy files that predate the field and whenever no finder has
    /// closed here yet.
    #[serde(default)]
    pub last_finder_scope: Option<String>,
    /// Canonical command lines the palette recalls, oldest first, so history
    /// survives a restart. Empty on legacy files that predate the field.
    #[serde(default)]
    pub palette_history: Vec<String>,
    /// Search patterns the `/` and `?` prompts recall, oldest first. Empty on
    /// legacy files that predate the field.
    #[serde(default)]
    pub search_history: Vec<String>,
    /// Code-search queries the modal recalls, oldest first. Empty on legacy
    /// files that predate the field.
    #[serde(default)]
    pub code_search_history: Vec<String>,
    /// The pattern `n` and `N` repeat, so a repeat after a restart searches for
    /// the same pattern. `None` on legacy files that predate the field.
    #[serde(default)]
    pub last_search: Option<LastSearchSnap>,
    /// One entry per tab in display order, mirroring the in-memory shape: the
    /// entry at [`Self::active_tab`] is `None` because that tree is saved in
    /// [`Self::panes`]. Empty on legacy files, which restore as a single tab.
    #[serde(default)]
    pub tabs: Vec<Option<PaneTree>>,
    /// Index into [`Self::tabs`] of the tab whose tree is in [`Self::panes`].
    #[serde(default)]
    pub active_tab: usize,
    /// Index into [`Self::tabs`] of the tab a toggle returns to. `None` before
    /// the first switch, and on files that predate the field.
    #[serde(default)]
    pub last_tab: Option<usize>,
    /// One entry per tab in display order, holding each tab's `RenameTab`
    /// override or `None`. Empty or short on files that predate the field, so
    /// missing entries restore as unnamed tabs.
    #[serde(default)]
    pub tab_names: Vec<Option<String>>,
    /// The remote host this workspace's window was handed to and did not close
    /// cleanly. `None` on files written by a local session, and on files that
    /// predate the field.
    #[serde(default)]
    pub remote: Option<RemoteTarget>,
    /// The place of each open commits list, keyed by the id its pane names, so
    /// a restored list returns the reader to the commit they stood on. Empty on
    /// files that predate the field.
    #[serde(default)]
    pub commit_lists: Vec<(CommitListId, CommitListSnap)>,
    /// Selection endpoints owed a re-take against a compacted buffer's seed,
    /// resolved but not yet re-anchored.
    ///
    /// Never on the wire, because a state read back from disk already holds the
    /// anchors its own seed produced. Every path that reads a state resolves
    /// these first, so nothing outside this module sees the list non-empty.
    #[serde(skip)]
    pub(crate) pending_reanchors: Vec<PendingReanchor>,
    /// The buffers saved by reference to a file that changed or went missing
    /// while stoat was closed.
    ///
    /// Their saved anchors name offsets in text the file no longer holds, so
    /// the install resets their editors to one cursor and drops their jumps.
    /// Never on the wire, since only a read of a state file fills it.
    #[serde(skip)]
    pub(crate) drifted_buffers: Vec<BufferId>,
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

impl WorkspaceStateV1 {
    /// Re-take every pending selection endpoint against the seed its buffer
    /// restores from, and clear the list.
    ///
    /// The expensive half of a compacted reanchor. One buffer is built per
    /// pending buffer, so a save pays it on the blocking pool rather than on
    /// the run loop where the snapshot is taken.
    ///
    /// The seed is built rather than synthesized so the anchors come from the
    /// same code any other anchor does, including at the buffer ends where the
    /// sentinels take over.
    ///
    /// Idempotent, since a resolved list is empty and every entry point calls
    /// this before reading the state.
    pub(crate) fn resolve_reanchors(&mut self) {
        // One seed at a time, because the pending list groups every editor and
        // jump of a buffer together. An ungrouped list rebuilds on each switch,
        // which costs more and stays correct.
        let mut held: Option<(BufferId, TextBuffer)> = None;
        for pending in std::mem::take(&mut self.pending_reanchors) {
            if held.as_ref().is_none_or(|(id, _)| *id != pending.buffer) {
                let Some(entry) = self
                    .buffers
                    .entries
                    .iter()
                    .find(|entry| entry.id == pending.buffer)
                else {
                    continue;
                };
                let seed = TextBuffer::from_history(pending.buffer, &entry.history);
                held = Some((pending.buffer, seed));
            }
            let Some((_, seed)) = &held else {
                continue;
            };

            let mut endpoints = pending.endpoints.into_iter();
            let remap = |_: &Anchor| match endpoints.next() {
                Some((offset, bias)) => seed.anchor_at(offset, bias),
                None => unreachable!("an endpoint was resolved for every anchor"),
            };
            match pending.target {
                ReanchorTarget::Editor(editor_id) => {
                    let Some((_, editor)) =
                        self.editors.iter_mut().find(|(id, _)| *id == editor_id)
                    else {
                        continue;
                    };
                    editor.selections.reanchor(remap);
                },
                ReanchorTarget::Jump { tab, pane, entry } => {
                    let tree = if tab == self.active_tab {
                        &mut self.panes
                    } else {
                        let Some(tree) = self.tabs.get_mut(tab).and_then(Option::as_mut) else {
                            continue;
                        };
                        tree
                    };
                    tree.pane_mut(pane).jumplist.reanchor_entry(entry, remap);
                },
            }
        }
    }
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

        let buffers = {
            let held = self.held_buffers();
            self.buffers
                .snapshot_retaining(|id, buffer| held.contains(&id) || buffer.dirty)
        };

        let trees: Vec<(usize, &PaneTree)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| (i, tab.parked.as_ref().unwrap_or(&self.panes)))
            .collect();
        let pending_reanchors = pending_reanchors(&self.buffers, &buffers, &editors, &trees);

        let rebase_active = self
            .rebase_active
            .as_ref()
            .map(|active| ActiveRebaseSnap::from_active(active).snap);

        WorkspaceStateV1 {
            uid: self.uid,
            git_root: self.git_root.clone(),
            panes: self.panes.persisted_copy(),
            docks: pane::persisted_docks(&self.docks),
            focus: match self.focus {
                FocusTarget::SplitPane => FocusTargetSnap::SplitPane(self.panes.focus()),
                FocusTarget::Dock(id) => FocusTargetSnap::Dock(id),
            },
            editors,
            buffers,
            rebase: self.rebase.clone(),
            rebase_active,
            name: self.name.clone(),
            parent: self.parent,
            last_finder_scope: self.last_finder_scope.clone(),
            palette_history: self.palette_history.entries().to_vec(),
            search_history: self.search_history.entries().to_vec(),
            code_search_history: self.code_search_history.entries().to_vec(),
            last_search: self.last_search.as_ref().map(|last| LastSearchSnap {
                query: last.query.clone(),
                direction: last.direction,
            }),
            tabs: self
                .tabs
                .iter()
                .map(|tab| tab.parked.as_ref().map(PaneTree::persisted_copy))
                .collect(),
            active_tab: self.active_tab,
            last_tab: self.last_tab,
            tab_names: self.tabs.iter().map(|tab| tab.name.clone()).collect(),
            remote: self.remote.clone(),
            commit_lists: self
                .commit_lists
                .iter()
                .map(|(id, list)| (id, list.snapshot()))
                .collect(),
            pending_reanchors,
            drifted_buffers: Vec::new(),
        }
    }

    /// Serialize the current workspace state to RON and write it atomically
    /// to `path`. Parent directory is created if missing.
    ///
    /// The write goes through [`Self::save_gate`] as the newest save, so it
    /// writes nothing once the workspace is retired.
    pub(crate) fn save_state(&self, path: &Path, fs: &dyn FsHost) -> io::Result<()> {
        let seq = self.save_gate.issue();
        let mut state = self.to_state();
        state.resolve_reanchors();
        write_state_gated(&self.save_gate, seq, &state, &self.meta(), path, fs).map(|_| ())
    }

    /// The registry entry describing this workspace, as it stands.
    pub(crate) fn meta(&self) -> super::registry::WorkspaceMeta {
        super::registry::WorkspaceMeta {
            uid: self.uid,
            name: self.name.clone(),
            git_root: self.git_root.clone(),
            buffer_count: self.buffers.len(),
            remote_host: self.remote.as_ref().map(|target| target.host.clone()),
            parent: self.parent,
        }
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
        let mut state: WorkspaceStateV1 = ron::from_str(&body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        resolve_disk_seeds(&mut state, fs);
        self.apply_state(state, executor);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn apply_state(&mut self, mut state: WorkspaceStateV1, executor: &Executor) {
        state.resolve_reanchors();
        self.buffers
            .restore_from(std::mem::take(&mut state.buffers));
        self.install_restored_parts(state, executor);
    }

    /// Install a pre-built buffer `registry` and the remaining restored `state`.
    ///
    /// A `--continue` restore and a workspace copy build the registry off the
    /// main thread with [`restore_parts`] and hand it here, where only the cheap
    /// remainder runs. `state.buffers` is ignored in favor of `registry`.
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
        let redraw = self.redraw_notify.clone();
        let mut editors: SlotMap<EditorId, EditorState> = SlotMap::with_key();
        let mut editor_id_map: HashMap<EditorId, EditorId> = HashMap::new();
        let drifted = state.drifted_buffers;
        for (old_id, snap) in state.editors {
            let Some(buffer) = self.buffers.get(snap.buffer_id) else {
                tracing::warn!(
                    buffer_id = ?snap.buffer_id,
                    "buffer missing after restore; dropping editor"
                );
                continue;
            };
            let mut editor = EditorState::restore(snap, buffer, executor.clone(), redraw.clone());
            if drifted.contains(&editor.buffer_id) {
                reset_drifted_editor(&mut editor);
            }
            let new_id = editors.insert(editor);
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

        // A save keeps only the buffers a pane holds or the user dirtied, so a
        // jump into any other buffer names one this restore did not bring back.
        // A history entry for one rides into every later save unless it goes
        // here, since no id is ever issued again. A jump into a drifted buffer
        // names an offset in text that is gone.
        for tree in std::iter::once(&mut panes).chain(parked.iter_mut().flatten()) {
            for (_, pane) in tree.all_panes_mut() {
                pane.jumplist.retain_buffers(|buffer_id| {
                    self.buffers.get(buffer_id).is_some() && !drifted.contains(&buffer_id)
                });
                pane.buffer_history
                    .retain(|&buffer_id| self.buffers.get(buffer_id).is_some());
            }
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
        self.rebase_active = match state.rebase_active {
            // A rebase saved with no pause was out in the stepper, and the
            // snapshot does not record which entry was in flight. Driving it
            // on skips that entry, so the restore ends it.
            Some(snap) if snap.pause.is_none() => {
                self.badges.insert(Badge {
                    source: BadgeSource::Review,
                    anchor: BadgeAnchor::BottomRight,
                    state: BadgeState::Error,
                    label: "rebase interrupted".to_string(),
                    detail: Some(
                        "the session closed while the rebase ran; HEAD is where it stopped"
                            .to_string(),
                    ),
                });
                None
            },
            snap => snap.map(ActiveRebaseSnap::into_active),
        };
        self.name = if state.name.is_empty() {
            super::name::default_workspace_name(state.uid)
        } else {
            state.name
        };
        self.parent = state.parent;
        self.last_finder_scope = state.last_finder_scope;
        self.remote = state.remote;
        self.palette_history = InputHistory::from_entries(state.palette_history);
        self.search_history = InputHistory::from_entries(state.search_history);
        self.code_search_history = InputHistory::from_entries(state.code_search_history);
        self.last_search = state
            .last_search
            .map(|snap| LastSearch::uncompiled(snap.query, snap.direction));

        // A covered editor comes back under a fresh id, as a pane's editor does.
        self.restored_commit_lists = state
            .commit_lists
            .into_iter()
            .map(|(id, mut snap)| {
                if let Some(View::Editor(old)) = snap.covered {
                    snap.covered = editor_id_map.get(&old).map(|&new| View::Editor(new));
                }
                (id, snap)
            })
            .collect();

        // Exactly the active slot may be empty, since that tree is in `panes`.
        let coherent = parked.len() > 1
            && state.active_tab < parked.len()
            && parked
                .iter()
                .enumerate()
                .all(|(i, slot)| (i == state.active_tab) == slot.is_none());
        // A short or missing name vec (a legacy or partially-written file) fills
        // with None, so a tab past its end restores unnamed rather than failing.
        let mut names = state.tab_names.into_iter();
        if coherent {
            self.tabs = parked
                .into_iter()
                .map(|parked| Tab {
                    parked,
                    name: names.next().flatten(),
                })
                .collect();
            self.active_tab = state.active_tab;
            // A target past the tabs, or on the active one, names no tab to
            // return to.
            self.last_tab = state
                .last_tab
                .filter(|&tab| tab < self.tabs.len() && tab != state.active_tab);
        } else {
            // The file's tab shape disagrees with itself, and its intent is not
            // recoverable. One tab holding the restored active tree is always a
            // coherent workspace, and is what a legacy file means anyway.
            self.tabs = vec![Tab {
                parked: None,
                name: None,
            }];
            self.active_tab = 0;
            self.last_tab = None;
        }
    }
}

/// Write an already-taken workspace snapshot and its registry entry to `path`,
/// creating the parent directory if missing.
///
/// Split from [`Workspace::save_state`] so a caller can take the snapshot on
/// the thread that owns the workspace and pay the serialization somewhere else.
/// Nothing here reads the live workspace, so what lands on disk is the
/// workspace as it stood when `state` was taken, however long ago that was.
///
/// The state file is replaced atomically through a temporary file of its own,
/// so a reader never sees a partial one, and two writes never rename each
/// other's bytes into place.
fn write_state(
    state: &WorkspaceStateV1,
    meta: &super::registry::WorkspaceMeta,
    path: &Path,
    fs: &dyn FsHost,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs.create_dir_all(parent)?;
    }

    // The limit keeps the top-level and per-buffer fields one per line, and
    // puts each history and each deeper tree on one line. One field per line
    // at every depth spends about four times the bytes on each undo group.
    let config = ron::ser::PrettyConfig::default().depth_limit(4);
    let body = ron::ser::to_string_pretty(state, config)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    fs.write_atomic(path, body.as_bytes())?;

    super::registry::write_meta(meta, path, fs)
}

/// Rename the saved session whose state file is `state_path` to `name`.
///
/// The name lives in the state file and in its meta sidecar. A restore takes
/// the name from the state file, so a sidecar-only rename comes undone at the
/// next restore, and both are rewritten. The read and the write block, so a
/// caller on the run loop runs this on the blocking pool.
pub(crate) fn rename_saved_session(
    state_path: &Path,
    name: &str,
    fs: &dyn FsHost,
) -> io::Result<()> {
    let mut buf = Vec::new();
    fs.read(state_path, &mut buf)?;
    let body = String::from_utf8(buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut state: WorkspaceStateV1 = ron::from_str(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    state.name = name.to_owned();
    let meta = super::registry::meta_of(&state);
    write_state(&state, &meta, state_path, fs)
}

/// The order of one workspace's session writes, and whether any of them still
/// lands.
///
/// Every save takes its snapshot on the run loop and writes on the blocking
/// pool, which runs the writes in no fixed order. A write the pool already
/// holds when a close or a delete removes the session files also writes them
/// again. Each save takes a sequence number with its snapshot, and each write
/// holds the gate's state while it writes, so a write older than the last one
/// written, one a move of the session superseded, or one for a retired
/// workspace, lands nothing.
///
/// [`Self::issue`] takes no lock, so no save, close, delete, or `:cd` waits on
/// the run loop for a write in progress. The state lock puts the writes, the
/// retire, and the supersede in one order. The pool takes it, and so does the
/// synchronous save of a quit, which must land before the process exits.
///
/// See also:
/// - [`write_state_gated`] for the write that reads the gate.
#[derive(Default)]
pub(crate) struct SaveGate {
    issued: AtomicU64,
    state: Mutex<GateState>,
}

/// What the writes of one [`SaveGate`] did, read and changed under its lock.
#[derive(Default)]
struct GateState {
    written: u64,
    retired: bool,
}

impl SaveGate {
    /// Take the sequence number of a snapshot about to be taken.
    pub(crate) fn issue(&self) -> u64 {
        self.issued.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The sequence number the last [`Self::issue`] took, or 0 before the
    /// first.
    pub(crate) fn issued(&self) -> u64 {
        self.issued.load(Ordering::Relaxed)
    }

    /// Stop every write of this workspace that has not landed, before its
    /// session files are removed.
    ///
    /// Waits for a write in progress, and no write starts after.
    pub(crate) fn retire(&self) {
        self.state.lock().expect("save gate poisoned").retired = true;
    }

    /// Skip every write numbered `seq` or lower, before the session file they
    /// name is removed and the workspace saves under another path.
    ///
    /// A `:cd` moves the session to the new root's file, and a write issued
    /// before it names the old one. The move reads `seq` from [`Self::issued`]
    /// on the run loop and supersedes later on the pool. A save issued after
    /// the move takes a higher number, so it lands whether its write or the
    /// supersede runs first. Waits for a write in progress.
    pub(crate) fn supersede_through(&self, seq: u64) {
        let mut state = self.state.lock().expect("save gate poisoned");
        state.written = state.written.max(seq);
    }
}

/// [`write_state`] for the save numbered `seq`, unless a newer save of the
/// workspace already landed, a move of its session superseded the save, or the
/// workspace is retired.
///
/// Holds `gate`'s state for the whole write, so two writes of one workspace
/// never overlap. Returns whether the write ran.
pub(crate) fn write_state_gated(
    gate: &SaveGate,
    seq: u64,
    state: &WorkspaceStateV1,
    meta: &super::registry::WorkspaceMeta,
    path: &Path,
    fs: &dyn FsHost,
) -> io::Result<bool> {
    let mut gate = gate.state.lock().expect("save gate poisoned");
    if gate.retired || seq <= gate.written {
        return Ok(false);
    }
    write_state(state, meta, path, fs)?;
    gate.written = seq;
    Ok(true)
}

/// The selection endpoints of one editor or one jump, resolved against the
/// live buffer and waiting to be re-taken against the seed that will replace
/// it.
///
/// The offsets run start then end of each selection, in selection order. That
/// is the order [`SelectionsCollection::anchors`] yields an editor's and the
/// order both [`SelectionsCollection::reanchor`] and
/// [`JumpList::reanchor_entry`](crate::jumplist::JumpList::reanchor_entry)
/// consume them, so the two halves pair up positionally.
#[derive(Debug)]
pub(crate) struct PendingReanchor {
    target: ReanchorTarget,
    buffer: BufferId,
    endpoints: Vec<(usize, Bias)>,
}

/// What a [`PendingReanchor`]'s endpoints are re-taken into.
#[derive(Debug)]
enum ReanchorTarget {
    /// An editor's selection set.
    Editor(EditorId),
    /// One entry of a pane's jumplist. `tab` indexes
    /// [`WorkspaceStateV1::tabs`], and the active tab's tree is
    /// [`WorkspaceStateV1::panes`].
    Jump {
        tab: usize,
        pane: PaneId,
        entry: usize,
    },
}

/// Resolve the selections of every editor showing a buffer whose history is
/// persisting compacted, and of every jump into such a buffer from any pane of
/// any tab in `trees`.
///
/// A selection endpoint is an anchor naming an insertion in the fragment tree
/// the op log builds, and a compacted history replays a seed instead of that
/// log. The restored tree holds one insertion the saved anchors never mention,
/// so carrying them across would leave the cursor at a position unrelated to
/// where the user left it. Resolving each against the live buffer and re-taking
/// it against the seed keeps the position, which is the whole point of
/// persisting selections.
///
/// Only the resolve happens here, because only it needs the live buffer, and it
/// is O(log n) per endpoint. Re-taking means building the seed, which is the
/// expensive half and needs nothing but the history, so
/// [`WorkspaceStateV1::resolve_reanchors`] does it wherever the state is next
/// used. On the autosave path that is the blocking pool.
fn pending_reanchors(
    registry: &BufferRegistry,
    snap: &BufferRegistrySnapshot,
    editors: &[(EditorId, EditorStateSnapshot)],
    trees: &[(usize, &PaneTree)],
) -> Vec<PendingReanchor> {
    let mut pending = Vec::new();
    for entry in snap.entries.iter().filter(|e| e.history.compacted) {
        let Some(live) = registry.get(entry.id) else {
            continue;
        };
        let live = live.read().expect("buffer poisoned");

        for (id, editor) in editors.iter().filter(|(_, ed)| ed.buffer_id == entry.id) {
            pending.push(PendingReanchor {
                target: ReanchorTarget::Editor(*id),
                buffer: entry.id,
                endpoints: editor
                    .selections
                    .anchors()
                    .map(|anchor| (live.resolve_anchor(anchor), anchor.bias))
                    .collect(),
            });
        }

        // Pushed in the same per-buffer pass as the editors, because
        // `resolve_reanchors` builds one seed per run of a buffer's pendings.
        for &(tab, tree) in trees {
            for (pane_id, pane) in tree.all_panes() {
                for (index, jump) in pane.jumplist.entries().iter().enumerate() {
                    if jump.buffer_id != entry.id {
                        continue;
                    }
                    pending.push(PendingReanchor {
                        target: ReanchorTarget::Jump {
                            tab,
                            pane: pane_id,
                            entry: index,
                        },
                        buffer: entry.id,
                        endpoints: jump
                            .selections
                            .iter()
                            .flat_map(|s| {
                                [
                                    (live.resolve_anchor(&s.start), s.start.bias),
                                    (live.resolve_anchor(&s.end), s.end.bias),
                                ]
                            })
                            .collect(),
                    });
                }
            }
        }
    }
    pending
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
    resolve_disk_seeds(&mut state, fs);
    Ok(restore_parts(state))
}

/// Load the file of each entry that names its seed as the file on disk, so the
/// replay that follows reads that text.
///
/// The file is trusted while its mtime and its normalized length match what
/// the entry saved. Otherwise its current text loads, with its own ending and
/// mtime, and the entry joins [`WorkspaceStateV1::drifted_buffers`]. An mtime
/// that does not read never matches, so such a file counts as drifted too. A
/// file that is gone or unreadable loads as `"\n"`, as an open of a missing
/// path does, and counts as drifted.
///
/// The read decompresses a gzip stream, because the open did, so the seed it
/// replaces holds the decompressed text.
fn resolve_disk_seeds(state: &mut WorkspaceStateV1, fs: &dyn FsHost) {
    for entry in &mut state.buffers.entries {
        let Some(&BufferOp::DiskSeed { len }) = entry.history.ops.first() else {
            continue;
        };
        let read = entry.path.as_deref().and_then(|path| {
            // The stat comes first, as an open's does. A write between the two
            // calls then leaves an mtime older than the text. The next save then
            // finds the file changed on disk and does not write over it.
            let modified = fs.metadata(path).ok().flatten().map(|meta| meta.modified);
            let raw = crate::action_handlers::read_text_via_host(fs, path).ok()?;
            Some((raw, modified))
        });

        let (text, drifted) = match read {
            Some((raw, modified)) => {
                let text = LineEnding::normalize(&raw).into_owned();
                let unchanged =
                    modified.is_some() && modified == entry.disk_mtime && text.len() as u64 == len;
                if !unchanged {
                    entry.line_ending = LineEnding::detect(&raw);
                    entry.disk_mtime = modified;
                }
                (text, !unchanged)
            },
            None => ("\n".to_string(), true),
        };
        entry.history.ops[0] = BufferOp::Edit { old: 0..0, text };
        if drifted {
            state.drifted_buffers.push(entry.id);
        }
    }
}

/// The buffer registry `state` restores, and the rest of the state to install
/// with it through [`Workspace::install_restored`].
///
/// Replays each buffer's op log and undo tree, so a caller on the run loop runs
/// this on the blocking pool. The reanchors that a live snapshot carries
/// resolve first, against the same histories.
pub(crate) fn restore_parts(mut state: WorkspaceStateV1) -> (BufferRegistry, WorkspaceStateV1) {
    state.resolve_reanchors();
    let mut registry = BufferRegistry::new();
    registry.restore_from(std::mem::take(&mut state.buffers));
    (registry, state)
}

/// Put `editor` on one block cursor at the start of its saved scroll row, for a
/// buffer whose file changed while stoat was closed.
///
/// The saved selections name offsets in the text the file held before, so none
/// of them still means anything. The row clamps to the buffer's last row.
fn reset_drifted_editor(editor: &mut EditorState) {
    let snapshot = editor.display_map.buffer_snapshot();
    let rope = snapshot.rope();
    editor.scroll_row = editor.scroll_row.min(rope.max_point().row);
    let offset = rope.point_to_offset(Point::new(editor.scroll_row, 0));
    editor.selections.set_block_cursor(offset, &snapshot);
    editor.move_source_cursor = None;
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
        // Terminal, commits, and changes view panes survive the sweep with a
        // dead id. The app respawns a fresh shell, list, or changes view for
        // each after restore. See action_handlers::terminal,
        // action_handlers::commits, and action_handlers::diff_changes. An image
        // pane refers to a file rather than a live session, so a restored one
        // is as good as it was.
        View::Terminal(_)
        | View::Commits(_)
        | View::DiffChanges(_)
        | View::Label(_)
        | View::Editor(_)
        | View::Image { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        host::FakeFs,
        jumplist::{JumpEntry, JumpList},
        multi_buffer::MultiBuffer,
        pane::{Axis, DockSide, DockVisibility, Placement},
        rebase::{ActiveRebase, RebasePause},
        selection::SelectionsCollection,
    };
    use std::{
        collections::{HashSet, VecDeque},
        sync::Arc,
        time::Duration,
    };
    use stoat_scheduler::TestScheduler;
    use stoat_text::{Bias, Selection, SelectionGoal};

    /// Text a state file holds only where a save spells out a buffer's text.
    const SENTINEL: &str = "sentinel-5e2b";

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
        let mut ws = Workspace::new(git_root, exec, crate::test_notify());
        ws.layout(ratatui::layout::Rect::new(0, 0, 120, 40));
        ws
    }

    /// Opens `path` with an editor holding it, as a file a pane shows, so a save
    /// keeps the buffer.
    fn open_held(ws: &mut Workspace, path: &Path, text: &str, exec: &Executor) -> BufferId {
        let (id, buffer) = ws.buffers.open(path, text);
        ws.editors.insert(EditorState::new(
            id,
            buffer,
            exec.clone(),
            crate::test_notify(),
        ));
        id
    }

    /// A save that serializes later must still record the workspace as it was
    /// when the snapshot was taken.
    ///
    /// That is the whole basis for taking the snapshot on the thread that owns
    /// the workspace and writing it somewhere else. A workspace switch snapshots
    /// the workspace being left, and by the time the bytes reach disk the user
    /// has moved on and is editing again. Were the write to consult the live
    /// workspace, it would save whatever happened to be true by then.
    #[test]
    fn a_deferred_write_records_the_workspace_as_it_was_snapshotted() {
        let fake = FakeFs::new();
        let exec = executor();
        let ws_dir = PathBuf::from("/deferred");
        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        open_held(&mut ws, &ws_dir.join("before.txt"), "before\n", &exec);

        let (state, meta) = (ws.to_state(), ws.meta());

        // Everything after this point is what the user did while the write was
        // still queued, and none of it belongs in the file.
        open_held(&mut ws, &ws_dir.join("after.txt"), "after\n", &exec);
        ws.name = "renamed".to_string();

        let deferred = ws_dir.join("deferred.ron");
        write_state(&state, &meta, &deferred, &fake).unwrap();

        let reference = FakeFs::new();
        let taken_then = ws_dir.join("taken_then.ron");
        write_state(&state, &meta, &taken_then, &reference).unwrap();

        let written = |fs: &FakeFs, path: &Path| {
            let mut buf = Vec::new();
            fs.read(path, &mut buf).unwrap();
            String::from_utf8(buf).unwrap()
        };
        let body = written(&fake, &deferred);
        assert!(
            body.contains("before.txt"),
            "the snapshotted buffer is in the file, got {body}",
        );
        assert!(
            !body.contains("after.txt"),
            "and the one opened after the snapshot is not, got {body}",
        );
        assert_eq!(
            body,
            written(&reference, &taken_then),
            "which is byte for byte what saving at snapshot time produced",
        );

        let meta_body = written(&fake, &super::super::registry::meta_path_for(&deferred));
        assert!(
            !meta_body.contains("renamed"),
            "the registry entry is the snapshot's too, got {meta_body}",
        );
    }

    /// Writes land in the order their saves were issued, and none lands once
    /// the workspace is retired. An older snapshot that reaches the gate after
    /// a newer one leaves the newer file in place, and a retired workspace
    /// writes no file back after its files are removed.
    #[test]
    fn a_gated_write_skips_an_older_or_retired_save() {
        let fake = FakeFs::new();
        let exec = executor();
        let mut ws = new_laid_out_workspace(PathBuf::from("/gated"), &exec);
        let path = PathBuf::from("/gated/state.ron");
        let meta_path = super::super::registry::meta_path_for(&path);
        let gate = SaveGate::default();
        let write = |ws: &Workspace, seq| {
            write_state_gated(&gate, seq, &ws.to_state(), &ws.meta(), &path, &fake).unwrap()
        };

        ws.name = "new".to_string();
        let newer = write(&ws, 2);
        ws.name = "old".to_string();
        let older = write(&ws, 1);
        let on_disk = {
            let mut buf = Vec::new();
            fake.read(&path, &mut buf).unwrap();
            let state: WorkspaceStateV1 = ron::from_str(&String::from_utf8(buf).unwrap()).unwrap();
            state.name
        };

        fake.remove_file(&path).unwrap();
        fake.remove_file(&meta_path).unwrap();
        gate.retire();
        let retired = write(&ws, 3);

        assert_eq!(
            (
                newer,
                older,
                on_disk,
                retired,
                [fake.exists(&path), fake.exists(&meta_path)]
            ),
            (true, false, "new".to_string(), false, [false, false]),
        );
    }

    /// The run loop issues a save while the pool writes another, so an issue
    /// takes no lock that a write holds.
    #[test]
    fn an_issue_takes_no_lock_while_a_write_holds_the_gate() {
        let gate = SaveGate::default();
        let _write = gate.state.lock().expect("save gate poisoned");
        assert_eq!((gate.issue(), gate.issue()), (1, 2));
    }

    /// A `:cd` reads its boundary on the run loop and supersedes on the pool,
    /// so the save it issues next has its number before the supersede runs.
    /// That save still lands, and the save queued before the move does not.
    #[test]
    fn a_save_issued_after_a_cd_still_lands() {
        let gate = SaveGate::default();
        let queued = gate.issue();
        let boundary = gate.issued();
        let after = gate.issue();
        gate.supersede_through(boundary);

        assert_eq!(
            (
                gated_write_lands(&gate, queued),
                gated_write_lands(&gate, after)
            ),
            (false, true),
        );
    }

    /// A supersede that reaches the pool after a newer save landed keeps that
    /// save's mark, so an older save still in the queue does not write over
    /// the newer file.
    #[test]
    fn a_late_supersede_keeps_a_newer_write_in_place() {
        let gate = SaveGate::default();
        let boundary = gate.issued();
        let (older, newer) = (gate.issue(), gate.issue());
        let newer_landed = gated_write_lands(&gate, newer);
        gate.supersede_through(boundary);

        assert_eq!(
            (newer_landed, gated_write_lands(&gate, older)),
            (true, false)
        );
    }

    /// Whether the save numbered `seq` writes through `gate`, measured with a
    /// fresh workspace written to a fresh fake.
    fn gated_write_lands(gate: &SaveGate, seq: u64) -> bool {
        let exec = executor();
        let ws = new_laid_out_workspace(PathBuf::from("/gated"), &exec);
        let path = PathBuf::from("/gated/state.ron");
        write_state_gated(gate, seq, &ws.to_state(), &ws.meta(), &path, &FakeFs::new())
            .expect("the write runs")
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
        let hole = ws.editors.insert(EditorState::new(
            id_hole,
            buf_hole,
            exec.clone(),
            crate::test_notify(),
        ));
        ws.editors.remove(hole);

        let (id_a, buf_a) = ws.buffers.open(&ws_dir.join("a.txt"), "alpha\n");
        let editor_a = ws.editors.insert(EditorState::new(
            id_a,
            buf_a,
            exec.clone(),
            crate::test_notify(),
        ));

        let root = ws.panes.focus();
        ws.panes.pane_mut(root).view = View::Editor(editor_a);

        // A second tab, leaving the first parked behind it.
        ws.new_tab(&exec);
        assert_eq!(ws.active_tab, 1);
        assert_eq!(parked_focus_editor(&ws, 0), Some(editor_a));

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(fresh.tabs.len(), 2, "both tabs survive");
        assert_eq!(fresh.active_tab, 1);
        assert!(
            fresh.tabs[1].parked.is_none(),
            "the active tab's tree is in ws.panes, not parked"
        );
        assert_eq!(fresh.last_tab, Some(0), "the toggle target persists");

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
        assert_eq!(
            (fresh.toggle_tab(), fresh.active_tab),
            (true, 0),
            "a toggle returns to the tab the save last left",
        );
    }

    /// A target past the saved tabs names nothing to return to, so it restores
    /// as no target rather than as a toggle into a tab that does not exist.
    #[test]
    fn an_out_of_range_toggle_target_restores_as_none() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/far-toggle");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.new_tab(&exec);
        let mut state = ws.to_state();
        state.last_tab = Some(9);
        let state_path = ws_dir.join("state.ron");
        write_state(&state, &ws.meta(), &state_path, &fake).unwrap();

        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(
            (fresh.tabs.len(), fresh.last_tab),
            (2, None),
            "both tabs restore, and the target past them restores as none",
        );
    }

    /// A pane's buffer history is what `goto_last_accessed` and a buffer close
    /// return through, so each pane in each tab keeps its own across a restart.
    #[test]
    fn round_trip_keeps_each_panes_buffer_history() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/history");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let id_a = open_held(&mut ws, &ws_dir.join("a.txt"), "alpha\n", &exec);
        let id_b = open_held(&mut ws, &ws_dir.join("b.txt"), "beta\n", &exec);
        let id_c = open_held(&mut ws, &ws_dir.join("c.txt"), "gamma\n", &exec);

        let first = ws.panes.focus();
        ws.panes.pane_mut(first).record_shown(id_a);
        ws.panes.pane_mut(first).record_shown(id_b);
        ws.new_tab(&exec);
        let second = ws.panes.focus();
        ws.panes.pane_mut(second).record_shown(id_c);

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();
        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        let parked = fresh.tabs[0].parked.as_ref().expect("tab 0 parks");
        assert_eq!(
            (
                parked.pane(parked.focus()).buffer_history.clone(),
                fresh.panes.pane(fresh.panes.focus()).buffer_history.clone(),
            ),
            (vec![id_a, id_b], vec![id_c]),
            "each tab's focused pane keeps the buffers it showed",
        );
    }

    /// A clean buffer that no editor holds leaves with its session, and the
    /// restore drops its history entry too. A session that shows such a buffer
    /// each time therefore grows no history across restarts.
    #[test]
    fn repeated_restores_do_not_grow_a_panes_history() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/regrow");
        let exec = executor();
        let state_path = ws_dir.join("state.ron");

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let held = open_held(&mut ws, &ws_dir.join("held.txt"), "held\n", &exec);
        let focus = ws.panes.focus();
        ws.panes.pane_mut(focus).record_shown(held);

        for _ in 0..3 {
            let (clean, _) = ws.buffers.open(&ws_dir.join("clean.txt"), "clean\n");
            let focus = ws.panes.focus();
            ws.panes.pane_mut(focus).record_shown(clean);
            ws.save_state(&state_path, &fake).unwrap();

            ws = Workspace::new(PathBuf::from("/elsewhere"), &exec, crate::test_notify());
            ws.restore_state(&state_path, &fake, &exec).unwrap();
        }

        assert_eq!(
            ws.panes.pane(ws.panes.focus()).buffer_history,
            [held],
            "the held buffer is the history's only entry after three restarts",
        );
    }

    /// The `--continue` reader and the synchronous one the persistence tests use
    /// both resolve the seed.
    #[test]
    fn a_disk_seed_entry_restores_the_file_text() {
        let (fake, exec, path) = (FakeFs::new(), executor(), notes_path());
        fake.insert_file(&path, "alpha\nbeta\n");
        let (id, state_path) = save_by_reference(&fake, &exec, &path, 0);

        let (fresh, drifted) = restore_by_reference(&fake, &exec, &state_path);
        let mut synchronous =
            Workspace::new(PathBuf::from("/elsewhere"), &exec, crate::test_notify());
        synchronous
            .restore_state(&state_path, &fake, &exec)
            .expect("the state restores");
        assert_eq!(
            (
                buffer_text(&fresh, id),
                buffer_text(&synchronous, id),
                drifted
            ),
            (
                "alpha\nbeta\n".to_string(),
                "alpha\nbeta\n".to_string(),
                Vec::new()
            ),
        );
    }

    /// The saved row is 3. A new text that reaches it puts the cursor at that
    /// row's start, and a shorter one clamps it to the last row. A rewrite of
    /// the same length drifts too, because its mtime moved.
    #[test]
    fn a_file_changed_while_closed_restores_its_new_text_and_a_cursor_on_its_scroll_row() {
        for (new_text, row, row_start) in [
            ("one\ntwo\nthree\nfour\nfive\n", 3, 14),
            ("one\ntwo", 1, 4),
            ("gamma\nzeta\n", 2, 11),
        ] {
            let (fake, exec, path) = (FakeFs::new(), executor(), notes_path());
            fake.insert_file(&path, "alpha\nbeta\n");
            let (id, state_path) = save_by_reference(&fake, &exec, &path, 3);
            fake.insert_file(&path, new_text);

            let (fresh, drifted) = restore_by_reference(&fake, &exec, &state_path);
            let editor = fresh
                .editors
                .values()
                .find(|editor| editor.buffer_id == id)
                .expect("the editor restores");
            let cursor = {
                let buffer = fresh.buffers.get(id).expect("buffer restores");
                let guard = buffer.read().expect("buffer poisoned");
                guard.resolve_anchor(&editor.selections.newest_anchor().start)
            };
            let jumps_into = fresh
                .panes
                .pane(fresh.panes.focus())
                .jumplist
                .entries()
                .iter()
                .filter(|jump| jump.buffer_id == id)
                .count();
            assert_eq!(
                (
                    buffer_text(&fresh, id),
                    cursor,
                    editor.scroll_row,
                    editor.move_source_cursor,
                    jumps_into,
                    drifted
                ),
                (new_text.to_string(), row_start, row, None, 0, vec![id]),
                "{new_text:?} loads on row {row} with no move source or jump into it",
            );
        }
    }

    #[test]
    fn a_missing_file_restores_as_an_empty_buffer_at_its_path() {
        let (fake, exec, path) = (FakeFs::new(), executor(), notes_path());
        fake.insert_file(&path, "alpha\nbeta\n");
        let (id, state_path) = save_by_reference(&fake, &exec, &path, 0);
        fake.remove_file(&path).expect("the file removes");

        let (fresh, drifted) = restore_by_reference(&fake, &exec, &state_path);
        assert_eq!(
            (
                buffer_text(&fresh, id),
                fresh.buffers.path_for(id).map(Path::to_path_buf),
                drifted,
            ),
            ("\n".to_string(), Some(path), vec![id]),
        );
    }

    #[test]
    fn a_crlf_file_restores_lf_with_its_ending() {
        let (fake, exec, path) = (FakeFs::new(), executor(), notes_path());
        fake.insert_file(&path, "alpha\r\nbeta\r\n");
        let (id, state_path) = save_by_reference(&fake, &exec, &path, 0);

        let (fresh, drifted) = restore_by_reference(&fake, &exec, &state_path);
        assert_eq!(
            (
                buffer_text(&fresh, id),
                fresh.buffers.line_ending(id),
                drifted
            ),
            ("alpha\nbeta\n".to_string(), LineEnding::Crlf, Vec::new()),
        );
    }

    /// A drifted entry takes the ending and the mtime the file holds, so a later
    /// save writes CRLF and looks for an external edit against the new mtime.
    #[test]
    fn a_file_rewritten_while_closed_takes_its_new_ending_and_mtime() {
        let (fake, exec, path) = (FakeFs::new(), executor(), notes_path());
        fake.insert_file(&path, "alpha\nbeta\n");
        let (id, state_path) = save_by_reference(&fake, &exec, &path, 0);
        fake.insert_file(&path, "alpha\r\nbeta\r\n");
        let mtime = fake
            .metadata(&path)
            .expect("the file stats")
            .expect("the file exists")
            .modified;

        let (fresh, _) = restore_by_reference(&fake, &exec, &state_path);
        assert_eq!(
            (fresh.buffers.line_ending(id), fresh.buffers.disk_mtime(id)),
            (LineEnding::Crlf, Some(mtime)),
        );
    }

    /// A rewrite inside one mtime tick leaves the mtime the entry saved, so only
    /// the length shows the change. Here the entry claims one byte more than the
    /// file holds.
    #[test]
    fn a_rewrite_that_keeps_the_mtime_counts_as_drifted() {
        let (fake, exec, path) = (FakeFs::new(), executor(), notes_path());
        fake.insert_file(&path, "alpha\nbeta\n");
        let (id, state_path) = save_by_reference(&fake, &exec, &path, 0);
        let mut state: WorkspaceStateV1 = {
            let body = crate::action_handlers::read_string_via_host(&fake, &state_path)
                .expect("the state reads");
            ron::from_str(&body).expect("the state parses")
        };
        let entry = state
            .buffers
            .entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .expect("the held buffer saves");
        entry.history.ops[0] = BufferOp::DiskSeed { len: 12 };
        let body = ron::to_string(&state).expect("the state serializes");
        fake.write(&state_path, body.as_bytes())
            .expect("the state writes");

        let (_, drifted) = restore_by_reference(&fake, &exec, &state_path);
        assert_eq!(drifted, [id]);
    }

    /// An mtime that reads at neither the save nor the restore proves nothing,
    /// so the file counts as drifted though its length matches.
    #[test]
    fn an_mtime_that_never_reads_counts_as_drifted() {
        let (fake, exec, path) = (FakeFs::new(), executor(), notes_path());
        fake.insert_file(&path, "alpha\nbeta\n");
        fake.fail_next_metadata(&path, io::ErrorKind::PermissionDenied);
        let (id, state_path) = save_by_reference(&fake, &exec, &path, 0);
        fake.fail_next_metadata(&path, io::ErrorKind::PermissionDenied);

        let (_, drifted) = restore_by_reference(&fake, &exec, &state_path);
        assert_eq!(drifted, [id]);
    }

    #[test]
    fn an_unedited_buffer_persists_no_text() {
        let (fake, exec) = (FakeFs::new(), executor());
        let mut ws = new_laid_out_workspace(PathBuf::from("/by-reference"), &exec);
        open_on_disk(&mut ws, &fake, &format!("{SENTINEL}\n"), &exec);

        let body = saved_body(&ws, &fake);
        assert_eq!(
            (body.contains(SENTINEL), body.contains("DiskSeed")),
            (false, true),
        );
    }

    /// The edit is saved, so the buffer is clean. Undo across a restart replays
    /// from the seed, so the seed stays in the state file.
    #[test]
    fn an_edited_buffer_persists_its_seed_inline() {
        let (fake, exec) = (FakeFs::new(), executor());
        let mut ws = new_laid_out_workspace(PathBuf::from("/by-reference"), &exec);
        let id = open_on_disk(&mut ws, &fake, &format!("{SENTINEL}\n"), &exec);
        {
            let buffer = ws.buffers.get(id).expect("the buffer opens");
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(0..0, "> ");
            guard.mark_clean();
        }

        let body = saved_body(&ws, &fake);
        assert_eq!(
            (body.contains(SENTINEL), body.contains("DiskSeed")),
            (true, false),
        );
    }

    /// A long enough log compacts to one seed edit that holds the current text,
    /// which is the shape of an unedited log. Only the unsaved edits keep that
    /// text in the state file.
    #[test]
    fn a_dirty_buffer_persists_its_text_once_its_log_compacts() {
        let (fake, exec) = (FakeFs::new(), executor());
        let mut ws = new_laid_out_workspace(PathBuf::from("/by-reference"), &exec);
        let id = open_on_disk(&mut ws, &fake, "\n", &exec);
        {
            let buffer = ws.buffers.get(id).expect("the buffer opens");
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(0..0, SENTINEL);
            for _ in 0..crate::buffer::OPS_COMPACT_THRESHOLD {
                guard.edit(0..0, "x");
            }
        }

        let body = saved_body(&ws, &fake);
        assert_eq!(
            (body.contains(SENTINEL), body.contains("DiskSeed")),
            (true, false),
        );
    }

    /// The second save is of the buffer the first restore replayed, whose log
    /// spells its seed out, and it names the file too.
    #[test]
    fn an_unedited_buffer_round_trips_through_a_save_and_a_restore() {
        let (fake, exec) = (FakeFs::new(), executor());
        let text = format!("alpha\n{SENTINEL}\n");
        let mut ws = new_laid_out_workspace(PathBuf::from("/by-reference"), &exec);
        let id = open_on_disk(&mut ws, &fake, &text, &exec);
        for editor in ws
            .editors
            .values_mut()
            .filter(|editor| editor.buffer_id == id)
        {
            let snapshot = editor.display_map.buffer_snapshot();
            editor.selections.set_block_cursor(8, &snapshot);
        }

        for restart in 1..=2 {
            let body = saved_body(&ws, &fake);
            let (fresh, drifted) = restore_by_reference(&fake, &exec, &notes_state_path());
            let cursor = {
                let editor = fresh
                    .editors
                    .values()
                    .find(|editor| editor.buffer_id == id)
                    .expect("the editor restores");
                let primary = &editor.selections.all_anchors()[editor.selections.primary_index()];
                let buffer = fresh.buffers.get(id).expect("the buffer restores");
                let guard = buffer.read().expect("buffer poisoned");
                guard.resolve_anchor(&primary.start)
            };
            assert_eq!(
                (
                    body.contains(SENTINEL),
                    buffer_text(&fresh, id),
                    cursor,
                    drifted
                ),
                (false, text.clone(), 8, Vec::new()),
                "restart {restart} writes no text and loads the file under the cursor",
            );
            ws = fresh;
        }
    }

    fn notes_path() -> PathBuf {
        PathBuf::from("/by-reference/notes.txt")
    }

    fn notes_state_path() -> PathBuf {
        PathBuf::from("/by-reference/state.ron")
    }

    /// The state file of a workspace whose one held buffer opened `path` from
    /// `fake`, saved with that buffer's entry naming the file as its seed.
    ///
    /// The buffer's editor sits at `scroll_row` on a picked move source, and the
    /// focused pane holds one jump into the buffer, so a restore shows whether
    /// any of them survives. A failed metadata read saves the entry without an
    /// mtime.
    fn save_by_reference(
        fake: &FakeFs,
        exec: &Executor,
        path: &Path,
        scroll_row: u32,
    ) -> (BufferId, PathBuf) {
        let raw = crate::action_handlers::read_string_via_host(fake, path).expect("the file reads");
        let text = LineEnding::normalize(&raw).into_owned();

        let ws_dir = PathBuf::from("/by-reference");
        let mut ws = new_laid_out_workspace(ws_dir.clone(), exec);
        let id = open_held(&mut ws, path, &text, exec);
        ws.buffers.set_line_ending(id, LineEnding::detect(&raw));
        for editor in ws
            .editors
            .values_mut()
            .filter(|editor| editor.buffer_id == id)
        {
            editor.scroll_row = scroll_row;
            editor.move_source_cursor = Some((0, 1));
        }
        let jump = jump_at(&ws.buffers, id, 0);
        let focus = ws.panes.focus();
        ws.panes.pane_mut(focus).jumplist.push(jump, &ws.buffers);

        let mut state = ws.to_state();
        let entry = state
            .buffers
            .entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .expect("the held buffer saves");
        entry.history.seed = None;
        entry.history.ops[0] = BufferOp::DiskSeed {
            len: text.len() as u64,
        };
        entry.disk_mtime = fake.metadata(path).ok().flatten().map(|meta| meta.modified);

        let state_path = ws_dir.join("state.ron");
        let body = ron::ser::to_string_pretty(&state, ron::ser::PrettyConfig::default())
            .expect("the state serializes");
        fake.write(&state_path, body.as_bytes())
            .expect("the state writes");
        (id, state_path)
    }

    /// The restore of `state_path` into a fresh workspace, and the buffers the
    /// read counted as drifted.
    fn restore_by_reference(
        fake: &FakeFs,
        exec: &Executor,
        state_path: &Path,
    ) -> (Workspace, Vec<BufferId>) {
        let (registry, state) = read_restore_parts(state_path, fake).expect("the state reads");
        let drifted = state.drifted_buffers.clone();
        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), exec, crate::test_notify());
        fresh.install_restored(registry, state, exec);
        (fresh, drifted)
    }

    /// Open `text` as a held buffer of the file at [`notes_path`], which `fake`
    /// holds first, with the mtime that an open of the file records.
    fn open_on_disk(ws: &mut Workspace, fake: &FakeFs, text: &str, exec: &Executor) -> BufferId {
        let path = notes_path();
        fake.insert_file(&path, text);
        let id = open_held(ws, &path, text, exec);
        let mtime = fake
            .metadata(&path)
            .expect("the file stats")
            .expect("the file exists")
            .modified;
        ws.buffers.set_disk_mtime(id, mtime);
        id
    }

    /// The body that a save of `ws` writes to [`notes_state_path`].
    fn saved_body(ws: &Workspace, fake: &FakeFs) -> String {
        let state_path = notes_state_path();
        ws.save_state(&state_path, fake).expect("the state saves");
        crate::action_handlers::read_string_via_host(fake, &state_path).expect("the state reads")
    }

    /// A state file holds each buffer's history and undo tree on one line, so a
    /// one-cursor undo group costs a few hundred bytes. One field per line at
    /// every depth spends about 2,700 on the same group.
    #[test]
    fn a_state_file_spends_under_800_bytes_per_single_edit_group() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/groups");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let id = open_held(&mut ws, &ws_dir.join("a.txt"), "", &exec);
        {
            let buffer = ws.buffers.get(id).expect("buffer open");
            let mut buffer = buffer.write().expect("buffer poisoned");
            let one_cursor = |at: Anchor| -> Arc<[Selection<Anchor>]> {
                Arc::from([Selection {
                    id: 0,
                    start: at,
                    end: at,
                    reversed: false,
                    goal: SelectionGoal::None,
                }])
            };
            for offset in 0..500 {
                let before = one_cursor(buffer.anchor_at(offset, Bias::Right));
                buffer.begin_group(before);
                buffer.edit(offset..offset, "x");
                let after = one_cursor(buffer.anchor_at(offset + 1, Bias::Right));
                buffer.seal_group(after);
            }
        }

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();
        let mut body = Vec::new();
        fake.read(&state_path, &mut body).unwrap();
        assert!(
            body.len() / 500 < 800,
            "{} bytes per group",
            body.len() / 500,
        );
    }

    /// A restart lists what was on screen rather than every file the workspace
    /// ever showed, so a save drops a clean buffer that no editor holds. A
    /// buffer with unsaved edits stays, so the save loses no edit.
    #[test]
    fn a_save_keeps_held_and_dirty_buffers_and_drops_hidden_clean_ones() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/hidden");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        open_held(&mut ws, &ws_dir.join("a.txt"), "alpha\n", &exec);
        let (id_b, buffer_b) = ws.buffers.open(&ws_dir.join("b.txt"), "beta\n");
        buffer_b.write().expect("buffer poisoned").edit(0..0, "x");
        ws.buffers.open(&ws_dir.join("c.txt"), "gamma\n");

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();
        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(
            (fresh.buffers.open_paths(), buffer_is_dirty(&fresh, id_b)),
            (vec![ws_dir.join("a.txt"), ws_dir.join("b.txt")], true),
            "the held and the edited buffers restore, and the hidden clean one does not",
        );
    }

    /// A restore brings back the buffers the save kept, which were held or
    /// edited when it ran, so the picker lists each one whatever holds it now.
    #[test]
    fn restored_buffers_are_listed_by_the_picker() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/listed");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        open_held(&mut ws, &ws_dir.join("a.txt"), "alpha\n", &exec);
        let (_, buffer_b) = ws.buffers.open(&ws_dir.join("b.txt"), "beta\n");
        buffer_b.write().expect("buffer poisoned").edit(0..0, "x");

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();
        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(
            fresh.buffers.listed_paths(&HashSet::new()),
            vec![ws_dir.join("a.txt"), ws_dir.join("b.txt")],
            "the clean restored buffer is listed with no editor holding it",
        );
    }

    /// A single-cursor jump at `offset` in buffer `id`, anchored the way a live
    /// jump is.
    fn jump_at(buffers: &BufferRegistry, id: BufferId, offset: usize) -> JumpEntry {
        let buffer = buffers.get(id).expect("buffer open");
        let anchor = buffer
            .read()
            .expect("buffer poisoned")
            .anchor_at(offset, Bias::Right);
        JumpEntry {
            buffer_id: id,
            selections: vec![Selection {
                id: 0,
                start: anchor,
                end: anchor,
                reversed: false,
                goal: SelectionGoal::None,
            }],
        }
    }

    /// The offset each entry of `jumplist` starts at, resolved against the
    /// buffers of `ws`.
    fn jump_offsets(ws: &Workspace, jumplist: &JumpList) -> Vec<usize> {
        jumplist
            .entries()
            .iter()
            .map(|jump| {
                let buffer = ws.buffers.get(jump.buffer_id).expect("buffer restores");
                let guard = buffer.read().expect("buffer poisoned");
                guard.resolve_anchor(&jump.selections[0].start)
            })
            .collect()
    }

    /// The jumplist is what `Ctrl-o` and `Ctrl-i` walk, so its entries, its
    /// cursor, and its push count all survive a restart.
    #[test]
    fn round_trip_keeps_each_panes_jumplist() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/jumps");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let id = open_held(
            &mut ws,
            &ws_dir.join("a.txt"),
            "0123456789abcdefghij",
            &exec,
        );
        let root = ws.panes.focus();
        for offset in [1, 5, 9] {
            let entry = jump_at(&ws.buffers, id, offset);
            ws.panes.pane_mut(root).jumplist.push(entry, &ws.buffers);
        }
        let live = jump_at(&ws.buffers, id, 9);
        ws.panes
            .pane_mut(root)
            .jumplist
            .backward(live, &ws.buffers, 1);

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();
        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        let jumplist = &fresh.panes.pane(fresh.panes.focus()).jumplist;
        assert_eq!(
            (
                jump_offsets(&fresh, jumplist),
                jumplist.cursor(),
                jumplist.generation(),
            ),
            (vec![1, 5, 9], 1, 3),
            "the entries, the cursor, and the push count survive",
        );
    }

    /// A save keeps only the buffers a pane holds or the user dirtied, so the
    /// restore drops every jump into another buffer, in a parked tab as in the
    /// active one, and the cursor stays at the tip of what is left.
    #[test]
    fn a_restored_jumplist_keeps_only_jumps_into_restored_buffers() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/jumps");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (unheld, _) = ws.buffers.open(&ws_dir.join("a.rs"), "0123456789");
        let held = open_held(&mut ws, &ws_dir.join("b.rs"), "0123456789", &exec);
        let push_jumps = |ws: &mut Workspace| {
            let pane = ws.panes.focus();
            for (id, offset) in [(unheld, 1), (held, 2), (unheld, 3)] {
                let entry = jump_at(&ws.buffers, id, offset);
                ws.panes.pane_mut(pane).jumplist.push(entry, &ws.buffers);
            }
            pane
        };
        let parked_pane = push_jumps(&mut ws);
        ws.new_tab(&exec);
        let active_pane = push_jumps(&mut ws);

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();
        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        let kept = |jumplist: &JumpList| {
            let buffers: Vec<BufferId> = jumplist
                .entries()
                .iter()
                .map(|jump| jump.buffer_id)
                .collect();
            (buffers, jump_offsets(&fresh, jumplist), jumplist.cursor())
        };
        let parked = fresh.tabs[0].parked.as_ref().expect("tab 0 parks");
        assert_eq!(
            [
                kept(&parked.pane(parked_pane).jumplist),
                kept(&fresh.panes.pane(active_pane).jumplist)
            ],
            [(vec![held], vec![2], 1), (vec![held], vec![2], 1)],
            "the parked tab and the active one each keep only the jump into b",
        );
    }

    /// A jump into a buffer whose history saves compacted names an insertion
    /// the seed does not hold, so the save re-anchors it as it does an
    /// editor's cursor, in a parked tab as in the active one. The jump inside
    /// the appended run is the discriminating one.
    #[test]
    fn a_compacted_buffer_keeps_its_jumps_in_every_tab() {
        use crate::buffer::OPS_COMPACT_THRESHOLD;

        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (id, buffer) = ws.buffers.open(&ws_dir.join("long.txt"), "abcdefghij\n");
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            for _ in 0..OPS_COMPACT_THRESHOLD {
                let end = guard.rope().len();
                guard.edit(end..end, "x");
            }
        }

        let first = ws.panes.focus();
        let entry = jump_at(&ws.buffers, id, 3);
        ws.panes.pane_mut(first).jumplist.push(entry, &ws.buffers);
        ws.new_tab(&exec);
        let second = ws.panes.focus();
        let entry = jump_at(&ws.buffers, id, OPS_COMPACT_THRESHOLD);
        ws.panes.pane_mut(second).jumplist.push(entry, &ws.buffers);

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();
        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        let parked = fresh.tabs[0].parked.as_ref().expect("tab 0 parks");
        assert_eq!(
            (
                jump_offsets(&fresh, &parked.pane(first).jumplist),
                jump_offsets(&fresh, &fresh.panes.pane(second).jumplist),
            ),
            (vec![3], vec![OPS_COMPACT_THRESHOLD]),
            "each jump resolves where it stood live",
        );
    }

    /// A rename is per-tab state that must survive a restart, and the second
    /// (parked) tab is the one whose name travels through the `tab_names` vec
    /// rather than being read off the active tree.
    #[test]
    fn a_renamed_tab_keeps_its_title_across_save_and_restore() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/named-tabs");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.new_tab(&exec);
        ws.tabs[0].name = Some("first".to_string());
        ws.tabs[1].name = Some("second".to_string());

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(fresh.tab_title(0), "first", "the parked tab keeps its name");
        assert_eq!(
            fresh.tab_title(1),
            "second",
            "the active tab keeps its name"
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

        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec, crate::test_notify());
        fresh
            .restore_state(&state_path, &fake, &exec)
            .expect("a legacy file still parses");

        assert_eq!(fresh.tabs.len(), 1);
        assert_eq!(fresh.active_tab, 0);
        assert!(fresh.tabs[0].parked.is_none());
        assert_eq!(fresh.last_tab, None);
    }

    /// A rebase saved with no pause was out in the stepper, so the restore
    /// drops it with a badge. A rebase stopped at a pause comes back.
    #[test]
    fn a_restored_rebase_with_no_pause_is_dropped() {
        let restored = |pause: Option<RebasePause>| {
            let exec = executor();
            let mut ws = new_laid_out_workspace(PathBuf::from("/rebasing"), &exec);
            ws.rebase_active = Some(ActiveRebase {
                workdir: PathBuf::from("/rebasing"),
                onto: "c1".to_string(),
                remaining: VecDeque::new(),
                current_head: "c2".to_string(),
                last_pick_sha: Some("c2".to_string()),
                last_message: None,
                pause,
                branch: None,
            });
            let state = ws.to_state();

            let mut fresh = new_laid_out_workspace(PathBuf::from("/rebasing"), &exec);
            fresh.apply_state(state, &exec);
            let badge = fresh
                .badges
                .find_by_source(BadgeSource::Review)
                .and_then(|id| fresh.badges.get(id))
                .map(|badge| badge.label.clone());
            (fresh.rebase_active.is_some(), badge)
        };

        let edit = RebasePause::Edit {
            cherry_picked_commit: "c2".to_string(),
        };
        assert_eq!(
            [restored(None), restored(Some(edit))],
            [
                (false, Some("rebase interrupted".to_string())),
                (true, None)
            ],
        );
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
        let editor_a = ws.editors.insert(EditorState::new(
            id_a,
            buf_a,
            exec.clone(),
            crate::test_notify(),
        ));
        let editor_b = ws.editors.insert(EditorState::new(
            id_b,
            buf_b,
            exec.clone(),
            crate::test_notify(),
        ));
        ws.editors[editor_a].scroll_row = 7;
        ws.editors[editor_b].scroll_row = 3;

        let root = ws.panes.focus();
        ws.panes.pane_mut(root).view = View::Editor(editor_a);
        let right = ws.panes.split(Axis::Vertical);
        ws.panes.pane_mut(right).view = View::Editor(editor_b);
        ws.focus = FocusTarget::SplitPane;

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec, crate::test_notify());
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

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
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

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
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

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
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

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(fresh.panes.pane_count(), count_before);
        assert!(
            fresh.panes.windowed_panes().is_empty(),
            "no pane stays windowed after restart"
        );
        assert_eq!(fresh.panes.split_pane_ids().len(), count_before);
    }

    fn buffer_text(ws: &Workspace, id: BufferId) -> String {
        let buffer = ws.buffers.get(id).expect("buffer missing");
        let guard = buffer.read().expect("buffer poisoned");
        guard.rope().to_string()
    }

    fn buffer_is_dirty(ws: &Workspace, id: BufferId) -> bool {
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

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
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

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
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
        let editor_id = ws.editors.insert(EditorState::new(
            id,
            buffer.clone(),
            exec.clone(),
            crate::test_notify(),
        ));

        let multi = MultiBuffer::singleton(buffer.clone());
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

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
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
        let restored_multi = MultiBuffer::singleton(restored_buffer);
        let restored_snap = restored_multi.snapshot();
        let offsets_after: Vec<usize> = fresh.editors[restored_editor_id]
            .selections
            .all_anchors()
            .iter()
            .map(|s| restored_snap.resolve_anchor(&s.start))
            .collect();
        assert_eq!(offsets_after, offsets_before);
    }

    /// A session written before the selection set recorded its primary still
    /// loads. The primary is recomputed as the highest-id selection rather than
    /// taken as the first.
    #[test]
    fn a_session_without_a_recorded_primary_loads_and_recomputes_it() {
        let fake = FakeFs::new();
        let exec = executor();
        let mut ws = new_laid_out_workspace(PathBuf::from("/test"), &exec);
        let (id, buffer) = ws.buffers.open(Path::new("/test/code.txt"), "abcdefghij\n");
        let editor_id = ws.editors.insert(EditorState::new(
            id,
            buffer.clone(),
            exec.clone(),
            crate::test_notify(),
        ));
        let snap = MultiBuffer::singleton(buffer).snapshot();
        let mut selections = SelectionsCollection::new();
        for offset in [3, 7] {
            selections.insert_cursor(
                snap.anchor_at(offset, Bias::Right),
                SelectionGoal::None,
                &snap,
            );
        }
        ws.editors[editor_id].selections = selections;
        let root = ws.panes.focus();
        ws.panes.pane_mut(root).view = View::Editor(editor_id);

        let state_path = PathBuf::from("/test/state.ron");
        ws.save_state(&state_path, &fake).unwrap();
        let legacy = {
            let mut buf = Vec::new();
            fake.read(&state_path, &mut buf).unwrap();
            let body = String::from_utf8(buf).unwrap();
            let kept: Vec<&str> = body
                .lines()
                .filter(|line| !line.contains("newest"))
                .collect();
            kept.join("\n")
        };
        fake.write(&state_path, legacy.as_bytes()).unwrap();

        let mut fresh = Workspace::new(PathBuf::from("/test"), &exec, crate::test_notify());
        fresh
            .restore_state(&state_path, &fake, &exec)
            .expect("a file without newest still loads");

        let editor = match fresh.panes.pane(fresh.panes.focus()).view {
            View::Editor(id) => &fresh.editors[id],
            _ => panic!("the restored pane shows an editor"),
        };
        let restored =
            MultiBuffer::singleton(fresh.buffers.get(editor.buffer_id).expect("buffer")).snapshot();
        let anchors = editor.selections.all_anchors();
        let primary = &anchors[editor.selections.primary_index()];
        assert_eq!(
            (restored.resolve_anchor(&primary.start), primary.id),
            (
                7,
                anchors.iter().map(|sel| sel.id).max().expect("a selection")
            ),
        );
    }

    /// The snapshot runs on the UI thread, so it must not copy an open file's
    /// bytes. Every entry carrying a rope seed is what says it did not.
    #[test]
    fn a_snapshot_carries_each_open_file_as_a_rope_seed() {
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let a = open_held(&mut ws, &ws_dir.join("a.txt"), "alpha\n", &exec);
        let b = open_held(&mut ws, &ws_dir.join("b.txt"), "beta\n", &exec);

        let state = ws.to_state();
        // A buffer loaded empty pushed no op and so stands in for nothing. The
        // workspace's own scratch is one, which is why this keys on the files.
        let seed = |id| {
            state
                .buffers
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .and_then(|entry| entry.history.seed.as_ref())
                .map(ToString::to_string)
        };

        assert_eq!(
            (seed(a), seed(b)),
            (Some("alpha\n".to_owned()), Some("beta\n".to_owned())),
            "each open file rides as a rope handle rather than a copy",
        );
    }

    #[test]
    fn clean_buffer_has_single_insert_op() {
        use crate::buffer::BufferOp;

        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let file = ws_dir.join("untouched.txt");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let id = open_held(&mut ws, &file, "hello world\n", &exec);
        assert!(!buffer_is_dirty(&ws, id));

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
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

    /// The snapshot is taken on the run loop, so the half of a compacted
    /// reanchor that rebuilds a buffer must not happen there. What it leaves
    /// behind is the resolved endpoints, and resolving them lands the same
    /// anchors the eager pass produced.
    #[test]
    fn a_snapshot_defers_the_reanchor_and_resolving_it_lands_the_same_anchors() {
        use crate::{
            buffer::OPS_COMPACT_THRESHOLD, multi_buffer::MultiBuffer,
            selection::SelectionsCollection,
        };
        use stoat_text::{Bias, SelectionGoal};

        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (id, buffer) = ws.buffers.open(&ws_dir.join("long.txt"), "abcdefghij\n");
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            for _ in 0..OPS_COMPACT_THRESHOLD {
                let end = guard.rope().len();
                guard.edit(end..end, "x");
            }
        }

        let editor_id = ws.editors.insert(EditorState::new(
            id,
            buffer.clone(),
            exec.clone(),
            crate::test_notify(),
        ));
        let multi = MultiBuffer::singleton(buffer.clone());
        let multi_snap = multi.snapshot();
        let mut selections = SelectionsCollection::new();
        for offset in [3, OPS_COMPACT_THRESHOLD] {
            selections.insert_cursor(
                multi_snap.anchor_at(offset, Bias::Right),
                SelectionGoal::None,
                &multi_snap,
            );
        }
        ws.editors[editor_id].selections = selections;

        let mut state = ws.to_state();
        assert_eq!(
            state.pending_reanchors.len(),
            1,
            "the shown compacted buffer leaves its editor's endpoints owed",
        );

        // The seed the restore replays is what the resolved anchors have to
        // read against, so the offsets come off a buffer built from it.
        let entry = state
            .buffers
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .expect("the buffer is in the snapshot");
        let seed = TextBuffer::from_history(id, &entry.history);
        let live = buffer.read().expect("buffer poisoned");
        let expected: Vec<usize> = ws.editors[editor_id]
            .selections
            .anchors()
            .map(|anchor| {
                seed.resolve_anchor(&seed.anchor_at(live.resolve_anchor(anchor), anchor.bias))
            })
            .collect();
        drop(live);

        state.resolve_reanchors();
        assert!(
            state.pending_reanchors.is_empty(),
            "and resolving spends them",
        );

        let (_, editor) = state
            .editors
            .iter()
            .find(|(eid, _)| *eid == editor_id)
            .expect("the editor is in the snapshot");
        let landed: Vec<usize> = editor
            .selections
            .anchors()
            .map(|anchor| seed.resolve_anchor(anchor))
            .collect();
        assert_eq!(
            landed, expected,
            "the deferred half lands what the eager pass did",
        );
    }

    /// The op log is what carries selections across a restart, so bounding it
    /// and keeping the cursor where the user left it are one property, not two.
    /// The discriminating cursor is the one inside the appended run. Its anchor
    /// names a one-byte insertion the seed does not contain, so a compaction
    /// that did not re-anchor would land it at the top of the file.
    #[test]
    fn a_compacted_buffer_persists_bounded_and_keeps_its_selections() {
        use crate::{
            buffer::OPS_COMPACT_THRESHOLD, multi_buffer::MultiBuffer,
            selection::SelectionsCollection,
        };
        use stoat_text::{Bias, SelectionGoal};

        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let file = ws_dir.join("long.txt");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (id, buffer) = ws.buffers.open(&file, "abcdefghij\n");
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            for _ in 0..OPS_COMPACT_THRESHOLD {
                let end = guard.rope().len();
                guard.edit(end..end, "x");
            }
        }

        let editor_id = ws.editors.insert(EditorState::new(
            id,
            buffer.clone(),
            exec.clone(),
            crate::test_notify(),
        ));
        let multi = MultiBuffer::singleton(buffer.clone());
        let multi_snap = multi.snapshot();
        let mut selections = SelectionsCollection::new();
        for offset in [3, OPS_COMPACT_THRESHOLD] {
            selections.insert_cursor(
                multi_snap.anchor_at(offset, Bias::Right),
                SelectionGoal::None,
                &multi_snap,
            );
        }
        ws.editors[editor_id].selections = selections;

        let spans_before: Vec<(usize, usize)> = ws.editors[editor_id]
            .selections
            .all_anchors()
            .iter()
            .map(|s| {
                (
                    multi_snap.resolve_anchor(&s.start),
                    multi_snap.resolve_anchor(&s.end),
                )
            })
            .collect();

        let root = ws.panes.focus();
        ws.panes.pane_mut(root).view = View::Editor(editor_id);

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
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

        assert_eq!(
            buffer_text(&fresh, restored_bid),
            multi_snap.text(),
            "the seed restores the text the log built"
        );
        assert_eq!(
            restored_buffer
                .read()
                .expect("buffer poisoned")
                .history()
                .ops
                .len(),
            1,
            "the persisted log stays bounded across the restart"
        );

        let restored_multi = MultiBuffer::singleton(restored_buffer);
        let restored_snap = restored_multi.snapshot();
        let spans_after: Vec<(usize, usize)> = fresh.editors[restored_editor_id]
            .selections
            .all_anchors()
            .iter()
            .map(|s| {
                (
                    restored_snap.resolve_anchor(&s.start),
                    restored_snap.resolve_anchor(&s.end),
                )
            })
            .collect();
        assert_eq!(spans_after, spans_before);
    }

    #[test]
    fn scratch_buffer_history_round_trips() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (scratch_id, scratch_buf) = ws.buffers.new_scratch();
        {
            let mut guard = scratch_buf.write().expect("buffer poisoned");
            guard.edit(0..0, "notes\n");
            guard.edit(6..6, "more\n");
        }
        assert_eq!(buffer_text(&ws, scratch_id), "notes\nmore\n");

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(buffer_text(&fresh, scratch_id), "notes\nmore\n");
        let buffer = fresh.buffers.get(scratch_id).expect("scratch lost");
        let guard = buffer.read().expect("buffer poisoned");
        assert!(guard.dirty);
    }

    /// Neither of these can be recovered after a restore. The rope was
    /// normalised to LF on open so the text no longer says which terminator it
    /// arrived with, and re-opening the path finds it already registered and
    /// leaves both alone. So a session that drops them leaves the buffer
    /// rewriting a CRLF file whole on its first save, with the guard that
    /// refuses to overwrite an externally changed file permanently disarmed.
    ///
    /// The buffer holds an edit, so its entry carries its text inline rather
    /// than naming the file.
    #[test]
    fn a_restored_buffer_keeps_its_line_ending_and_disk_baseline() {
        use std::time::{Duration, UNIX_EPOCH};
        use stoat_text::LineEnding;

        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let file = ws_dir.join("crlf.txt");
        let exec = executor();
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let id = open_held(&mut ws, &file, "one\ntwo\n", &exec);
        ws.buffers
            .get(id)
            .expect("the buffer opens")
            .write()
            .expect("buffer poisoned")
            .edit(0..0, "zero\n");
        ws.buffers.set_line_ending(id, LineEnding::Crlf);
        ws.buffers.set_disk_mtime(id, mtime);

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(fresh.buffers.line_ending(id), LineEnding::Crlf);
        assert_eq!(fresh.buffers.disk_mtime(id), Some(mtime));
    }

    /// A session written before those two fields existed still loads, and its
    /// buffers restore the way they did then.
    #[test]
    fn a_state_file_without_the_line_ending_field_loads() {
        use stoat_text::LineEnding;

        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let file = ws_dir.join("legacy.txt");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let id = open_held(&mut ws, &file, "one\n", &exec);
        ws.buffers.set_line_ending(id, LineEnding::Crlf);

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let body = {
            let mut raw = Vec::new();
            fake.read(&state_path, &mut raw).expect("state readable");
            let text = String::from_utf8(raw).expect("state is utf8");
            assert!(text.contains("line_ending"), "the field is written");
            text.replace("line_ending: Crlf,", "")
                .replace("disk_mtime: None,", "")
        };
        fake.insert_file(&state_path, body.as_bytes());

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(fresh.buffers.line_ending(id), LineEnding::Lf);
        assert_eq!(fresh.buffers.disk_mtime(id), None);
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
        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
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

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(fresh.name, "my workspace");
    }

    #[test]
    fn parent_round_trips_through_save_and_restore() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.parent = Some(WorkspaceUid(7));
        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(fresh.parent, Some(WorkspaceUid(7)));
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

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
        assert_eq!(
            fresh.last_finder_scope, None,
            "fresh workspace remembers nothing"
        );
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(fresh.last_finder_scope, Some("modified".to_string()));
    }

    #[test]
    fn remote_target_round_trips_through_save_and_restore() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let target = RemoteTarget {
            transport: crate::ssh::Transport::Ssh,
            host: "box".to_string(),
            args: vec!["~/p".to_string()],
        };
        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.remote = Some(target.clone());
        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
        assert_eq!(fresh.remote, None, "fresh workspace is local");
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(fresh.remote, Some(target));
    }

    #[test]
    fn a_state_file_without_the_remote_field_restores_a_local_workspace() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.remote = Some(RemoteTarget {
            transport: crate::ssh::Transport::Mosh,
            host: "box".to_string(),
            args: Vec::new(),
        });
        let state_path = ws_dir.join("state.ron");

        let body = ron::ser::to_string(&LegacyState::from(ws.to_state())).unwrap();
        assert!(!body.contains("remote"), "no remote field on the wire");
        fake.write(&state_path, body.as_bytes()).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(fresh.remote, None, "a file predating the field is local");
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

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
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
    fn search_history_round_trips_through_save_and_restore() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.search_history = InputHistory::from_entries(vec!["fn main".into(), "unwrap".into()]);
        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
        assert!(
            fresh.search_history.entries().is_empty(),
            "fresh workspace remembers nothing"
        );
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(
            fresh.search_history.entries().to_vec(),
            ["fn main", "unwrap"]
        );
    }

    #[test]
    fn code_search_history_round_trips_through_save_and_restore() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.code_search_history = InputHistory::from_entries(vec!["impl Foo".into()]);
        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
        assert!(
            fresh.code_search_history.entries().is_empty(),
            "fresh workspace remembers nothing"
        );
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(fresh.code_search_history.entries().to_vec(), ["impl Foo"]);
    }
    #[test]
    fn legacy_state_without_search_history_loads_empty() {
        let exec = executor();
        let ws = new_laid_out_workspace(PathBuf::from("/test"), &exec);

        let body =
            ron::ser::to_string_pretty(&ws.to_state(), ron::ser::PrettyConfig::default()).unwrap();
        let legacy: String = body
            .lines()
            .filter(|line| !line.contains("search_history"))
            .collect::<Vec<_>>()
            .join("\n");

        let state: WorkspaceStateV1 =
            ron::from_str(&legacy).expect("a file without search_history still loads");
        assert!(state.search_history.is_empty());
    }

    #[test]
    fn the_active_search_round_trips_through_save_and_restore() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.last_search = Some(LastSearch::new(
            "fn main".into(),
            SearchDirection::Reverse,
            true,
        ));
        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(
            fresh
                .last_search
                .as_ref()
                .map(|last| (last.query.as_str(), last.direction)),
            Some(("fn main", SearchDirection::Reverse)),
            "the restored session repeats the same pattern the same way",
        );
    }

    #[test]
    fn legacy_state_without_last_search_loads_none() {
        let exec = executor();
        let ws = new_laid_out_workspace(PathBuf::from("/test"), &exec);

        let body =
            ron::ser::to_string_pretty(&ws.to_state(), ron::ser::PrettyConfig::default()).unwrap();
        let legacy: String = body
            .lines()
            .filter(|line| !line.contains("last_search"))
            .collect::<Vec<_>>()
            .join("\n");

        let state: WorkspaceStateV1 =
            ron::from_str(&legacy).expect("a file without last_search still loads");
        assert!(state.last_search.is_none());
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

        let mut fresh = Workspace::new(ws_dir.clone(), &exec, crate::test_notify());
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
