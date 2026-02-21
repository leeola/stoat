//! Diff review cycle comparison mode action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Cycle through [`DiffSource`] variants: All, Unstaged, Staged, LastCommit.
    ///
    /// When transitioning to [`DiffSource::LastCommit`], replaces the file list
    /// with commit-changed files. When transitioning away from it, re-gathers
    /// working tree status. Between All/Unstaged/Staged, keeps the file list
    /// and recomputes the diff.
    pub fn diff_review_cycle_comparison_mode(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        let old_source = self.review_state.source;
        let new_source = old_source.next();
        self.review_state.source = new_source;
        let new_mode = self.review_comparison_mode();
        debug!("Cycling diff source: {old_source:?} -> {new_source:?} (mode={new_mode:?})");

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git::repository::Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => return,
        };

        if new_source.is_commit() && !old_source.is_commit() {
            self.enter_last_commit_source(&repo, cx);
        } else if !new_source.is_commit() && old_source.is_commit() {
            self.exit_last_commit_source(&repo, cx);
        } else {
            self.recompute_current_file_diff(&repo, new_mode, cx);
        }

        cx.emit(crate::stoat::StoatEvent::Changed);
    }

    fn enter_last_commit_source(
        &mut self,
        repo: &crate::git::repository::Repository,
        cx: &mut Context<Self>,
    ) {
        let file_paths = match repo.commit_changed_files() {
            Ok(paths) => paths,
            Err(e) => {
                debug!("No commit changes found: {e}");
                return;
            },
        };

        if file_paths.is_empty() {
            debug!("No files changed in last commit");
            return;
        }

        self.review_state.files = file_paths;
        self.review_state.file_idx = 0;
        self.review_state.hunk_idx = 0;
        self.review_state.approved_hunks.clear();

        for (idx, file_path) in self.review_state.files.clone().iter().enumerate() {
            let abs_path = repo.workdir().join(file_path);

            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {e}", abs_path);
                continue;
            }

            match repo.head_content(&abs_path) {
                Ok(content) => {
                    let buffer_item = self.active_buffer(cx);
                    self.replace_buffer_content(&content, &buffer_item, cx);
                },
                Err(e) => tracing::warn!("Failed to read head content for {abs_path:?}: {e}"),
            }

            if let Some((diff, staged_rows, staged_hunk_indices)) =
                self.compute_diff_for_review_mode(&abs_path, cx)
            {
                if !diff.hunks.is_empty() {
                    let buffer_item = self.active_buffer(cx);
                    buffer_item.update(cx, |item, _| {
                        item.set_diff(Some(diff));
                        item.set_staged_rows(staged_rows);
                        item.set_staged_hunk_indices(staged_hunk_indices);
                    });

                    self.review_state.file_idx = idx;
                    self.review_state.hunk_idx = 0;
                    self.jump_to_current_hunk(false, cx);
                    return;
                }
            }
        }

        debug!("No files with hunks in LastCommit source");
    }

    fn exit_last_commit_source(
        &mut self,
        repo: &crate::git::repository::Repository,
        cx: &mut Context<Self>,
    ) {
        let entries = match crate::git::status::gather_git_status(repo.inner()) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        let mut seen = std::collections::HashSet::new();
        let new_files: Vec<std::path::PathBuf> = entries
            .into_iter()
            .filter(|e| seen.insert(e.path.clone()))
            .map(|e| e.path)
            .collect();

        self.review_state.files = new_files;
        self.review_state.file_idx = 0;
        self.review_state.hunk_idx = 0;
        self.review_state.approved_hunks.clear();

        if let Some(file_path) = self.review_state.files.first().cloned() {
            let abs_path = repo.workdir().join(&file_path);
            let _ = self.load_file(&abs_path, cx);
            self.refresh_git_diff(cx);

            if let Some((diff, staged_rows, staged_hunk_indices)) =
                self.compute_diff_for_review_mode(&abs_path, cx)
            {
                let buffer_item = self.active_buffer(cx);
                buffer_item.update(cx, |item, _| {
                    item.set_diff(Some(diff));
                    item.set_staged_rows(staged_rows);
                    item.set_staged_hunk_indices(staged_hunk_indices);
                });
            }

            self.jump_to_current_hunk(false, cx);
        }
    }

    fn recompute_current_file_diff(
        &mut self,
        repo: &crate::git::repository::Repository,
        new_mode: crate::git::diff_review::DiffComparisonMode,
        cx: &mut Context<Self>,
    ) {
        let current_file_path = match self.review_state.files.get(self.review_state.file_idx) {
            Some(path) => path.clone(),
            None => return,
        };

        let abs_path = repo.workdir().join(&current_file_path);

        let needs_buffer_replace = matches!(
            new_mode,
            crate::git::diff_review::DiffComparisonMode::IndexVsHead
                | crate::git::diff_review::DiffComparisonMode::HeadVsParent
        );

        if needs_buffer_replace {
            let content = match new_mode {
                crate::git::diff_review::DiffComparisonMode::IndexVsHead => {
                    repo.index_content(&abs_path).ok()
                },
                crate::git::diff_review::DiffComparisonMode::HeadVsParent => {
                    repo.head_content(&abs_path).ok()
                },
                _ => None,
            };

            if let Some(content) = content {
                let buffer_item = self.active_buffer(cx);
                self.replace_buffer_content(&content, &buffer_item, cx);
            }
        } else {
            let _ = self.load_file(&abs_path, cx);
        }

        if let Some((new_diff, staged_rows, staged_hunk_indices)) =
            self.compute_diff_for_review_mode(&abs_path, cx)
        {
            let buffer_item = self.active_buffer(cx);
            buffer_item.update(cx, |item, _cx| {
                item.set_diff(Some(new_diff.clone()));
                item.set_staged_rows(staged_rows);
                item.set_staged_hunk_indices(staged_hunk_indices);
            });

            let hunk_count = new_diff.hunks.len();
            if self.review_state.hunk_idx >= hunk_count {
                self.review_state.hunk_idx = 0;
            }

            if hunk_count > 0 {
                self.jump_to_current_hunk(true, cx);
            } else {
                self.reset_cursor_to_origin(cx);
            }
        } else {
            let buffer_item = self.active_buffer(cx);
            buffer_item.update(cx, |item, _| {
                item.set_diff(None);
            });
            self.reset_cursor_to_origin(cx);
        }
    }

    fn reset_cursor_to_origin(&mut self, cx: &mut Context<Self>) {
        let target_pos = text::Point::new(0, 0);
        self.cursor.move_to(target_pos);

        let buffer_snapshot = self.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
        let id = self.selections.next_id();
        self.selections.select(
            vec![text::Selection {
                id,
                start: target_pos,
                end: target_pos,
                reversed: false,
                goal: text::SelectionGoal::None,
            }],
            &buffer_snapshot,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn cycles_comparison_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            if s.mode() == "diff_review" && !s.review_state.files.is_empty() {
                let initial_mode = s.review_comparison_mode();
                s.diff_review_cycle_comparison_mode(cx);
                assert_ne!(s.review_comparison_mode(), initial_mode);
            }
        });
    }
}
