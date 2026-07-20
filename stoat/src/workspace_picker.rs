use crate::{
    fuzzy,
    input_view::InputView,
    paths,
    workspace::{registry::RegistryEntry, Workspace, WorkspaceId, WorkspaceUid},
};
use slotmap::SlotMap;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Where a listed workspace stands relative to the running instance.
///
/// Ordered so a sort lists [`Active`](Self::Active) first, then
/// [`Background`](Self::Background), then [`Inactive`](Self::Inactive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkspaceStatus {
    /// The focused workspace.
    Active,
    /// Open in the instance but not focused.
    Background,
    /// Persisted on disk, not open in any instance.
    Inactive,
}

/// Finder-style modal listing open and inactive workspaces, rendered as a
/// centered overlay when the `SwitchWorkspace` action fires.
///
/// A fuzzy filter input sits atop the list. Typed characters route into
/// [`Self::input`] via the insert-mode keymap, and [`Self::refilter`] re-ranks
/// the list against the query each idle tick. Navigation and selection route
/// through the `modal == workspace_picker` keymap block. [`Self::select_next`]
/// and [`Self::select_prev`] move the highlight, and [`Self::selected_id`]
/// reports the row to switch to.
pub struct WorkspacePicker {
    pub(crate) input: InputView,
    entries: Vec<PickerEntry>,
    /// Indices into `entries` in display order after fuzzy filtering. An empty
    /// query lists every entry in the status-grouped order.
    filtered: Vec<usize>,
    /// Matched character offsets in each filtered row's haystack, parallel to
    /// [`Self::filtered`], driving name-column highlights.
    match_indices: Vec<Vec<u32>>,
    /// Cursor into [`Self::filtered`].
    selected: usize,
}

/// One row in the picker. Built up-front from the workspace slotmap so the
/// picker owns its own display data and doesn't borrow from [`Stoat`] for
/// its lifetime.
pub struct PickerEntry {
    /// The open workspace's id, or `None` for an inactive on-disk row.
    pub id: Option<WorkspaceId>,
    pub basename: String,
    pub git_root: PathBuf,
    pub uid: WorkspaceUid,
    pub status: WorkspaceStatus,
    pub buffer_count: usize,
    pub run_count: usize,
    pub editor_count: usize,
    /// The state file an inactive row restores from. `None` for open rows.
    pub state_path: Option<PathBuf>,
    /// State file mtime, ordering inactive rows newest first. Epoch for open
    /// rows, which sort by name instead.
    pub mtime: SystemTime,
}

/// Rendering strategy for the picker's per-row path column. Selected once
/// per open by [`WorkspacePicker::path_display`] based on the relationship
/// between every entry's `git_root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathDisplay {
    /// Every entry shares the same `git_root`; callers should drop the path
    /// column outright because every row would render identically.
    Omit,
    /// Rows share a common ancestor; callers should render each row as the
    /// suffix of its `git_root` below the stored ancestor.
    Relative(PathBuf),
    /// No useful common ancestor; each row renders independently with
    /// `~/<tail>` abbreviation for paths under the user's home directory.
    TildeAbsolute,
}

