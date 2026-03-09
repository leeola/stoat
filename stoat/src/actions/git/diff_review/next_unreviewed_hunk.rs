//! Diff review next unreviewed hunk action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Jump to next unreviewed hunk across all files.
    ///
    /// Searches files on-demand for the next unreviewed hunk (not in
    /// [`Stoat::diff_review_approved_hunks`]). Loads each file, computes diff via
    /// [`Stoat::compute_diff_from_refs`], and checks for unreviewed hunks. Wraps around
    /// to the beginning if needed. Exits review mode via [`Stoat::diff_review_dismiss`] if all
    /// hunks reviewed.
    ///
    /// # Workflow
    ///
    /// 1. Search current file from next hunk onward
    /// 2. Search remaining files (load each on-demand)
    /// 3. Search current file from beginning up to start hunk (wrap-around)
    /// 4. If no unreviewed hunks found: dismiss review mode
    pub fn diff_review_next_unreviewed_hunk(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        if self.review_state.files.is_empty() {
            return;
        }

        let empty_set = std::collections::HashSet::new();

        // First: check current file from next hunk onward (no IO needed)
        let start_file = self.review_state.file_idx;
        let start_hunk = self.review_state.hunk_idx + 1;

        if let Some(current_file_path) = self.review_state.files.get(start_file) {
            let approved_hunks = self
                .review_state
                .approved_hunks
                .get(current_file_path)
                .unwrap_or(&empty_set);

            let hunk_count = {
                let buffer_item = self.active_buffer(cx);
                let item = buffer_item.read(cx);
                item.diff().map(|d| d.hunks.len()).unwrap_or(0)
            };

            if let Some(hunk_idx) =
                (start_hunk..hunk_count).find(|idx| !approved_hunks.contains(idx))
            {
                self.review_state.hunk_idx = hunk_idx;
                self.jump_to_current_hunk(true, cx);
                cx.notify();
                return;
            }
        }

        // Need to search other files - requires IO
        let git = self.services.git.clone();
        let fs = self.services.fs.clone();
        let root_path = self.worktree.lock().root().to_path_buf();
        let file_count = self.review_state.files.len();
        let files = self.review_state.files.clone();
        let approved_hunks = self.review_state.approved_hunks.clone();
        let comparison_mode = self.review_comparison_mode();

        cx.spawn(async move |this, cx| {
            let repo = match git.discover(&root_path).await {
                Ok(r) => r,
                Err(_) => return Some(()),
            };

            let empty_set = std::collections::HashSet::new();

            // Search remaining files
            for offset in 1..file_count {
                let file_idx = (start_file + offset) % file_count;
                if file_idx == start_file {
                    break;
                }

                let file_path = files[file_idx].clone();
                let abs_path = repo.workdir().join(&file_path);

                let content = match fs.read_to_string(&abs_path).await {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let mtime = fs.metadata(&abs_path).await.ok().and_then(|m| m.modified);
                let head = repo.head_content(&abs_path).await.ok();
                let index = repo.index_content(&abs_path).await.ok();
                let parent = if matches!(
                    comparison_mode,
                    crate::git::diff_review::DiffComparisonMode::HeadVsParent
                ) {
                    Some(repo.parent_content(&abs_path).await.unwrap_or_default())
                } else {
                    None
                };

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
                                parent.as_deref(),
                                Some(&content),
                                cx,
                            )
                        {
                            let file_approved =
                                approved_hunks.get(&file_path).unwrap_or(&empty_set);

                            if let Some(hunk_idx) =
                                (0..diff.hunks.len()).find(|idx| !file_approved.contains(idx))
                            {
                                let buffer_item = s.active_buffer(cx);
                                buffer_item.update(cx, |item, _| {
                                    item.set_diff(Some(diff));
                                    item.set_staged_rows(staged_rows);
                                    item.set_staged_hunk_indices(staged_hunk_indices);
                                });
                                s.review_state.file_idx = file_idx;
                                s.review_state.hunk_idx = hunk_idx;
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

            // Search current file from beginning up to start_hunk
            let found_in_current = this
                .update(cx, |s, cx| {
                    if let Some(current_file_path) = s.review_state.files.get(start_file) {
                        let file_approved = s
                            .review_state
                            .approved_hunks
                            .get(current_file_path)
                            .unwrap_or(&empty_set);

                        if let Some(hunk_idx) =
                            (0..start_hunk).find(|idx| !file_approved.contains(idx))
                        {
                            s.review_state.hunk_idx = hunk_idx;
                            s.jump_to_current_hunk(true, cx);
                            cx.notify();
                            return true;
                        }
                    }
                    false
                })
                .ok()
                .unwrap_or(false);

            if !found_in_current {
                debug!("All hunks reviewed");
                this.update(cx, |s, cx| {
                    s.diff_review_dismiss(cx);
                })
                .ok();
            }

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
    fn finds_next_unreviewed(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| s.open_diff_review(cx));
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            if s.mode() == "diff_review" {
                s.diff_review_next_unreviewed_hunk(cx);
            }
        });
    }
}
