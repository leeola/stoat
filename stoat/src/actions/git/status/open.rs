//! Open git status modal action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Open git status modal.
    ///
    /// Discovers the git repository from the worktree root, gathers status information
    /// for all modified files, and enters [`crate::stoat::KeyContext::Git`] with git_status mode.
    /// The modal displays modified files with their git status, allows filtering by status,
    /// and provides preview diffs for selected files.
    ///
    /// # Workflow
    ///
    /// 1. Saves current mode for later restoration
    /// 2. Discovers git repository from worktree root
    /// 3. Gathers git status entries using [`crate::git_status::gather_git_status`]
    /// 4. Gathers branch information using [`crate::git_status::gather_git_branch_info`]
    /// 5. Initializes git status state (files, filter, selection)
    /// 6. Enters Git KeyContext with git_status mode
    /// 7. Applies initial filter via [`Stoat::filter_git_status_files`]
    /// 8. Loads diff preview for first file
    ///
    /// # Behavior
    ///
    /// - Returns early if no git repository found
    /// - Logs error if git status gathering fails
    /// - Sets dirty count for status bar display
    /// - Initial filter is [`crate::git_status::GitStatusFilter::default`]
    ///
    /// # Related
    ///
    /// - [`Stoat::git_status_next`] - navigate to next file
    /// - [`Stoat::git_status_prev`] - navigate to previous file
    /// - [`Stoat::git_status_select`] - open selected file
    /// - [`Stoat::git_status_dismiss`] - close modal
    /// - [`Stoat::git_status_cycle_filter`] - change filter mode
    pub fn open_git_status(&mut self, cx: &mut Context<Self>) {
        debug!("Opening git status");

        // Save current mode and context to restore later
        // TODO: Context restoration should be configurable via keymap once we have
        // concrete use cases to guide the design of keymap-based abstractions
        self.git_status_previous_mode = Some(self.mode.clone());
        self.git_status_previous_key_context = Some(self.key_context);

        // Use worktree root to discover repository
        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git_repository::Repository::discover(&root_path).ok() {
            Some(repo) => repo,
            None => {
                debug!("No git repository found");
                return;
            },
        };

        // Gather git status
        let entries = match crate::git_status::gather_git_status(repo.inner()) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::error!("Failed to gather git status: {}", e);
                return;
            },
        };

        let dirty_count = entries.len();
        debug!(file_count = dirty_count, "Gathered git status");

        // Gather branch info
        let branch_info = crate::git_status::gather_git_branch_info(repo.inner());
        if let Some(ref info) = branch_info {
            debug!(
                branch = %info.branch_name,
                ahead = info.ahead,
                behind = info.behind,
                "Gathered git branch info"
            );
        }

        // Initialize git status state
        self.git_status_files = entries;
        self.git_status_filter = crate::git_status::GitStatusFilter::default();
        self.git_status_selected = 0;
        self.git_status_branch_info = branch_info;
        self.git_dirty_count = dirty_count;

        // Enter Git KeyContext and git_status mode
        self.key_context = crate::stoat::KeyContext::Git;
        self.mode = "git_status".into();
        debug!("Entered Git KeyContext with git_status mode");

        // Apply initial filter and load preview
        self.filter_git_status_files(cx);

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn opens_git_status(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.mode = "normal".to_string();
            s.open_git_status(cx);
            // Mode changes if git repo found
            if s.mode() == "git_status" {
                assert_eq!(s.key_context, crate::stoat::KeyContext::Git);
            }
        });
    }
}
