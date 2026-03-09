//! Diff review reset progress action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Reset all review progress and start from beginning.
    ///
    /// Clears all approved hunks from [`Stoat::diff_review_approved_hunks`] and resets to
    /// the first file with hunks at hunk index 0. If in review mode, loads the first file
    /// on-demand, computes its diff via [`Stoat::compute_diff_from_refs`], and jumps
    /// to the first hunk.
    pub fn diff_review_reset_progress(&mut self, cx: &mut Context<Self>) {
        debug!("Resetting diff review progress");

        self.review_state.approved_hunks.clear();

        if self.mode != "diff_review" || self.review_state.files.is_empty() {
            cx.notify();
            return;
        }

        let git = self.services.git.clone();
        let fs = self.services.fs.clone();
        let root_path = self.worktree.lock().root().to_path_buf();
        let files = self.review_state.files.clone();
        let comparison_mode = self.review_comparison_mode();

        cx.spawn(async move |this, cx| {
            let repo = match git.discover(&root_path).await {
                Ok(r) => r,
                Err(_) => {
                    this.update(cx, |_s, cx| cx.notify()).ok();
                    return Some(());
                },
            };

            for (idx, file_path) in files.iter().enumerate() {
                let abs_path = repo.workdir().join(file_path);

                let content = match fs.read_to_string(&abs_path).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("Failed to load file {:?}: {}", abs_path, e);
                        continue;
                    },
                };
                let mtime = fs.metadata(&abs_path).await.ok().and_then(|m| m.modified);
                let head = repo.head_content(&abs_path).await.ok();
                let index = repo.index_content(&abs_path).await.ok();

                let found = this
                    .update(cx, |s, cx| {
                        if s.load_file_from_content(
                            &abs_path,
                            &content,
                            mtime,
                            head.as_deref(),
                            index.as_deref(),
                            cx,
                        )
                        .is_err()
                        {
                            return false;
                        }

                        if let Some((diff, staged_rows, staged_hunk_indices)) = s
                            .compute_diff_from_refs(
                                &abs_path,
                                head.as_deref(),
                                index.as_deref(),
                                None,
                                None,
                                cx,
                            )
                        {
                            if !diff.hunks.is_empty() {
                                let buffer_item = s.active_buffer(cx);
                                buffer_item.update(cx, |item, _| {
                                    item.set_diff(Some(diff.clone()));
                                    item.set_staged_rows(staged_rows);
                                    item.set_staged_hunk_indices(staged_hunk_indices);
                                });

                                s.review_state.file_idx = idx;
                                s.review_state.hunk_idx = 0;
                                s.jump_to_current_hunk(true, cx);
                                cx.notify();
                                return true;
                            }
                        }
                        false
                    })
                    .ok()
                    .unwrap_or(false);

                if found {
                    if let Ok(counts) = repo.count_hunks_by_file(comparison_mode).await {
                        this.update(cx, |s, _| s.update_hunk_position_cache(&counts))
                            .ok();
                    }
                    return Some(());
                }
            }

            this.update(cx, |_s, cx| cx.notify()).ok();
            Some(())
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn resets_progress(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| s.open_diff_review(cx));
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            if s.mode() == "diff_review" && !s.review_state.files.is_empty() {
                s.diff_review_approve_hunk(cx);
                s.diff_review_reset_progress(cx);
                // Approvals cleared synchronously
                assert!(s.review_state.approved_hunks.is_empty());
            }
        });
    }
}
