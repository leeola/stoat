use crate::{
    fuzzy,
    input_view::InputView,
    picker,
    workspace::{registry::RegistryEntry, Workspace, WorkspaceId, WorkspaceUid},
};
use slotmap::SlotMap;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Where a listed workspace stands relative to the running instance.
///
/// The picker lists the [`Active`](Self::Active) row first and every other row
/// by recency, so a [`Background`](Self::Background) workspace and an
/// [`Inactive`](Self::Inactive) saved session interleave by time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceStatus {
    /// The focused workspace.
    Active,
    /// Open in the instance but not focused.
    Background,
    /// Persisted on disk, not open in any instance.
    Inactive,
}

/// The rows the workspace picker lists.
///
/// A family is a session that a launch opened, its family root, together with
/// every session created inside it. The family scope keys on that tie and not
/// on the root path, so two unrelated sessions in one project stay apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerScope {
    /// Every open workspace and saved session.
    All,
    /// The active workspace's family alone.
    Family,
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
///
/// The picker opens on the row after the active one, the session focused
/// before it, so Enter at once returns there. Enter on a query with no row
/// creates a session of that name.
///
/// [`Self::toggle_scope`] narrows the list to the active workspace's family and
/// back.
pub struct WorkspacePicker {
    pub(crate) input: InputView,
    entries: Vec<PickerEntry>,
    /// What a query is matched against, one per entry and parallel to them.
    ///
    /// Derived from `entries`, which are written once at construction, so these
    /// are built there too. Ranking runs from the idle tick, and rebuilding
    /// them per call would allocate a string per workspace a frame.
    haystacks: Vec<String>,
    /// Indices into `entries` in display order after fuzzy filtering. An empty
    /// query lists every entry in scope in the status-grouped order.
    filtered: Vec<usize>,
    /// Matched character offsets in each filtered row's haystack, parallel to
    /// [`Self::filtered`], driving name-column highlights.
    match_indices: Vec<Vec<u32>>,
    /// The parse behind the current ranking, so a painted row past the indexed
    /// block derives its offsets rather than going unhighlighted.
    last_pattern: Option<fuzzy::Pattern>,
    /// Cursor into [`Self::filtered`].
    selected: usize,
    /// Query the current ranking came from, so the idle tick's repeat call
    /// re-ranks nothing. `None` until the first ranking.
    last_filter_query: Option<String>,
    /// Rows the last render painted, stamped by the renderer and read by
    /// [`Self::page`]. `None` until the first frame, since the modal sizes
    /// itself to its entries and only render knows how many fit.
    pub(crate) viewport_rows: Option<usize>,
    /// The open workspace whose row [`Self::omit_active`] dropped.
    ///
    /// The row is omitted only for a workspace that holds nothing and that no
    /// saved session restores into. A switch away from such a workspace leaves
    /// it with no row, no save, and no way back.
    pub(crate) omitted_workspace: Option<WorkspaceId>,
    /// The entry under rename, an index into `entries`, while the input holds a
    /// new name instead of the filter query.
    renaming: Option<usize>,
    /// The filter query that a rename replaced, which goes back into the input
    /// when the rename ends.
    pub(crate) filter_query: String,
    /// Whether the list holds every session or the active workspace's family
    /// alone.
    scope: PickerScope,
    /// The family that [`PickerScope::Family`] lists, the active workspace's
    /// family root. The picker's input lives in the active workspace, so the
    /// active workspace stays the same while the picker is open.
    family_root: WorkspaceUid,
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
    /// The family root the session was created from, `None` for a session a
    /// launch opened.
    pub parent: Option<WorkspaceUid>,
    pub status: WorkspaceStatus,
    pub buffer_count: usize,
    pub run_count: usize,
    pub editor_count: usize,
    /// The state file an inactive row restores from. `None` for open rows.
    pub state_path: Option<PathBuf>,
    /// The state file's mtime for an inactive row, and the last focus instant
    /// for an open one. Every row after the active one orders by it, newest
    /// first.
    pub mtime: SystemTime,
    /// Host this workspace's window was last handed to, or `None` when it is
    /// local. Picking such a row hands the window straight back to that host,
    /// which the path column says by reading `host:path`.
    pub remote_host: Option<String>,
}

impl PickerEntry {
    /// The uid that names this row's family, its parent's or its own.
    fn family_root(&self) -> WorkspaceUid {
        self.parent.unwrap_or(self.uid)
    }
}

