use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Revert the current hunk in LastCommit source by applying the reverse patch
    /// to the working tree.
    pub fn diff_review_revert_hunk(&mut self, cx: &mut Context<Self>) {
        if !self.review_state.source.is_commit() {
            return;
        }

        let file_path = match self.current_file_path.as_ref() {
            Some(p) => p.clone(),
            None => {
                tracing::error!("diff_review_revert_hunk: No file path set");
                return;
            },
        };

        let cursor_row = self.cursor.position().row;
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

        let diff = match buffer_item.read(cx).diff() {
            Some(d) => d,
            None => {
                tracing::error!("diff_review_revert_hunk: No diff information available");
                return;
            },
        };

        let hunk_index = match diff.hunk_for_row(cursor_row, buffer_snapshot) {
            Some(idx) => idx,
            None => {
                tracing::error!("diff_review_revert_hunk: No hunk at cursor row {cursor_row}");
                return;
            },
        };

        let hunk = &diff.hunks[hunk_index];

        let patch = match super::super::hunk_patch::generate_hunk_patch(
            diff,
            hunk,
            buffer_snapshot,
            &file_path,
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("diff_review_revert_hunk: {e}");
                return;
            },
        };

        let repo_dir = self.worktree_root_abs();
        let services = self.services.clone();

        cx.spawn(async move |this, cx| {
            let result = async {
                let repo = services
                    .git
                    .open(&repo_dir)
                    .await
                    .map_err(|e| format!("Failed to open repository: {e}"))?;
                super::super::hunk_patch::apply_patch(
                    &patch,
                    &*repo,
                    true,
                    crate::git::provider::ApplyLocation::WorkDir,
                )
                .await?;
                tracing::info!("Reverted hunk at row {cursor_row} in {:?}", file_path);
                Ok::<(), String>(())
            }
            .await;
            let _ = this.update(cx, |stoat, cx| {
                if let Err(e) = result {
                    tracing::error!("diff_review_revert_hunk: {e}");
                    return;
                }
                stoat.refresh_git_diff(cx);
                cx.emit(crate::stoat::StoatEvent::Changed);
                cx.notify();
            });
        })
        .detach();
    }
}