impl WorkspacePicker {
    /// Build the picker from the open workspaces and the on-disk registry.
    ///
    /// Open workspaces list as [`WorkspaceStatus::Active`] (the focused one) or
    /// [`WorkspaceStatus::Background`]. Registry entries whose uid is not open
    /// list as [`WorkspaceStatus::Inactive`], so an open workspace wins over its
    /// own on-disk sidecar. Rows order Active first, then Background by name,
    /// then Inactive newest state file first.
    ///
    /// The `input` is built by the caller because listing borrows every
    /// workspace immutably, while creating the input needs the active workspace
    /// mutably.
    pub(crate) fn new(
        workspaces: &SlotMap<WorkspaceId, Workspace>,
        active: WorkspaceId,
        inactive: Vec<RegistryEntry>,
        input: InputView,
    ) -> Self {
        let mut entries: Vec<PickerEntry> = workspaces
            .iter()
            .map(|(id, ws)| PickerEntry {
                id: Some(id),
                basename: display_name(&ws.name, &ws.git_root),
                git_root: ws.git_root.clone(),
                uid: ws.uid,
                status: if id == active {
                    WorkspaceStatus::Active
                } else {
                    WorkspaceStatus::Background
                },
                buffer_count: ws.buffers.len(),
                run_count: ws.runs.len(),
                editor_count: ws.editors.len(),
                state_path: None,
                mtime: UNIX_EPOCH,
            })
            .collect();

        let open_uids: HashSet<WorkspaceUid> = entries.iter().map(|e| e.uid).collect();
        for reg in inactive {
            if open_uids.contains(&reg.meta.uid) {
                continue;
            }
            entries.push(PickerEntry {
                id: None,
                basename: display_name(&reg.meta.name, &reg.meta.git_root),
                git_root: reg.meta.git_root,
                uid: reg.meta.uid,
                status: WorkspaceStatus::Inactive,
                buffer_count: reg.meta.buffer_count,
                run_count: 0,
                editor_count: 0,
                state_path: Some(reg.state_path),
                mtime: reg.mtime,
            });
        }

        entries.sort_by(|a, b| {
            a.status
                .cmp(&b.status)
                .then_with(|| match a.status {
                    WorkspaceStatus::Inactive => b.mtime.cmp(&a.mtime),
                    _ => a.basename.cmp(&b.basename),
                })
                .then_with(|| a.uid.0.cmp(&b.uid.0))
        });

        let selected = entries
            .iter()
            .position(|e| e.status == WorkspaceStatus::Active)
            .unwrap_or(0);

        let mut picker = Self {
            input,
            entries,
            filtered: Vec::new(),
            match_indices: Vec::new(),
            selected,
        };
        picker.refilter("");
        picker
    }

    pub fn entries(&self) -> &[PickerEntry] {
        &self.entries
    }

    pub fn filtered(&self) -> &[usize] {
        &self.filtered
    }

    pub fn match_indices(&self) -> &[Vec<u32>] {
        &self.match_indices
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Re-rank the entries for `query`, matches first by score descending then
    /// by status-grouped order. An empty or unusable query lists every entry in
    /// the status-grouped order with no highlights.
    ///
    /// The haystack per row is `"{basename} {git_root}"`, so a query narrows on
    /// either the workspace name or any part of its root path. Match highlights
    /// land only on name-column indices when painted.
    pub(crate) fn refilter(&mut self, query: &str) {
        let items = self
            .entries
            .iter()
            .enumerate()
            .map(|(idx, e)| (idx, format!("{} {}", e.basename, e.git_root.display())));

        match fuzzy::match_and_rank(query, items) {
            Some(mut matches) => {
                matches.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.item.cmp(&b.item)));
                self.filtered = matches.iter().map(|m| m.item).collect();
                self.match_indices = matches.into_iter().map(|m| m.matched_indices).collect();
            },
            None => {
                self.filtered = (0..self.entries.len()).collect();
                self.match_indices = vec![Vec::new(); self.entries.len()];
            },
        }

        self.clamp_selected();
    }

    /// Free the filter input's editor and scratch buffer. Called when the picker
    /// closes so the transient input leaves no unsaved buffer behind.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
    }

    /// The open workspace under the selection, or `None` when the picker is
    /// empty or the selected row is an inactive on-disk workspace.
    pub fn selected_id(&self) -> Option<WorkspaceId> {
        self.selected_entry().and_then(|entry| entry.id)
    }

    /// The full row under the selection, so the caller can distinguish an
    /// inactive row (with a state path to restore) from an open one.
    pub fn selected_entry(&self) -> Option<&PickerEntry> {
        let idx = *self.filtered.get(self.selected)?;
        self.entries.get(idx)
    }

    pub fn select_next(&mut self) {
        move_selection(self.filtered.len(), &mut self.selected, 1);
    }

    pub fn select_prev(&mut self) {
        move_selection(self.filtered.len(), &mut self.selected, -1);
    }

    fn clamp_selected(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len() - 1;
        }
    }

    /// How the per-row path column should render for this picker's entries.
    ///
    /// When every row has an identical `git_root`, returns [`PathDisplay::Omit`]
    /// so callers can drop the column entirely: the basename already carries
    /// the only distinguishing information. When there's a shared ancestor
    /// beyond the filesystem root, returns [`PathDisplay::Relative`] so rows
    /// render as the tail below that ancestor. Otherwise returns
    /// [`PathDisplay::TildeAbsolute`] so rows render each path independently
    /// with `~` abbreviation for home.
    pub fn path_display(&self) -> PathDisplay {
        let roots: Vec<&Path> = self.entries.iter().map(|e| e.git_root.as_path()).collect();

        let all_same = roots
            .first()
            .is_some_and(|first| roots.iter().all(|r| r == first));
        if all_same {
            return PathDisplay::Omit;
        }

        match paths::common_ancestor(roots.iter().copied()) {
            Some(ancestor) => PathDisplay::Relative(ancestor),
            None => PathDisplay::TildeAbsolute,
        }
    }

    pub fn hint_bindings(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Enter", "select".to_string()),
            ("Esc", "cancel".to_string()),
            ("Ctrl-N", "next".to_string()),
            ("Ctrl-P", "prev".to_string()),
            ("\u{2193}", "next".to_string()),
            ("\u{2191}", "prev".to_string()),
        ]
    }
}