impl WorkspacePicker {
    /// Build the picker from the open workspaces and the on-disk registry.
    ///
    /// Open workspaces list as [`WorkspaceStatus::Active`] (the focused one) or
    /// [`WorkspaceStatus::Background`]. Registry entries whose uid is not open
    /// list as [`WorkspaceStatus::Inactive`], so an open workspace wins over its
    /// own on-disk sidecar. The active row leads, and every other row follows
    /// newest first, an open row by its last focus and a saved one by its state
    /// file. The selection starts on the first row after the active one.
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
                parent: ws.parent,
                status: if id == active {
                    WorkspaceStatus::Active
                } else {
                    WorkspaceStatus::Background
                },
                buffer_count: ws.buffers.len(),
                run_count: ws.runs.len(),
                editor_count: ws.editors.len(),
                state_path: None,
                mtime: ws.last_active,
                remote_host: ws.remote.as_ref().map(|target| target.host.clone()),
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
                parent: reg.meta.parent,
                status: WorkspaceStatus::Inactive,
                buffer_count: reg.meta.buffer_count,
                run_count: 0,
                editor_count: 0,
                state_path: Some(reg.state_path),
                mtime: reg.mtime,
                remote_host: reg.meta.remote_host,
            });
        }

        entries.sort_by(|a, b| {
            (a.status != WorkspaceStatus::Active)
                .cmp(&(b.status != WorkspaceStatus::Active))
                .then_with(|| b.mtime.cmp(&a.mtime))
                .then_with(|| a.uid.0.cmp(&b.uid.0))
        });

        let selected = entries
            .iter()
            .position(|e| e.status != WorkspaceStatus::Active)
            .unwrap_or(0);

        let haystacks = entries.iter().map(haystack).collect();

        let mut picker = Self {
            input,
            entries,
            haystacks,
            filtered: Vec::new(),
            match_indices: Vec::new(),
            last_pattern: None,
            selected,
            last_filter_query: None,
            viewport_rows: None,
            omitted_workspace: None,
            renaming: None,
            filter_query: String::new(),
            scope: PickerScope::All,
            family_root: workspaces[active].family_root(),
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

    /// Matched offsets to highlight in filtered `row`'s haystack.
    ///
    /// See [`picker::row_indices`] for what `scratch` and `matching` are for.
    pub(crate) fn row_indices<'a>(
        &'a self,
        row: usize,
        scratch: &'a mut Vec<u32>,
        matching: &mut fuzzy::Scratch,
    ) -> &'a [u32] {
        let haystack = self
            .filtered
            .get(row)
            .and_then(|&idx| self.haystacks.get(idx))
            .map(String::as_str)
            .unwrap_or_default();
        picker::row_indices(
            &self.match_indices,
            self.last_pattern.as_ref(),
            row,
            haystack,
            scratch,
            matching,
        )
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Re-rank the entries for `query`, matches first by score descending then
    /// by status-grouped order. An empty or unusable query lists every entry in
    /// scope in the status-grouped order with no highlights.
    ///
    /// The haystack per row is `"{basename} {git_root}"`, so a query narrows on
    /// either the workspace name or any part of its root path. Match highlights
    /// land only on name-column indices when painted.
    ///
    /// Re-ranking for an unchanged query is skipped, so the idle tick this runs
    /// from costs nothing while the picker sits open. A rename skips it too,
    /// because the input then holds a new name, not a query.
    pub(crate) fn refilter(&mut self, query: &str) {
        if self.renaming.is_some() || self.last_filter_query.as_deref() == Some(query) {
            return;
        }
        self.rank(query);
    }

    /// Rank the entries in scope for `query`, whatever the cached query or a
    /// rename says, for a caller whose entry list or scope changed under the
    /// ranking.
    fn rank(&mut self, query: &str) {
        self.last_filter_query = Some(query.to_owned());

        let family = match self.scope {
            PickerScope::All => None,
            PickerScope::Family => Some(self.family_root),
        };
        let entries = &self.entries;
        let in_scope = |idx: &usize| family.is_none_or(|root| entries[*idx].family_root() == root);
        let items = self
            .haystacks
            .iter()
            .enumerate()
            .filter(|(idx, _)| in_scope(idx))
            .map(|(idx, haystack)| (idx, haystack.as_str()));

        self.last_pattern = picker::rank_into(
            query,
            items,
            self.entries.len(),
            &mut self.filtered,
            &mut self.match_indices,
        );
        // An unusable query lists every entry without walking `items`, so the
        // scope narrows that list here. Its rows hold no offsets, so the offset
        // list only shrinks to stay parallel.
        if self.last_pattern.is_none() {
            self.filtered.retain(in_scope);
            self.match_indices.truncate(self.filtered.len());
        }

        self.clamp_selected();
    }

    /// Re-rank the list against the highlighted row's own name, keeping that row
    /// selected, and return the name for the caller to write into the input.
    ///
    /// `None` for an empty list. The caller owns the input write, so this stays
    /// free of any [`Workspace`] borrow.
    ///
    /// [`Self::refilter`] ranks by score and then by entry order, not by name,
    /// so the completed row is not reliably first afterwards. It is tracked by
    /// its index into `entries` and the cursor moved to wherever that lands.
    pub(crate) fn complete_selected(&mut self) -> Option<String> {
        let entry_idx = *self.filtered.get(self.selected)?;
        let basename = self.entries.get(entry_idx)?.basename.clone();

        self.refilter(&basename);
        if let Some(row) = self.filtered.iter().position(|&i| i == entry_idx) {
            self.selected = row;
        }
        Some(basename)
    }

    /// Drop the entries at `indices` and re-rank what remains.
    ///
    /// A delete takes a session's row with the rows of its sub-sessions, so one
    /// call drops them all. Each index names a distinct entry, in any order.
    /// The selection keeps its row index, clamped into the shorter list, so it
    /// lands on the row that took a deleted row's place or on the new last row.
    ///
    /// [`Self::filtered`] holds indices into `entries`, so removing an entry
    /// invalidates every index past it. The removal runs from the highest index
    /// down, and the cached query then ranks the shrunk lists once, even during
    /// a rename.
    pub(crate) fn remove_entries(&mut self, indices: &[usize]) {
        let mut indices = indices.to_vec();
        indices.sort_unstable_by(|a, b| b.cmp(a));
        for entry_idx in indices {
            self.entries.remove(entry_idx);
            self.haystacks.remove(entry_idx);
            self.renaming = match self.renaming {
                Some(idx) if idx == entry_idx => None,
                Some(idx) if idx > entry_idx => Some(idx - 1),
                other => other,
            };
        }

        let query = self.last_filter_query.take().unwrap_or_default();
        self.rank(&query);
    }

    /// Drop the selected row's entry, as the delete of a session without
    /// sub-sessions does. A no-op on an empty list.
    #[cfg(test)]
    pub(crate) fn remove_selected(&mut self) {
        let Some(&entry_idx) = self.filtered.get(self.selected) else {
            return;
        };
        self.remove_entries(&[entry_idx]);
    }

    /// Drop the active workspace's row and select the first row that remains.
    ///
    /// A no-op when that row is the only one. A picker with no rows draws
    /// nothing while its input still takes the keys.
    pub(crate) fn omit_active(&mut self) {
        if self.entries.len() <= 1 {
            return;
        }
        let Some(idx) = self
            .entries
            .iter()
            .position(|e| e.status == WorkspaceStatus::Active)
        else {
            return;
        };

        self.selected = 0;
        self.omitted_workspace = self.entries[idx].id;
        self.remove_entries(&[idx]);
    }

    /// Whether the input holds a new name for a row instead of the filter
    /// query.
    pub(crate) fn renaming(&self) -> bool {
        self.renaming.is_some()
    }

    /// Start renaming the selected row, and return its name to seed the rename
    /// field.
    ///
    /// The filter query waits in [`Self::filter_query`], and the ranking holds
    /// until the rename ends. `None` on an empty list.
    pub(crate) fn rename_start(&mut self) -> Option<String> {
        let entry_idx = *self.filtered.get(self.selected)?;
        let name = self.entries.get(entry_idx)?.basename.clone();
        self.renaming = Some(entry_idx);
        self.filter_query = self.last_filter_query.clone().unwrap_or_default();
        Some(name)
    }

    /// Give the row under rename the name `name`, and end the rename.
    ///
    /// Returns the row's open workspace, its state file, and its old name, so
    /// the caller carries the name to what the row stands for. `None` while no
    /// rename runs.
    pub(crate) fn rename_finish(
        &mut self,
        name: &str,
    ) -> Option<(Option<WorkspaceId>, Option<PathBuf>, String)> {
        let idx = self.renaming.take()?;
        let entry = self.entries.get_mut(idx)?;
        let old = std::mem::replace(&mut entry.basename, name.to_owned());
        self.haystacks[idx] = haystack(entry);
        // The row now matches other queries, so the filter the caller restores
        // ranks afresh.
        self.last_filter_query = None;
        Some((entry.id, entry.state_path.clone(), old))
    }

    /// End the rename and leave the row's name as it was.
    pub(crate) fn rename_cancel(&mut self) {
        self.renaming = None;
    }

    /// Flip the list between every session and the active workspace's family,
    /// and select the first row past the active one, as the picker opens.
    ///
    /// A no-op during a rename, which holds the ranking.
    pub(crate) fn toggle_scope(&mut self) {
        if self.renaming.is_some() {
            return;
        }
        self.scope = match self.scope {
            PickerScope::All => PickerScope::Family,
            PickerScope::Family => PickerScope::All,
        };

        let query = self.last_filter_query.take().unwrap_or_default();
        self.rank(&query);
        self.selected = self
            .filtered
            .iter()
            .position(|&idx| self.entries[idx].status != WorkspaceStatus::Active)
            .unwrap_or(0);
    }

    /// The name of the family the list narrows to, or `None` while it lists
    /// every session.
    ///
    /// The name comes from the family root's row. A root without a row reads
    /// as `family`, because an untouched root leaves no saved session, and the
    /// picker omits the row of an untouched active workspace.
    pub(crate) fn family_title(&self) -> Option<&str> {
        match self.scope {
            PickerScope::All => None,
            PickerScope::Family => Some(
                self.entries
                    .iter()
                    .find(|entry| entry.uid == self.family_root)
                    .map_or("family", |entry| entry.basename.as_str()),
            ),
        }
    }

    /// The name column's text for the entry at `idx`, `parent/name` for a
    /// sub-session whose root has a row, and the row's own name otherwise.
    ///
    /// Recency order separates a family in the full list, so the row spells
    /// the tie itself. A root without a row leaves no prefix.
    pub(crate) fn display_label(&self, idx: usize) -> String {
        let entry = &self.entries[idx];
        let parent = entry
            .parent
            .and_then(|uid| self.entries.iter().find(|e| e.uid == uid));
        match parent {
            Some(parent) => format!("{}/{}", parent.basename, entry.basename),
            None => entry.basename.clone(),
        }
    }

    /// The entry indices of the sessions created inside the session at entry
    /// `idx`.
    ///
    /// A row never counts as its own sub-session, even when a uid collision
    /// makes its parent its own uid, so a delete never takes one row twice.
    pub(crate) fn sub_sessions_of(&self, idx: usize) -> Vec<usize> {
        let uid = self.entries[idx].uid;
        self.entries
            .iter()
            .enumerate()
            .filter(|&(other, entry)| other != idx && entry.parent == Some(uid))
            .map(|(other, _)| other)
            .collect()
    }

    /// Whether any row leads somewhere other than the active workspace.
    ///
    /// Without one, the bare launch keeps the finder closed.
    pub(crate) fn has_switch_target(&self) -> bool {
        self.entries
            .iter()
            .any(|e| e.status != WorkspaceStatus::Active)
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
        self.move_selection(1);
    }

    pub fn select_prev(&mut self) {
        self.move_selection(-1);
    }

    pub(crate) fn move_selection(&mut self, delta: i32) {
        picker::nav_move(self.filtered.len(), &mut self.selected, delta);
    }

    /// Page the selection by half the rendered list height in `dir` (negative
    /// up, positive down). Falls back to a single row before the first render
    /// sets [`Self::viewport_rows`].
    pub(crate) fn page(&mut self, dir: i32) {
        let step = dir * picker::nav_page_step(self.viewport_rows);
        picker::nav_move(self.filtered.len(), &mut self.selected, step);
    }

    fn clamp_selected(&mut self) {
        picker::nav_clamp(self.filtered.len(), &mut self.selected);
    }
}

