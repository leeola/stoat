//! Stage selected lines from line selection mode.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Stage only the selected lines from the current hunk.
    ///
    /// Generates a partial patch from the [`LineSelection`], applies it via
    /// libgit2, then clears line selection and returns to diff review.
    pub fn diff_review_line_select_stage(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = self.apply_line_selection(false, cx) {
            tracing::error!("DiffReviewLineSelectStage failed: {e}");
        }
    }

    pub(crate) fn apply_line_selection(
        &mut self,
        reverse: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let selection = self
            .line_selection
            .as_ref()
            .ok_or_else(|| "No line selection active".to_string())?;

        if !selection.has_selection() {
            return Err("No lines selected".to_string());
        }

        let file_path = self
            .current_file_path
            .as_ref()
            .ok_or_else(|| "No file path set".to_string())?
            .clone();

        let repo_dir = self.worktree_root_abs();

        let patch = super::super::hunk_patch::generate_partial_hunk_patch(selection, &file_path)?;

        let (location, actual_reverse) = if self.diff_review_comparison_mode
            == crate::git::diff_review::DiffComparisonMode::HeadVsParent
        {
            (git2::ApplyLocation::WorkDir, true)
        } else {
            (git2::ApplyLocation::Index, reverse)
        };

        super::super::hunk_patch::apply_patch(&patch, &repo_dir, actual_reverse, location)?;

        let action = if reverse { "Unstaged" } else { "Staged" };
        tracing::info!(
            "{action} {} selected lines in {:?}",
            selection.selected_count(),
            file_path
        );

        self.line_selection = None;
        self.set_mode_by_name("diff_review", cx);
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();

        Ok(())
    }
}
