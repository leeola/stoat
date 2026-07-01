use crate::buffer_registry::DirtyBuffer;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::Path;

/// Modal that lists every dirty buffer when [`stoat_action::QuitAll`]
/// fires with unsaved work pending. The user confirms with `y` / Enter
/// to discard and quit, or cancels with `n` / Esc / Ctrl-c. Constructed
/// fresh on every QuitAll dispatch; pre-rendered display strings are
/// captured at construction so the renderer doesn't need to borrow back
/// into [`crate::workspace::Workspace`] state.
pub struct QuitAllConfirm {
    entries: Vec<DirtyEntry>,
}

/// One dirty-buffer row in the modal.
pub struct DirtyEntry {
    pub display: String,
}

pub enum ConfirmOutcome {
    /// Re-render but keep the modal open.
    None,
    /// User cancelled; caller should drop the modal.
    Cancel,
    /// User confirmed; caller should treat this as a quit signal.
    Confirm,
}

impl QuitAllConfirm {
    /// Build a modal listing every dirty buffer in `entries`. `git_root`
    /// is used to format file paths relative to the workspace root;
    /// scratch buffers (no path) render as `<scratch>`.
    pub fn new(entries: &[DirtyBuffer], git_root: &Path) -> Self {
        let entries = entries
            .iter()
            .map(|d| DirtyEntry {
                display: match &d.path {
                    Some(p) => crate::paths::display_relative(p, git_root),
                    None => "<scratch>".to_string(),
                },
            })
            .collect();
        Self { entries }
    }

    pub fn entries(&self) -> &[DirtyEntry] {
        &self.entries
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ConfirmOutcome {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => ConfirmOutcome::Confirm,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => ConfirmOutcome::Cancel,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ConfirmOutcome::Cancel
            },
            _ => ConfirmOutcome::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{buffer::BufferId, test_harness::keys};
    use std::path::PathBuf;

    fn dirty(id: u64, path: Option<&str>) -> DirtyBuffer {
        DirtyBuffer {
            id: BufferId::new(id),
            path: path.map(PathBuf::from),
        }
    }

    #[test]
    fn new_formats_paths_relative_to_git_root() {
        let modal = QuitAllConfirm::new(
            &[dirty(1, Some("/r/a.rs")), dirty(2, Some("/r/sub/b.rs"))],
            Path::new("/r"),
        );
        let displays: Vec<&str> = modal.entries().iter().map(|e| e.display.as_str()).collect();
        assert_eq!(displays, vec!["a.rs", "sub/b.rs"]);
    }

    #[test]
    fn new_renders_scratch_buffer_as_placeholder() {
        let modal = QuitAllConfirm::new(&[dirty(1, None)], Path::new("/r"));
        assert_eq!(
            modal
                .entries()
                .iter()
                .map(|e| e.display.as_str())
                .collect::<Vec<_>>(),
            vec!["<scratch>"]
        );
    }

    #[test]
    fn y_confirms() {
        let mut modal = QuitAllConfirm::new(&[dirty(1, Some("/r/a.rs"))], Path::new("/r"));
        assert!(matches!(
            modal.handle_key(keys::key(KeyCode::Char('y'))),
            ConfirmOutcome::Confirm
        ));
    }

    #[test]
    fn enter_confirms() {
        let mut modal = QuitAllConfirm::new(&[dirty(1, Some("/r/a.rs"))], Path::new("/r"));
        assert!(matches!(
            modal.handle_key(keys::key(KeyCode::Enter)),
            ConfirmOutcome::Confirm
        ));
    }

    #[test]
    fn n_cancels() {
        let mut modal = QuitAllConfirm::new(&[dirty(1, Some("/r/a.rs"))], Path::new("/r"));
        assert!(matches!(
            modal.handle_key(keys::key(KeyCode::Char('n'))),
            ConfirmOutcome::Cancel
        ));
    }

    #[test]
    fn esc_cancels() {
        let mut modal = QuitAllConfirm::new(&[dirty(1, Some("/r/a.rs"))], Path::new("/r"));
        assert!(matches!(
            modal.handle_key(keys::key(KeyCode::Esc)),
            ConfirmOutcome::Cancel
        ));
    }

    #[test]
    fn ctrl_c_cancels() {
        let mut modal = QuitAllConfirm::new(&[dirty(1, Some("/r/a.rs"))], Path::new("/r"));
        let event = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(modal.handle_key(event), ConfirmOutcome::Cancel));
    }

    #[test]
    fn other_keys_keep_modal_open() {
        let mut modal = QuitAllConfirm::new(&[dirty(1, Some("/r/a.rs"))], Path::new("/r"));
        assert!(matches!(
            modal.handle_key(keys::key(KeyCode::Char('x'))),
            ConfirmOutcome::None
        ));
    }

    #[test]
    fn snapshot_quit_all_confirm_modal() {
        let mut h = crate::Stoat::test();
        h.seed_focused_buffer("unsaved");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::QuitAll);
        h.assert_snapshot("quit_all_confirm_modal");
    }

    #[test]
    fn snapshot_quit_all_confirm_over_full_editor() {
        let mut h = crate::Stoat::test();
        // Fill the editor so the modal overlaps buffer text. Without a cleared
        // background the interior gaps would show these X's bleeding through.
        let line = "X".repeat(78);
        let filled: String = (0..30).map(|_| format!("{line}\n")).collect();
        h.seed_focused_buffer(&filled);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::QuitAll);
        h.assert_snapshot("quit_all_confirm_over_full_editor");
    }
}