/// The text a query matches `entry` against, which holds the row's name, its
/// remote host, and its root path.
fn haystack(entry: &PickerEntry) -> String {
    let host = entry.remote_host.as_deref().unwrap_or_default();
    format!("{} {} {}", entry.basename, host, entry.git_root.display())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::BufferId, editor_state::EditorId, input_view::SubmitTarget, workspace::Workspace,
    };
    use std::{
        sync::Arc,
        time::{Duration, UNIX_EPOCH},
    };
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

    /// The instant `secs` after the epoch, as a workspace's last focus or a
    /// state file's mtime. The test clock gives every workspace one instant,
    /// so a test that orders rows sets them apart.
    fn focused_at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn slotmap_with_two(exec: &Executor) -> (SlotMap<WorkspaceId, Workspace>, WorkspaceId) {
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let a = workspaces.insert(Workspace::new(
            PathBuf::from("/tmp/alpha"),
            exec,
            crate::test_notify(),
        ));
        workspaces[a].id = a;
        workspaces[a].last_active = focused_at(2);
        let b = workspaces.insert(Workspace::new(
            PathBuf::from("/tmp/beta"),
            exec,
            crate::test_notify(),
        ));
        workspaces[b].id = b;
        workspaces[b].last_active = focused_at(1);
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
        assert_eq!(
            picker.selected(),
            1,
            "the selection starts on the row after the active one"
        );
    }

    /// The active workspace is inserted last, and slot order puts the older
    /// session first. Only the focus instants put `previous` first.
    #[test]
    fn the_picker_opens_on_the_previous_session() {
        let exec = executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let mut insert = |root: &str, focused: u64| {
            let id = workspaces.insert(Workspace::new(
                PathBuf::from(root),
                &exec,
                crate::test_notify(),
            ));
            workspaces[id].id = id;
            workspaces[id].last_active = focused_at(focused);
            id
        };
        let older = insert("/tmp/older", 1);
        let previous = insert("/tmp/previous", 2);
        let active = insert("/tmp/active", 3);

        let mut picker = WorkspacePicker::new(&workspaces, active, Vec::new(), dummy_input());
        let opened_on = picker.selected_id();
        picker.select_next();
        assert_eq!(
            (opened_on, picker.selected_id()),
            (Some(previous), Some(older)),
            "Enter returns to the previous session, and Down reaches the one before it",
        );
    }

    /// The three names are chosen so completing the middle row drops the first
    /// and keeps the third. `zulu` cannot match `ab` while `abc` can, so the
    /// list reshuffles instead of narrowing to one row, and the cursor's old
    /// index stays in range while pointing at `abc`. Narrowing would let
    /// `clamp_selected` land on the right entry by itself and prove nothing.
    #[test]
    fn complete_selected_keeps_the_completed_row_selected() {
        let exec = executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let mut insert = |root: &str, name: &str, focused: u64| {
            let id = workspaces.insert(Workspace::new(
                PathBuf::from(root),
                &exec,
                crate::test_notify(),
            ));
            workspaces[id].id = id;
            workspaces[id].name = name.into();
            workspaces[id].last_active = focused_at(focused);
            id
        };
        let active = insert("/tmp/zulu", "zulu", 3);
        insert("/tmp/ab", "ab", 2);
        insert("/tmp/abc", "abc", 1);

        let mut picker = WorkspacePicker::new(&workspaces, active, Vec::new(), dummy_input());
        assert_eq!(
            picker
                .entries()
                .iter()
                .map(|e| e.basename.as_str())
                .collect::<Vec<_>>(),
            ["zulu", "ab", "abc"],
            "the active workspace leads, then the rest by recency"
        );

        assert_eq!(
            picker.selected_entry().map(|e| e.basename.as_str()),
            Some("ab"),
            "the second row is highlighted before completing"
        );

        assert_eq!(picker.complete_selected().as_deref(), Some("ab"));

        assert_eq!(
            picker.filtered().len(),
            2,
            "zulu drops out but abc still matches, so the list reshuffles"
        );
        assert_eq!(
            picker.selected_entry().map(|e| e.basename.as_str()),
            Some("ab"),
            "the completed workspace stays selected"
        );
    }

    /// The three names are chosen so the survivors both still match an empty
    /// query and stay in entry order, which is what proves the rebuilt
    /// `filtered` indexes the shrunk `entries` rather than the old list.
    #[test]
    fn remove_selected_drops_the_row_and_reranks() {
        let mut picker = picker_with_roots(&["/tmp/alpha", "/tmp/beta", "/tmp/gamma"]);

        assert_eq!(
            picker.selected_entry().map(|e| e.basename.as_str()),
            Some("beta"),
            "the middle row is highlighted before removing"
        );

        picker.remove_selected();

        assert_eq!(
            picker
                .entries()
                .iter()
                .map(|e| e.basename.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "gamma"],
            "the removed row is gone and the survivors keep their order"
        );
        assert_eq!(
            picker.filtered(),
            [0, 1],
            "the ranking is rebuilt against the shrunk entry list"
        );
        assert_eq!(
            picker.selected_entry().map(|e| e.basename.as_str()),
            Some("gamma"),
            "the selection lands on the row that took the removed one's place"
        );
    }

    #[test]
    fn remove_selected_on_the_last_row_clamps_the_selection() {
        let mut picker = picker_with_roots(&["/tmp/alpha", "/tmp/beta"]);
        picker.select_next();

        picker.remove_selected();

        assert_eq!(picker.entries().len(), 1, "only the first row survives");
        assert_eq!(
            picker.selected(),
            0,
            "the cursor moves up rather than pointing past the end"
        );
    }

    #[test]
    fn remove_selected_on_an_empty_list_does_nothing() {
        let mut picker = picker_with_roots(&["/tmp/alpha", "/tmp/beta"]);
        picker.refilter("zzzznomatch");
        assert!(picker.filtered().is_empty(), "the query matches nothing");

        picker.remove_selected();

        assert_eq!(picker.entries().len(), 2, "no entry is dropped");
    }

    /// The selection moves to `gamma` first, so the reset to the first survivor
    /// is observable.
    #[test]
    fn omit_active_drops_the_active_row_and_selects_the_first_survivor() {
        let mut picker = picker_with_roots(&["/tmp/alpha", "/tmp/beta", "/tmp/gamma"]);
        picker.select_next();

        picker.omit_active();

        assert_eq!(
            picker
                .entries()
                .iter()
                .map(|e| e.basename.as_str())
                .collect::<Vec<_>>(),
            ["beta", "gamma"],
            "the active row is gone and the rest keep their order"
        );
        assert_eq!(
            picker.filtered(),
            [0, 1],
            "the ranking is rebuilt against the shrunk entry list"
        );
        assert_eq!(
            picker.selected_entry().map(|e| e.basename.as_str()),
            Some("beta"),
            "the selection starts on the first row that remains"
        );
        assert!(
            picker.has_switch_target(),
            "every row that remains leads elsewhere"
        );
    }

    #[test]
    fn omit_active_keeps_a_lone_row() {
        let mut picker = picker_with_roots(&["/tmp/alpha"]);

        picker.omit_active();

        assert_eq!(
            picker
                .entries()
                .iter()
                .map(|e| e.basename.as_str())
                .collect::<Vec<_>>(),
            ["alpha"],
            "the only row stays"
        );
        assert!(
            !picker.has_switch_target(),
            "the lone row is the active workspace"
        );
    }

    #[test]
    fn rename_start_returns_the_selected_name_and_freezes_the_ranking() {
        let mut picker = picker_with_roots(&["/tmp/alpha", "/tmp/beta", "/tmp/gamma"]);
        let seeded = picker.rename_start();
        picker.refilter("gamma");
        assert_eq!(
            (seeded.as_deref(), picker.renaming(), picker.filtered()),
            (Some("beta"), true, [0, 1, 2].as_slice()),
            "the rename seeds the row's name, and the name typed over it ranks nothing"
        );
    }

    #[test]
    fn rename_finish_rewrites_the_row_and_its_haystack() {
        let mut picker = picker_with_roots(&["/tmp/alpha", "/tmp/beta"]);
        let beta = picker.selected_id();
        picker.rename_start();
        let renamed = picker.rename_finish("delta");
        picker.refilter("delta");
        assert_eq!(
            (
                renamed,
                picker.renaming(),
                picker
                    .entries()
                    .iter()
                    .map(|e| e.basename.as_str())
                    .collect::<Vec<_>>(),
                picker.filtered(),
            ),
            (
                Some((beta, None, "beta".to_string())),
                false,
                vec!["alpha", "delta"],
                [1].as_slice(),
            ),
            "the row takes the name, and a query for the name finds the row"
        );
    }

    /// The roots hold neither name, so only the name decides the match.
    #[test]
    fn a_renamed_row_drops_out_of_a_filter_it_no_longer_matches() {
        let exec = executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let mut insert = |root: &str, name: &str, focused: u64| {
            let id = workspaces.insert(Workspace::new(
                PathBuf::from(root),
                &exec,
                crate::test_notify(),
            ));
            workspaces[id].id = id;
            workspaces[id].name = name.into();
            workspaces[id].last_active = focused_at(focused);
            id
        };
        let active = insert("/tmp/w1", "alpha", 2);
        insert("/tmp/w2", "beta", 1);
        let mut picker = WorkspacePicker::new(&workspaces, active, Vec::new(), dummy_input());
        picker.refilter("beta");

        picker.rename_start();
        picker.rename_finish("zeta");
        let query = picker.filter_query.clone();
        picker.refilter(&query);
        assert!(
            picker.filtered().is_empty(),
            "the filter ranks again, and the renamed row matches it no more"
        );
    }

    /// The delete removes the row ahead of the one under rename, so the rename
    /// follows its row to the new index.
    #[test]
    fn a_delete_during_a_rename_keeps_the_rename_on_its_row() {
        let mut picker = picker_with_roots(&["/tmp/alpha", "/tmp/beta", "/tmp/gamma"]);
        picker.select_next();
        picker.rename_start();
        picker.select_prev();
        picker.remove_selected();

        picker.rename_finish("delta");
        assert_eq!(
            picker
                .entries()
                .iter()
                .map(|e| e.basename.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "delta"],
        );
    }

    #[test]
    fn rename_cancel_keeps_the_name_and_the_filter_query() {
        let mut picker = picker_with_roots(&["/tmp/alpha", "/tmp/beta", "/tmp/gamma"]);
        picker.refilter("a");
        picker.rename_start();
        picker.rename_cancel();
        assert_eq!(
            (
                picker.renaming(),
                picker.filter_query.as_str(),
                picker
                    .entries()
                    .iter()
                    .map(|e| e.basename.as_str())
                    .collect::<Vec<_>>(),
            ),
            (false, "a", vec!["alpha", "beta", "gamma"]),
            "every name stays, and the filter query waits to go back into the input"
        );
    }

    #[test]
    fn complete_selected_on_an_empty_list_is_none() {
        let exec = executor();
        let (workspaces, active) = slotmap_with_two(&exec);
        let mut picker = WorkspacePicker::new(&workspaces, active, Vec::new(), dummy_input());
        picker.refilter("zzzznomatch");
        assert!(picker.filtered().is_empty(), "the query matches nothing");

        assert_eq!(picker.complete_selected(), None);
    }

    /// The active row leads although it was focused before everything else,
    /// and the open `beta` falls between the two saved sessions by time.
    #[test]
    fn rows_after_the_active_one_order_by_recency_across_open_and_saved() {
        use crate::workspace::registry::{RegistryEntry, WorkspaceMeta};

        let exec = executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let mut insert = |root: &str, name: &str, focused: u64| {
            let id = workspaces.insert(Workspace::new(
                PathBuf::from(root),
                &exec,
                crate::test_notify(),
            ));
            workspaces[id].id = id;
            workspaces[id].name = name.into();
            workspaces[id].last_active = focused_at(focused);
            id
        };
        let a = insert("/tmp/alpha", "alpha", 10);
        let b = insert("/tmp/beta", "beta", 70);
        let open_uid = workspaces[a].uid;

        let entry = |uid: u64, name: &str, root: &str, secs: u64| RegistryEntry {
            meta: WorkspaceMeta {
                uid: WorkspaceUid(uid),
                name: name.into(),
                git_root: PathBuf::from(root),
                buffer_count: 3,
                remote_host: None,
                parent: None,
            },
            state_path: PathBuf::from(root).join("s.ron"),
            mtime: focused_at(secs),
        };

        let inactive = vec![
            entry(open_uid.0, "shadow", "/tmp/alpha", 100),
            entry(9, "old-proj", "/tmp/old", 50),
            entry(8, "new-proj", "/tmp/new", 90),
        ];

        let picker = WorkspacePicker::new(&workspaces, a, inactive, dummy_input());
        assert_eq!(
            picker
                .entries()
                .iter()
                .map(|e| (e.basename.as_str(), e.id, e.status, e.buffer_count))
                .collect::<Vec<_>>(),
            [
                ("alpha", Some(a), WorkspaceStatus::Active, 1),
                ("new-proj", None, WorkspaceStatus::Inactive, 3),
                ("beta", Some(b), WorkspaceStatus::Background, 1),
                ("old-proj", None, WorkspaceStatus::Inactive, 3),
            ],
            "the open workspace's shadow sidecar dedupes, and the rest interleave newest first"
        );
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

        assert_eq!(picker.selected_id(), picker.entries()[1].id);
        picker.select_prev();
        assert_eq!(picker.selected_id(), Some(active));
    }

    #[test]
    fn single_workspace_picker_lists_only_current() {
        let exec = executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let a = workspaces.insert(Workspace::new(
            PathBuf::from("/tmp/alpha"),
            &exec,
            crate::test_notify(),
        ));
        workspaces[a].id = a;

        let picker = WorkspacePicker::new(&workspaces, a, Vec::new(), dummy_input());
        assert_eq!(picker.entries().len(), 1);
        assert_eq!(picker.entries()[0].status, WorkspaceStatus::Active);
    }

    fn picker_with_roots(roots: &[&str]) -> WorkspacePicker {
        let exec = executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let mut first = None;
        for (rank, root) in roots.iter().enumerate() {
            let id = workspaces.insert(Workspace::new(
                PathBuf::from(*root),
                &exec,
                crate::test_notify(),
            ));
            workspaces[id].id = id;
            // Cleared so the row's name falls back to the path basename. The
            // default is derived from the uid, which differs run to run.
            workspaces[id].name = String::new();
            // Each root focused before the one listed ahead of it, so the rows keep
            // the given order.
            workspaces[id].last_active = focused_at((roots.len() - rank) as u64);
            first.get_or_insert(id);
        }
        let active = first.expect("at least one workspace");
        WorkspacePicker::new(&workspaces, active, Vec::new(), dummy_input())
    }

    #[test]
    fn refilter_narrows_reranks_and_clamps_selection() {
        let exec = executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let mut active = None;
        for root in ["/tmp/alpha", "/tmp/beta", "/tmp/gamma"] {
            let id = workspaces.insert(Workspace::new(
                PathBuf::from(root),
                &exec,
                crate::test_notify(),
            ));
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

    #[test]
    fn an_unchanged_query_reuses_the_ranking() {
        let mut picker = picker_with_roots(&["/tmp/alpha", "/tmp/beta"]);
        picker.filtered.clear();

        picker.refilter("");
        assert!(
            picker.filtered.is_empty(),
            "the same query ranks nothing again"
        );

        picker.refilter("alpha");
        assert_eq!(picker.filtered.len(), 1, "a changed query ranks afresh");
    }

    #[test]
    fn a_query_narrows_on_the_root_path_as_well_as_the_name() {
        let mut picker = picker_with_roots(&["/tmp/alpha", "/elsewhere/beta"]);
        picker.refilter("elsewhere");
        assert_eq!(
            picker.selected_entry().map(|e| e.basename.as_str()),
            Some("beta"),
            "the stored haystack carries the root path, not just the name"
        );
        assert_eq!(picker.filtered.len(), 1, "the other root does not match");
    }

    #[test]
    fn a_query_narrows_on_a_remote_host() {
        let exec = executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let mut ids = Vec::new();
        for root in ["/tmp/alpha", "/tmp/beta"] {
            let id = workspaces.insert(Workspace::new(
                PathBuf::from(root),
                &exec,
                crate::test_notify(),
            ));
            workspaces[id].id = id;
            workspaces[id].name = String::new();
            ids.push(id);
        }
        workspaces[ids[1]].remote = Some(crate::ssh::RemoteTarget {
            transport: crate::ssh::Transport::Ssh,
            host: "faraway".to_owned(),
            args: Vec::new(),
        });

        let mut picker = WorkspacePicker::new(&workspaces, ids[0], Vec::new(), dummy_input());
        picker.refilter("faraway");

        assert_eq!(
            picker.selected_entry().map(|e| e.basename.as_str()),
            Some("beta"),
            "the haystack carries the host, so a host names its workspace"
        );
        assert_eq!(picker.filtered.len(), 1, "the local row does not match");
    }

    /// A picker over `root`, its active sub-session `sub`, and an unrelated
    /// `other`, which the rows list as `sub`, `other`, `root`. The roots match
    /// no query letter, so a query narrows on names alone.
    fn family_picker() -> WorkspacePicker {
        let exec = executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let mut insert = |name: &str, uid: u64, parent: Option<u64>, focused: u64| {
            let id = workspaces.insert(Workspace::new(
                PathBuf::from("/w"),
                &exec,
                crate::test_notify(),
            ));
            let ws = &mut workspaces[id];
            ws.id = id;
            ws.name = name.to_string();
            ws.uid = WorkspaceUid(uid);
            ws.parent = parent.map(WorkspaceUid);
            ws.last_active = focused_at(focused);
            id
        };
        insert("root", 1, None, 1);
        let sub = insert("sub", 2, Some(1), 3);
        insert("other", 3, None, 2);
        WorkspacePicker::new(&workspaces, sub, Vec::new(), dummy_input())
    }

    /// The listed rows by name, the selected row's name, and the family title.
    fn listing(picker: &WorkspacePicker) -> (Vec<&str>, Option<&str>, Option<&str>) {
        let rows = picker
            .filtered
            .iter()
            .map(|&idx| picker.entries[idx].basename.as_str())
            .collect();
        let selected = picker.selected_entry().map(|e| e.basename.as_str());
        (rows, selected, picker.family_title())
    }

    /// The test moves the selection to the active row first, so a toggle that
    /// leaves the selection there fails the second step.
    #[test]
    fn toggle_scope_lists_only_the_active_family_and_back() {
        let mut picker = family_picker();
        picker.select_prev();
        assert_eq!(
            listing(&picker),
            (vec!["sub", "other", "root"], Some("sub"), None),
            "the picker opens on every session"
        );

        picker.toggle_scope();
        assert_eq!(
            listing(&picker),
            (vec!["sub", "root"], Some("root"), Some("root")),
            "the family of a sub-session is its root's"
        );

        picker.refilter("he");
        assert_eq!(
            listing(&picker),
            (vec![], None, Some("root")),
            "a query matching only `other` lists nothing in the family"
        );

        picker.toggle_scope();
        assert_eq!(
            listing(&picker),
            (vec!["other"], Some("other"), None),
            "the toggle back keeps the query"
        );
    }

    #[test]
    fn a_family_without_its_root_row_reads_as_family() {
        let mut picker = family_picker();
        picker.toggle_scope();
        picker.remove_selected();

        assert_eq!(listing(&picker), (vec!["sub"], Some("sub"), Some("family")));
    }

    #[test]
    fn a_rename_holds_the_scope() {
        let mut picker = family_picker();
        picker.rename_start();
        picker.toggle_scope();

        assert_eq!(
            listing(&picker),
            (vec!["sub", "other", "root"], Some("other"), None)
        );
    }

    /// Every entry's name-column label, in entry order.
    fn labels(picker: &WorkspacePicker) -> Vec<String> {
        (0..picker.entries.len())
            .map(|idx| picker.display_label(idx))
            .collect()
    }

    #[test]
    fn display_label_prefixes_a_sub_session_with_its_parent() {
        assert_eq!(labels(&family_picker()), ["root/sub", "other", "root"]);
    }

    /// The selection starts on `other`, and one step down reaches `root`.
    #[test]
    fn display_label_drops_a_missing_parent() {
        let mut picker = family_picker();
        picker.select_next();
        picker.remove_selected();

        assert_eq!(labels(&picker), ["sub", "other"]);
    }

    /// The indices come in ascending order, which a removal from the lowest
    /// index up gets wrong.
    #[test]
    fn remove_entries_drops_every_index_and_reranks() {
        let mut picker = family_picker();
        picker.remove_entries(&[0, 2]);

        assert_eq!(listing(&picker), (vec!["other"], Some("other"), None));
    }
}
