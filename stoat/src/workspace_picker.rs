use crate::workspace::{Workspace, WorkspaceId, WorkspaceUid};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use slotmap::SlotMap;
use std::path::PathBuf;

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

impl WorkspacePicker {
    pub fn new(workspaces: &SlotMap<WorkspaceId, Workspace>, active: WorkspaceId) -> Self {
        let mut entries: Vec<PickerEntry> = workspaces
            .iter()
            .map(|(id, ws)| PickerEntry {
                id,
                basename: ws
                    .git_root
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("(unnamed)")
                    .to_string(),
                git_root: ws.git_root.clone(),
                uid: ws.uid,
                is_current: id == active,
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
    use crate::workspace::Workspace;
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
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

        picker.handle_key(key(KeyCode::Down));
        assert_eq!(picker.selected(), 1);
        picker.handle_key(key(KeyCode::Down));
        assert_eq!(picker.selected(), 1);
        picker.handle_key(key(KeyCode::Up));
        assert_eq!(picker.selected(), 0);
        picker.handle_key(key(KeyCode::Up));
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn ctrl_n_and_ctrl_p_move_selection() {
        let exec = executor();
        let (workspaces, active) = slotmap_with_two(&exec);
        let mut picker = WorkspacePicker::new(&workspaces, active);

        picker.handle_key(ctrl('n'));
        assert_eq!(picker.selected(), 1);
        picker.handle_key(ctrl('p'));
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn enter_selects_focused_entry() {
        let exec = executor();
        let (workspaces, active) = slotmap_with_two(&exec);
        let mut picker = WorkspacePicker::new(&workspaces, active);

        picker.handle_key(key(KeyCode::Down));
        let sibling_id = picker.entries()[1].id;
        match picker.handle_key(key(KeyCode::Enter)) {
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
            picker.handle_key(key(KeyCode::Esc)),
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
}
