//! Diff review cycle comparison mode action implementation and tests.

use crate::{git::diff_review::DiffComparisonMode, stoat::Stoat};
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

        let git = self.services.git.clone();
        let fs = self.services.fs.clone();
        let root_path = self.worktree.lock().root().to_path_buf();
        let current_file_idx = self.review_state.file_idx;
        let current_files = self.review_state.files.clone();

        cx.spawn(async move |this, cx| {
            let repo = match git.discover(&root_path).await {
                Ok(r) => r,
                Err(_) => {
                    this.update(cx, |s, cx| {
                        s.review_state.cached_hunk_totals = None;
                        cx.emit(crate::stoat::StoatEvent::Changed);
                    })
                    .ok();
                    return Some(());
                },
            };

            if new_source.is_commit() && !old_source.is_commit() {
                let file_paths = repo
                    .commit_changed_files()
                    .await
                    .ok()
                    .filter(|p| !p.is_empty());

                if let Some(file_paths) = file_paths {
                    this.update(cx, |s, _cx| {
                        s.review_state.files = file_paths.clone();
                        s.review_state.file_idx = 0;
                        s.review_state.hunk_idx = 0;
                        s.review_state.approved_hunks.clear();
                    })
                    .ok()?;

                    for (idx, file_path) in file_paths.iter().enumerate() {
                        let abs_path = repo.workdir().join(file_path);
                        let content = match fs.read_to_string(&abs_path).await {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::warn!("Failed to load file {:?}: {e}", abs_path);
                                continue;
                            },
                        };
                        let mtime = fs.metadata(&abs_path).await.ok().and_then(|m| m.modified);
                        let head = repo.head_content(&abs_path).await.ok();
                        let index = repo.index_content(&abs_path).await.ok();
                        let parent = Some(repo.parent_content(&abs_path).await.unwrap_or_default());

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

                                if let Some(ref h) = head {
                                    let buffer_item = s.active_buffer(cx);
                                    s.replace_buffer_content(h, &buffer_item, cx);
                                }

                                if let Some((diff, staged_rows, staged_hunk_indices)) = s
                                    .compute_diff_from_refs(
                                        &abs_path,
                                        None,
                                        None,
                                        parent.as_deref(),
                                        Some(&content),
                                        cx,
                                    )
                                {
                                    if !diff.hunks.is_empty() {
                                        let buffer_item = s.active_buffer(cx);
                                        buffer_item.update(cx, |item, _| {
                                            item.set_diff(Some(diff));
                                            item.set_staged_rows(staged_rows);
                                            item.set_staged_hunk_indices(staged_hunk_indices);
                                        });
                                        s.review_state.file_idx = idx;
                                        s.review_state.hunk_idx = 0;
                                        s.jump_to_current_hunk(false, cx);
                                        return true;
                                    }
                                }
                                false
                            })
                            .ok()
                            .unwrap_or(false);

                        if found {
                            break;
                        }
                    }
                }
            } else if !new_source.is_commit() && old_source.is_commit() {
                let entries = repo.gather_status().await.ok();
                if let Some(entries) = entries {
                    let mut seen = std::collections::HashSet::new();
                    let new_files: Vec<std::path::PathBuf> = entries
                        .into_iter()
                        .filter(|e| seen.insert(e.path.clone()))
                        .map(|e| e.path)
                        .collect();

                    let first_abs = new_files.first().map(|fp| repo.workdir().join(fp));

                    let file_data = if let Some(ref abs_path) = first_abs {
                        let content = fs.read_to_string(abs_path).await.ok();
                        let mtime = fs.metadata(abs_path).await.ok().and_then(|m| m.modified);
                        let head = repo.head_content(abs_path).await.ok();
                        let index = repo.index_content(abs_path).await.ok();
                        Some((content, mtime, head, index))
                    } else {
                        None
                    };

                    this.update(cx, |s, cx| {
                        s.review_state.files = new_files;
                        s.review_state.file_idx = 0;
                        s.review_state.hunk_idx = 0;
                        s.review_state.approved_hunks.clear();

                        if let (Some(abs_path), Some((Some(content), mtime, head, index))) =
                            (first_abs, file_data)
                        {
                            let _ = s.load_file_from_content(
                                &abs_path,
                                &content,
                                mtime,
                                head.as_deref(),
                                index.as_deref(),
                                cx,
                            );

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
                                let buffer_item = s.active_buffer(cx);
                                buffer_item.update(cx, |item, _| {
                                    item.set_diff(Some(diff));
                                    item.set_staged_rows(staged_rows);
                                    item.set_staged_hunk_indices(staged_hunk_indices);
                                });
                            }

                            s.jump_to_current_hunk(false, cx);
                        }
                    })
                    .ok()?;
                }
            } else {
                let current_file_path = current_files.get(current_file_idx).cloned();
                if let Some(current_file_path) = current_file_path {
                    let abs_path = repo.workdir().join(&current_file_path);

                    let needs_buffer_replace = matches!(
                        new_mode,
                        DiffComparisonMode::IndexVsHead | DiffComparisonMode::HeadVsParent
                    );

                    let file_content = fs.read_to_string(&abs_path).await.ok();
                    let file_mtime = fs.metadata(&abs_path).await.ok().and_then(|m| m.modified);
                    let head = repo.head_content(&abs_path).await.ok();
                    let index = repo.index_content(&abs_path).await.ok();
                    let parent = if matches!(new_mode, DiffComparisonMode::HeadVsParent) {
                        Some(repo.parent_content(&abs_path).await.unwrap_or_default())
                    } else {
                        None
                    };

                    this.update(cx, |s, cx| {
                        if needs_buffer_replace {
                            let replace_content = match new_mode {
                                DiffComparisonMode::IndexVsHead => index.as_deref(),
                                DiffComparisonMode::HeadVsParent => head.as_deref(),
                                _ => None,
                            };
                            if let Some(content) = replace_content {
                                let buffer_item = s.active_buffer(cx);
                                s.replace_buffer_content(content, &buffer_item, cx);
                            }
                        } else if let Some(ref content) = file_content {
                            let _ = s.load_file_from_content(
                                &abs_path,
                                content,
                                file_mtime,
                                head.as_deref(),
                                index.as_deref(),
                                cx,
                            );
                        }

                        let (d_head, d_index, d_parent, d_working) = match new_mode {
                            DiffComparisonMode::WorkingVsHead => {
                                (head.as_deref(), index.as_deref(), None, None)
                            },
                            DiffComparisonMode::WorkingVsIndex => {
                                (None, index.as_deref(), None, None)
                            },
                            DiffComparisonMode::IndexVsHead => (head.as_deref(), None, None, None),
                            DiffComparisonMode::HeadVsParent => {
                                (None, None, parent.as_deref(), file_content.as_deref())
                            },
                        };

                        if let Some((new_diff, staged_rows, staged_hunk_indices)) = s
                            .compute_diff_from_refs(
                                &abs_path, d_head, d_index, d_parent, d_working, cx,
                            )
                        {
                            let buffer_item = s.active_buffer(cx);
                            buffer_item.update(cx, |item, _cx| {
                                item.set_diff(Some(new_diff.clone()));
                                item.set_staged_rows(staged_rows);
                                item.set_staged_hunk_indices(staged_hunk_indices);
                            });

                            let hunk_count = new_diff.hunks.len();
                            if s.review_state.hunk_idx >= hunk_count {
                                s.review_state.hunk_idx = 0;
                            }

                            if hunk_count > 0 {
                                s.jump_to_current_hunk(true, cx);
                            } else {
                                s.reset_cursor_to_origin(cx);
                            }
                        } else {
                            let buffer_item = s.active_buffer(cx);
                            buffer_item.update(cx, |item, _| {
                                item.set_diff(None);
                            });
                            s.reset_cursor_to_origin(cx);
                        }
                    })
                    .ok()?;
                }
            }

            let new_mode = this.update(cx, |s, _| s.review_comparison_mode()).ok();
            if let Some(mode) = new_mode {
                if let Ok(counts) = repo.count_hunks_by_file(mode).await {
                    this.update(cx, |s, _| s.update_hunk_position_cache(&counts))
                        .ok();
                }
            }

            this.update(cx, |_, cx| {
                cx.emit(crate::stoat::StoatEvent::Changed);
            })
            .ok();

            Some(())
        })
        .detach();
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
            buffer_snapshot,
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
        stoat.update(|s, cx| s.open_diff_review(cx));
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            if s.mode() == "diff_review" && !s.review_state.files.is_empty() {
                let initial_mode = s.review_comparison_mode();
                s.diff_review_cycle_comparison_mode(cx);
                assert_ne!(s.review_comparison_mode(), initial_mode);
            }
        });
    }
}
