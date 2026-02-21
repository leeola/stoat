use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Revert the current hunk in LastCommit source by applying the reverse patch
    /// to the working tree.
    pub fn diff_review_revert_hunk(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        if !self.review_state.source.is_commit() {
            return Err("Revert hunk is only available in LastCommit source".to_string());
        }

        let file_path = self
            .current_file_path
            .as_ref()
            .ok_or_else(|| "No file path set".to_string())?
            .clone();

        let repo_dir = self.worktree_root_abs();

        let cursor_row = self.cursor.position().row;
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

        let diff = buffer_item
            .read(cx)
            .diff()
            .ok_or_else(|| "No diff information available".to_string())?;

        let hunk_index = diff
            .hunk_for_row(cursor_row, &buffer_snapshot)
            .ok_or_else(|| format!("No hunk at cursor row {cursor_row}"))?;

        let hunk = &diff.hunks[hunk_index];

        let patch = super::super::hunk_patch::generate_hunk_patch(
            diff,
            hunk,
            &buffer_snapshot,
            &file_path,
        )?;

        super::super::hunk_patch::apply_patch(
            &patch,
            &repo_dir,
            true,
            git2::ApplyLocation::WorkDir,
        )?;

        tracing::info!("Reverted hunk at row {cursor_row} in {:?}", file_path);

        // Recompute diff to update colors (reverted hunk transitions from committed to unstaged)
        if let Some((new_diff, staged_rows, staged_hunk_indices)) =
            self.compute_diff_for_review_mode(&file_path, cx)
        {
            let buffer_item = self.active_buffer(cx);
            buffer_item.update(cx, |item, _| {
                item.set_diff(Some(new_diff));
                item.set_staged_rows(staged_rows);
                item.set_staged_hunk_indices(staged_hunk_indices);
            });
        }

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();

        Ok(())
    }
}
