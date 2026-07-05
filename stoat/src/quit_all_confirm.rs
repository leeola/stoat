use crate::buffer_registry::DirtyBuffer;
use std::path::Path;

/// Modal that lists every dirty buffer when [`stoat_action::QuitAll`]
/// fires with unsaved work pending. Confirming the prompt discards the
/// listed buffers and quits. Cancelling dismisses it without quitting.
/// The `modal == quit_confirm` keymap block binds those keys to
/// [`stoat_action::QuitAllConfirm`] and [`stoat_action::QuitAllCancel`].
///
/// Constructed fresh on every QuitAll dispatch. Pre-rendered display
/// strings are captured at construction so the renderer doesn't need to
/// borrow back into [`crate::workspace::Workspace`] state.
pub struct QuitAllConfirm {
    entries: Vec<DirtyEntry>,
}

/// One dirty-buffer row in the modal.
pub struct DirtyEntry {
    pub display: String,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::BufferId;
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
