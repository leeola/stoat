use crate::{
    paths,
    workspace::{Workspace, WorkspaceId, WorkspaceUid},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use slotmap::SlotMap;
use std::path::{Path, PathBuf};

/// Modal list of open workspaces. Rendered as a centered overlay when
/// [`SwitchWorkspace`] fires; navigates between in-memory workspaces and
/// emits [`PickerOutcome::Select`] once the user confirms a row.
pub struct WorkspacePicker {
    entries: Vec<PickerEntry>,
    selected: usize,
}

/// One row in the picker. Built up-front from the workspace slotmap so the
/// picker owns its own display data and doesn't borrow from [`Stoat`] for
/// its lifetime.
pub struct PickerEntry {
    pub id: WorkspaceId,
    pub basename: String,
    pub git_root: PathBuf,
    pub uid: WorkspaceUid,
    pub is_current: bool,
    pub buffer_count: usize,
    pub run_count: usize,
    pub editor_count: usize,
}

pub enum PickerOutcome {
    /// Re-render but keep the picker open.
    None,
    /// User cancelled; caller should drop the picker.
    Close,
    /// User selected a row; caller should switch to this workspace and drop
    /// the picker.
    Select(WorkspaceId),
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
    pub fn new(workspaces: &SlotMap<WorkspaceId, Workspace>, active: WorkspaceId) -> Self {
        let mut entries: Vec<PickerEntry> = workspaces
            .iter()
            .map(|(id, ws)| PickerEntry {
                id,
                basename: if !ws.name.is_empty() {
                    ws.name.clone()
                } else {
                    ws.git_root
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("(unnamed)")
                        .to_string()
                },
                git_root: ws.git_root.clone(),
                uid: ws.uid,
                is_current: id == active,
                buffer_count: ws.buffers.len(),
                run_count: ws.runs.len(),
                editor_count: ws.editors.len(),
            })
            .collect();
        // Current workspace first, then alphabetical by basename so the list
        // reads predictably in the modal.
        entries.sort_by(|a, b| {
            b.is_current
                .cmp(&a.is_current)
                .then_with(|| a.basename.cmp(&b.basename))
                .then_with(|| a.uid.0.cmp(&b.uid.0))
        });

        let selected = entries.iter().position(|e| e.is_current).unwrap_or(0);
        Self { entries, selected }
    }

    pub fn entries(&self) -> &[PickerEntry] {
        &self.entries
    }

    pub fn selected(&self) -> usize {
        self.selected
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

    pub fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        match key.code {
            KeyCode::Esc => PickerOutcome::Close,
            KeyCode::Enter => match self.entries.get(self.selected) {
                Some(entry) => PickerOutcome::Select(entry.id),
                None => PickerOutcome::Close,
            },
            KeyCode::Up => {
                move_selection(self.entries.len(), &mut self.selected, -1);
                PickerOutcome::None
            },
            KeyCode::Down => {
                move_selection(self.entries.len(), &mut self.selected, 1);
                PickerOutcome::None
            },
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                move_selection(self.entries.len(), &mut self.selected, -1);
                PickerOutcome::None
            },
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                move_selection(self.entries.len(), &mut self.selected, 1);
                PickerOutcome::None
            },
            _ => PickerOutcome::None,
        }
    }
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
    use crate::{test_harness::keys, workspace::Workspace};
    use std::sync::Arc;
    use stoat_scheduler::{Executor, TestScheduler};

    fn executor() -> Executor {
        Arc::new(TestScheduler::new()).executor()
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
        let picker = WorkspacePicker::new(&workspaces, active);

        let entries = picker.entries();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_current);
        assert_eq!(entries[0].id, active);
        assert!(!entries[1].is_current);
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn down_and_up_clamp_at_ends() {
        let exec = executor();
        let (workspaces, active) = slotmap_with_two(&exec);
        let mut picker = WorkspacePicker::new(&workspaces, active);

        picker.handle_key(keys::key(KeyCode::Down));
        assert_eq!(picker.selected(), 1);
        picker.handle_key(keys::key(KeyCode::Down));
        assert_eq!(picker.selected(), 1);
        picker.handle_key(keys::key(KeyCode::Up));
        assert_eq!(picker.selected(), 0);
        picker.handle_key(keys::key(KeyCode::Up));
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn ctrl_n_and_ctrl_p_move_selection() {
        let exec = executor();
        let (workspaces, active) = slotmap_with_two(&exec);
        let mut picker = WorkspacePicker::new(&workspaces, active);

        picker.handle_key(keys::ctrl('n'));
        assert_eq!(picker.selected(), 1);
        picker.handle_key(keys::ctrl('p'));
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn enter_selects_focused_entry() {
        let exec = executor();
        let (workspaces, active) = slotmap_with_two(&exec);
        let mut picker = WorkspacePicker::new(&workspaces, active);

        picker.handle_key(keys::key(KeyCode::Down));
        let sibling_id = picker.entries()[1].id;
        match picker.handle_key(keys::key(KeyCode::Enter)) {
            PickerOutcome::Select(id) => assert_eq!(id, sibling_id),
            _ => panic!("expected Select outcome"),
        }
    }

    #[test]
    fn escape_closes_picker() {
        let exec = executor();
        let (workspaces, active) = slotmap_with_two(&exec);
        let mut picker = WorkspacePicker::new(&workspaces, active);

        assert!(matches!(
            picker.handle_key(keys::key(KeyCode::Esc)),
            PickerOutcome::Close
        ));
    }

    #[test]
    fn single_workspace_picker_lists_only_current() {
        let exec = executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let a = workspaces.insert(Workspace::new(PathBuf::from("/tmp/alpha"), &exec));
        workspaces[a].id = a;

        let picker = WorkspacePicker::new(&workspaces, a);
        assert_eq!(picker.entries().len(), 1);
        assert!(picker.entries()[0].is_current);
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
        WorkspacePicker::new(&workspaces, active)
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
}
