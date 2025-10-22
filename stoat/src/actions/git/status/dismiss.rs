//! Git status dismiss action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Dismiss git status modal.
    ///
    /// Clears all git status state including files list, selection index, branch info,
    /// and previous mode reference. Mode and KeyContext transitions are now handled
    /// by the [`crate::actions::SetKeyContext`] action bound to Escape.
    ///
    /// # Workflow
    ///
    /// 1. Clears git_status_files list
    /// 2. Resets git_status_selected to 0
    /// 3. Clears git_status_branch_info
    /// 4. Clears git_status_previous_mode
    /// 5. Emits Changed event and notifies
    ///
    /// # Behavior
    ///
    /// - Only operates in git_status mode
    /// - Does NOT change mode or KeyContext (handled by SetKeyContext)
    /// - Clears all state to prepare for next open
    ///
    /// # Related
    ///
    /// - [`Stoat::open_git_status`] - opens the modal
    /// - [`crate::actions::SetKeyContext`] - handles mode/context transitions
    pub fn git_status_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_status" {
            return;
        }

        debug!("Dismissing git status");

        // Clear git status state
        self.git_status_files.clear();
        self.git_status_selected = 0;
        self.git_status_branch_info = None;
        self.git_status_previous_mode = None;

        // Restore previous KeyContext (this auto-applies the default mode for that context)
        if let Some(previous_context) = self.git_status_previous_key_context.take() {
            self.handle_set_key_context(previous_context, cx);
        } else {
            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn dismisses_git_status(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_git_status(cx);
            if s.mode() == "git_status" {
                s.git_status_dismiss(cx);
                // State cleared
                assert_eq!(s.git_status_files.len(), 0);
                assert!(s.git_status_previous_mode.is_none());
            }
        });
    }
}
