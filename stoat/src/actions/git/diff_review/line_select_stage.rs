//! Stage selected lines from line selection mode.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Stage only the selected lines from the current hunk.
    ///
    /// Generates a partial patch from the [`LineSelection`], applies it via
    /// libgit2, then clears line selection and returns to diff review.
    pub fn diff_review_line_select_stage(&mut self, cx: &mut Context<Self>) {
        self.apply_line_selection(false, cx);
    }

    pub(crate) fn apply_line_selection(&mut self, reverse: bool, cx: &mut Context<Self>) {
        let selection = match self.line_selection.as_ref() {
            Some(s) => s,
            None => {
                tracing::error!("apply_line_selection: No line selection active");
                return;
            },
        };

        if !selection.has_selection() {
            tracing::error!("apply_line_selection: No lines selected");
            return;
        }

        let file_path = match self.current_file_path.as_ref() {
            Some(p) => p.clone(),
            None => {
                tracing::error!("apply_line_selection: No file path set");
                return;
            },
        };

        let patch =
            match super::super::hunk_patch::generate_partial_hunk_patch(selection, &file_path) {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!("apply_line_selection: {e}");
                    return;
                },
            };

        let selected_count = selection.selected_count();

        let repo_dir = self.worktree_root_abs();
        let services = self.services.clone();
        let is_commit = self.review_state.source.is_commit();

        let (location, actual_reverse) = if is_commit {
            (crate::git::provider::ApplyLocation::WorkDir, true)
        } else {
            (crate::git::provider::ApplyLocation::Index, reverse)
        };

        self.line_selection = None;
        self.set_mode_by_name("diff_review", cx);
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = async {
                let repo = services
                    .git
                    .open(&repo_dir)
                    .await
                    .map_err(|e| format!("Failed to open repository: {e}"))?;
                super::super::hunk_patch::apply_patch(&patch, &*repo, actual_reverse, location)
                    .await?;

                let action = if reverse { "Unstaged" } else { "Staged" };
                tracing::info!(
                    "{action} {selected_count} selected lines in {:?}",
                    file_path
                );
                Ok::<(), String>(())
            }
            .await;
            let _ = this.update(cx, |stoat, cx| {
                if let Err(e) = result {
                    tracing::error!("apply_line_selection: {e}");
                    return;
                }
                stoat.refresh_git_diff(cx);
            });
        })
        .detach();
    }
}