/// A workspace's display name, its explicit name or its git root's basename
/// when unnamed.
fn display_name(name: &str, git_root: &Path) -> String {
    if !name.is_empty() {
        return name.to_string();
    }
    git_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("(unnamed)")
        .to_string()
}

fn move_selection(len: usize, selected: &mut usize, delta: i32) {
    if len == 0 {
        *selected = 0;
        return;
    }
    let max = (len - 1) as i32;
    let next = (*selected as i32 + delta).clamp(0, max);
    *selected = next as usize;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::BufferId, editor_state::EditorId, input_view::SubmitTarget, workspace::Workspace,
    };
    use std::sync::Arc;
    use stoat_scheduler::{Executor, TestScheduler};

    fn executor() -> Executor {
        Arc::new(TestScheduler::new()).executor()
    }

    fn dummy_input() -> InputView {
        InputView {
            editor_id: EditorId::default(),
            buffer_id: BufferId::new(0),
            target: SubmitTarget::WorkspacePicker,
            max_height: 1,
        }
    }

    fn slotmap_with_two(exec: &Executor) -> (SlotMap<WorkspaceId, Workspace>, WorkspaceId) {
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let a = workspaces.insert(Workspace::new(PathBuf::from("/tmp/alpha"), exec));
        workspaces[a].id = a;
        let b = workspaces.insert(Workspace::new(PathBuf::from("/tmp/beta"), exec));
        workspaces[b].id = b;
        (workspaces, a)
    }

    #[test]
    fn new_lists_all_workspaces_current_first() {
        let exec = executor();
        let (workspaces, active) = slotmap_with_two(&exec);
        let picker = WorkspacePicker::new(&workspaces, active, Vec::new(), dummy_input());

        let entries = picker.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].status, WorkspaceStatus::Active);
        assert_eq!(entries[0].id, Some(active));
        assert_eq!(entries[1].status, WorkspaceStatus::Background);
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn new_merges_registry_dedupes_open_and_orders_inactive_newest_first() {
        use crate::workspace::registry::{RegistryEntry, WorkspaceMeta};
        use std::time::Duration;

        let exec = executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let a = workspaces.insert(Workspace::new(PathBuf::from("/tmp/alpha"), &exec));
        workspaces[a].id = a;
        let open_uid = workspaces[a].uid;

        let entry = |uid: u64, name: &str, root: &str, secs: u64| RegistryEntry {
            meta: WorkspaceMeta {
                uid: WorkspaceUid(uid),
                name: name.into(),
                git_root: PathBuf::from(root),
                buffer_count: 3,
            },
            state_path: PathBuf::from(root).join("s.ron"),
            mtime: UNIX_EPOCH + Duration::from_secs(secs),
        };

        let inactive = vec![
            entry(open_uid.0, "shadow", "/tmp/alpha", 100),
            entry(9, "old-proj", "/tmp/old", 50),
            entry(8, "new-proj", "/tmp/new", 90),
        ];

        let picker = WorkspacePicker::new(&workspaces, a, inactive, dummy_input());
        let entries = picker.entries();

        assert_eq!(
            entries.len(),
            3,
            "the open workspace's shadow sidecar dedupes"
        );
        assert_eq!(entries[0].status, WorkspaceStatus::Active);
        assert_eq!(entries[0].id, Some(a));

        assert_eq!(entries[1].status, WorkspaceStatus::Inactive);
        assert_eq!(
            entries[1].basename, "new-proj",
            "inactive rows are newest first"
        );
        assert_eq!(entries[1].id, None);
        assert_eq!(entries[1].buffer_count, 3);
        assert_eq!(entries[2].basename, "old-proj");
    }

    #[test]
    fn select_next_prev_clamp_at_ends() {
        let exec = executor();
        let (workspaces, active) = slotmap_with_two(&exec);
        let mut picker = WorkspacePicker::new(&workspaces, active, Vec::new(), dummy_input());

        picker.select_next();
        assert_eq!(picker.selected(), 1);
        picker.select_next();
        assert_eq!(picker.selected(), 1);
        picker.select_prev();
        assert_eq!(picker.selected(), 0);
        picker.select_prev();
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn selected_id_tracks_selection() {
        let exec = executor();
        let (workspaces, active) = slotmap_with_two(&exec);
        let mut picker = WorkspacePicker::new(&workspaces, active, Vec::new(), dummy_input());

        assert_eq!(picker.selected_id(), Some(active));
        picker.select_next();
        assert_eq!(picker.selected_id(), picker.entries()[1].id);
    }

    #[test]
    fn single_workspace_picker_lists_only_current() {
        let exec = executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let a = workspaces.insert(Workspace::new(PathBuf::from("/tmp/alpha"), &exec));
        workspaces[a].id = a;

        let picker = WorkspacePicker::new(&workspaces, a, Vec::new(), dummy_input());
        assert_eq!(picker.entries().len(), 1);
        assert_eq!(picker.entries()[0].status, WorkspaceStatus::Active);
    }

    fn picker_with_roots(roots: &[&str]) -> WorkspacePicker {
        let exec = executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let mut first = None;
        for root in roots {
            let id = workspaces.insert(Workspace::new(PathBuf::from(*root), &exec));
            workspaces[id].id = id;
            first.get_or_insert(id);
        }
        let active = first.expect("at least one workspace");
        WorkspacePicker::new(&workspaces, active, Vec::new(), dummy_input())
    }

    #[test]
    fn path_display_omits_when_all_identical() {
        let picker = picker_with_roots(&["/tmp/alpha", "/tmp/alpha"]);
        assert_eq!(picker.path_display(), PathDisplay::Omit);
    }

    #[test]
    fn path_display_relative_when_shared_ancestor() {
        let picker = picker_with_roots(&["/tmp/alpha", "/tmp/beta"]);
        assert_eq!(
            picker.path_display(),
            PathDisplay::Relative(PathBuf::from("/tmp"))
        );
    }

    #[test]
    fn path_display_tilde_when_divergent() {
        let picker = picker_with_roots(&["/tmp/alpha", "/var/beta"]);
        assert_eq!(picker.path_display(), PathDisplay::TildeAbsolute);
    }

    #[test]
    fn refilter_narrows_reranks_and_clamps_selection() {
        let exec = executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let mut active = None;
        for root in ["/tmp/alpha", "/tmp/beta", "/tmp/gamma"] {
            let id = workspaces.insert(Workspace::new(PathBuf::from(root), &exec));
            workspaces[id].id = id;
            workspaces[id].name = String::new();
            active.get_or_insert(id);
        }
        let active = active.expect("at least one workspace");
        let mut picker = WorkspacePicker::new(&workspaces, active, Vec::new(), dummy_input());
        picker.select_next();
        picker.select_next();
        assert_eq!(picker.selected(), 2);

        picker.refilter("alpha");
        assert_eq!(picker.filtered().len(), 1, "only alpha matches");
        assert_eq!(
            picker.selected_entry().map(|e| e.basename.as_str()),
            Some("alpha")
        );
        assert_eq!(
            picker.selected(),
            0,
            "the stale selection clamps into the shorter list"
        );

        picker.refilter("");
        assert_eq!(picker.filtered().len(), 3, "empty query restores every row");
    }
}
