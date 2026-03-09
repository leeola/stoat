//! Action implementations for Stoat.
//!
//! These demonstrate the Context<Self> pattern - methods can spawn self-updating tasks.

use crate::stoat::Stoat;
use gpui::Context;
use text::ToPoint;
use tracing::debug;

impl Stoat {
    // ==== Git status helper methods ====
    // FIXME: Git status methods moved to AppState and PaneGroupView as part of Phase 3.
    // The following methods have been removed:
    // - load_git_diff_preview() -> moved to PaneGroupView::load_git_status_preview()
    // - filter_git_status_files() -> logic moved to AppState::open_git_status()
    // - git_status_files(), git_status_filtered(), git_status_filter() -> access via
    //   app_state.git_status
    // - git_status_branch_info() -> access via app_state.git_status.branch_info
    // - git_status_selected() -> access via app_state.git_status.selected
    // - git_status_preview() -> access via app_state.git_status.preview
    // - git_dirty_count() -> access via app_state.git_status.dirty_count

    /// Accessor for current file path (for status bar).
    pub fn current_file_path(&self) -> Option<&std::path::Path> {
        self.current_file_path.as_deref()
    }

    // ==== Diff review helper methods ====

    /// Jump cursor to the start of the current hunk.
    ///
    /// Uses the current file and hunk indices to position the cursor and scroll
    /// the view to show the hunk. Following Zed's go_to_hunk pattern.
    ///
    /// Implements smart scrolling:
    /// - If hunk fits in viewport: centers the hunk
    /// - If hunk is too large: positions top of hunk at 1/3 from viewport top
    pub fn jump_to_current_hunk(&mut self, animate: bool, cx: &mut Context<Self>) {
        // Get the diff from the buffer item (has fresh anchors) instead of GitIndex (has stale
        // anchors)
        let buffer_item = self.active_buffer(cx);
        let (diff, buffer_snapshot) = {
            let item = buffer_item.read(cx);
            let diff = match item.diff() {
                Some(d) => d.clone(),
                None => return,
            };
            let buffer_snapshot = item.buffer().read(cx).snapshot();
            (diff, buffer_snapshot)
        };

        if self.review_state.hunk_idx >= diff.hunks.len() {
            return;
        }

        let hunk = &diff.hunks[self.review_state.hunk_idx];

        // Convert hunk anchors to points
        let hunk_start = hunk.buffer_range.start.to_point(buffer_snapshot);
        let hunk_end = hunk.buffer_range.end.to_point(buffer_snapshot);

        let hunk_idx = self.review_state.hunk_idx;
        let start_row = hunk_start.row;

        // Move cursor to hunk start (always in buffer coordinates)
        self.cursor.move_to(hunk_start);

        // Smart scrolling based on hunk size
        if let Some(viewport_lines) = self.viewport_lines {
            // In diff review, phantom rows shift display rows relative to buffer rows.
            // Convert to display coordinates so the viewport targets the right position.
            let (display_start_row, display_end_row) = if self.is_in_diff_review(cx) {
                let mode = Some(self.review_comparison_mode());
                let display_buffer = buffer_item.read(cx).display_buffer(cx, true, mode);
                let start = display_buffer.buffer_row_to_display(hunk_start.row).0;
                let end = display_buffer.buffer_row_to_display(hunk_end.row).0;
                (start as f32, end as f32)
            } else {
                (hunk_start.row as f32, hunk_end.row as f32)
            };

            let hunk_height = display_end_row - display_start_row;

            let target_scroll_y = if hunk_height < viewport_lines * 0.4 {
                let hunk_middle = display_start_row + (hunk_height / 2.0);
                (hunk_middle - (viewport_lines / 2.0)).max(0.0)
            } else {
                const TOP_PADDING: f32 = 3.0;
                (display_start_row - TOP_PADDING).max(0.0)
            };

            let target = gpui::point(self.scroll.position.x, target_scroll_y);
            if animate {
                self.scroll.start_animation_to(target);
            } else {
                self.scroll.scroll_to(target);
            }
        } else {
            self.ensure_cursor_visible(cx);
        }

        debug!(hunk = hunk_idx, line = start_row, "Jumped to hunk");
    }

    /// Load next file in diff review.
    ///
    /// Searches forward through the file list for the next file with hunks,
    /// wrapping to the first file if at the end.
    pub fn load_next_file(&mut self, cx: &mut Context<Self>) {
        if self.review_state.files.is_empty() {
            return;
        }

        let git = self.services.git.clone();
        let fs = self.services.fs.clone();
        let root_path = self.worktree.lock().root().to_path_buf();
        let file_count = self.review_state.files.len();
        let current_idx = self.review_state.file_idx;
        let files = self.review_state.files.clone();
        let comparison_mode = self.review_comparison_mode();

        cx.spawn(async move |this, cx| {
            let repo = match git.discover(&root_path).await {
                Ok(r) => r,
                Err(_) => return Some(()),
            };

            for offset in 1..=file_count {
                let next_idx = (current_idx + offset) % file_count;
                let file_path = &files[next_idx];
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

                        // Replace buffer content for non-working modes
                        match comparison_mode {
                            crate::git::diff_review::DiffComparisonMode::IndexVsHead => {
                                if let Some(ref idx_content) = index {
                                    let buffer_item = s.active_buffer(cx);
                                    s.replace_buffer_content(idx_content, &buffer_item, cx);
                                }
                            },
                            crate::git::diff_review::DiffComparisonMode::HeadVsParent => {
                                if let Some(ref h) = head {
                                    let buffer_item = s.active_buffer(cx);
                                    s.replace_buffer_content(h, &buffer_item, cx);
                                }
                            },
                            _ => {},
                        }

                        let (d_head, d_index, d_parent, d_working) = match comparison_mode {
                            crate::git::diff_review::DiffComparisonMode::WorkingVsHead => {
                                (head.as_deref(), index.as_deref(), None, None)
                            },
                            crate::git::diff_review::DiffComparisonMode::WorkingVsIndex => {
                                (None, index.as_deref(), None, None)
                            },
                            crate::git::diff_review::DiffComparisonMode::IndexVsHead => {
                                (head.as_deref(), None, None, None)
                            },
                            crate::git::diff_review::DiffComparisonMode::HeadVsParent => {
                                (None, None, parent.as_deref(), Some(content.as_str()))
                            },
                        };

                        if let Some((diff, staged_rows, staged_hunk_indices)) = s
                            .compute_diff_from_refs(
                                &abs_path, d_head, d_index, d_parent, d_working, cx,
                            )
                        {
                            if !diff.hunks.is_empty() {
                                let buffer_item = s.active_buffer(cx);
                                buffer_item.update(cx, |item, _| {
                                    item.set_diff(Some(diff.clone()));
                                    item.set_staged_rows(staged_rows);
                                    item.set_staged_hunk_indices(staged_hunk_indices);
                                });

                                debug!(
                                    "Loaded next file with {} hunks at idx={}",
                                    diff.hunks.len(),
                                    next_idx
                                );

                                s.review_state.file_idx = next_idx;
                                s.review_state.hunk_idx = 0;
                                s.jump_to_current_hunk(true, cx);
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

            debug!("No more files with hunks in current comparison mode");
            this.update(cx, |s, cx| {
                let buffer_item = s.active_buffer(cx);
                buffer_item.update(cx, |item, _| {
                    item.set_diff(None);
                });
                s.cursor.move_to(text::Point::new(0, 0));
            })
            .ok();

            Some(())
        })
        .detach();
    }

    /// Load previous file in diff review.
    ///
    /// Searches backward through the file list for the previous file with hunks,
    /// wrapping to the last file if at the beginning.
    pub fn load_prev_file(&mut self, cx: &mut Context<Self>) {
        if self.review_state.files.is_empty() {
            return;
        }

        let git = self.services.git.clone();
        let fs = self.services.fs.clone();
        let root_path = self.worktree.lock().root().to_path_buf();
        let file_count = self.review_state.files.len();
        let current_idx = self.review_state.file_idx;
        let files = self.review_state.files.clone();
        let comparison_mode = self.review_comparison_mode();

        cx.spawn(async move |this, cx| {
            let repo = match git.discover(&root_path).await {
                Ok(r) => r,
                Err(_) => return Some(()),
            };

            for offset in 1..=file_count {
                let prev_idx = if current_idx >= offset {
                    current_idx - offset
                } else {
                    file_count - (offset - current_idx)
                };

                let file_path = &files[prev_idx];
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

                        match comparison_mode {
                            crate::git::diff_review::DiffComparisonMode::IndexVsHead => {
                                if let Some(ref idx_content) = index {
                                    let buffer_item = s.active_buffer(cx);
                                    s.replace_buffer_content(idx_content, &buffer_item, cx);
                                }
                            },
                            crate::git::diff_review::DiffComparisonMode::HeadVsParent => {
                                if let Some(ref h) = head {
                                    let buffer_item = s.active_buffer(cx);
                                    s.replace_buffer_content(h, &buffer_item, cx);
                                }
                            },
                            _ => {},
                        }

                        let (d_head, d_index, d_parent, d_working) = match comparison_mode {
                            crate::git::diff_review::DiffComparisonMode::WorkingVsHead => {
                                (head.as_deref(), index.as_deref(), None, None)
                            },
                            crate::git::diff_review::DiffComparisonMode::WorkingVsIndex => {
                                (None, index.as_deref(), None, None)
                            },
                            crate::git::diff_review::DiffComparisonMode::IndexVsHead => {
                                (head.as_deref(), None, None, None)
                            },
                            crate::git::diff_review::DiffComparisonMode::HeadVsParent => {
                                (None, None, parent.as_deref(), Some(content.as_str()))
                            },
                        };

                        if let Some((diff, staged_rows, staged_hunk_indices)) = s
                            .compute_diff_from_refs(
                                &abs_path, d_head, d_index, d_parent, d_working, cx,
                            )
                        {
                            if !diff.hunks.is_empty() {
                                let buffer_item = s.active_buffer(cx);
                                buffer_item.update(cx, |item, _| {
                                    item.set_diff(Some(diff.clone()));
                                    item.set_staged_rows(staged_rows);
                                    item.set_staged_hunk_indices(staged_hunk_indices);
                                });

                                debug!(
                                    "Loaded prev file with {} hunks at idx={}",
                                    diff.hunks.len(),
                                    prev_idx
                                );

                                let last_hunk_idx = diff.hunks.len().saturating_sub(1);
                                s.review_state.file_idx = prev_idx;
                                s.review_state.hunk_idx = last_hunk_idx;
                                s.jump_to_current_hunk(true, cx);
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

            debug!("No more files with hunks in current comparison mode");
            this.update(cx, |s, cx| {
                let buffer_item = s.active_buffer(cx);
                buffer_item.update(cx, |item, _| {
                    item.set_diff(None);
                });
                s.cursor.move_to(text::Point::new(0, 0));
            })
            .ok();

            Some(())
        })
        .detach();
    }

    /// Capture current per-file hunk counts into [`DiffReviewState::last_hunk_snapshot`].
    pub(crate) fn refresh_review_hunk_snapshot(&mut self, cx: &mut Context<Self>) {
        let git = self.services.git.clone();
        let root_path = self.worktree.lock().root().to_path_buf();
        let comparison_mode = self.review_comparison_mode();

        cx.spawn(async move |this, cx| {
            let repo = git.discover(&root_path).await.ok()?;
            let counts = repo.count_hunks_by_file(comparison_mode).await.ok()?;
            this.update(cx, |s, _cx| {
                s.review_state.last_hunk_snapshot = counts;
            })
            .ok();
            Some(())
        })
        .detach();
    }

    /// Among files whose hunk count increased (or are entirely new), return the one
    /// with the most recent mtime.
    async fn find_newest_change_async(
        old_snapshot: &std::collections::HashMap<std::path::PathBuf, usize>,
        new_counts: &std::collections::HashMap<std::path::PathBuf, usize>,
        workdir: &std::path::Path,
        fs: &dyn crate::fs::Fs,
    ) -> Option<std::path::PathBuf> {
        let mut best: Option<(std::path::PathBuf, std::time::SystemTime)> = None;

        for (path, &new_count) in new_counts {
            let old_count = old_snapshot.get(path).copied().unwrap_or(0);
            if new_count <= old_count {
                continue;
            }

            let abs_path = workdir.join(path);
            let mtime = fs
                .metadata(&abs_path)
                .await
                .ok()
                .and_then(|m| m.modified)
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

            if best
                .as_ref()
                .is_none_or(|(_, best_mtime)| mtime > *best_mtime)
            {
                best = Some((path.clone(), mtime));
            }
        }

        best.map(|(path, _)| path)
    }

    /// Refresh the diff review state from disk/git.
    ///
    /// Re-gathers git status to update the file list, reloads the current file's
    /// diff, and when follow mode is on, auto-navigates to the most recently
    /// modified file's new hunks.
    pub(crate) fn refresh_review_state(&mut self, cx: &mut Context<Self>) {
        let git = self.services.git.clone();
        let fs = self.services.fs.clone();
        let root_path = self.worktree.lock().root().to_path_buf();
        let files = self.review_state.files.clone();
        let file_idx = self.review_state.file_idx;
        let hunk_idx = self.review_state.hunk_idx;
        let source = self.review_state.source;
        let follow = self.review_state.follow;
        let last_snapshot = self.review_state.last_hunk_snapshot.clone();
        let comparison_mode = self.review_comparison_mode();

        cx.spawn(async move |this, cx| {
            let repo = match git.discover(&root_path).await {
                Ok(r) => r,
                Err(_) => return Some(()),
            };

            let new_files: Vec<std::path::PathBuf> = if source.is_commit() {
                match repo.commit_changed_files().await {
                    Ok(paths) => paths,
                    Err(_) => return Some(()),
                }
            } else {
                let entries = match repo.gather_status().await {
                    Ok(entries) => entries,
                    Err(_) => return Some(()),
                };
                let mut seen = std::collections::HashSet::new();
                entries
                    .into_iter()
                    .filter(|e| seen.insert(e.path.clone()))
                    .map(|e| e.path)
                    .collect()
            };

            let new_counts = match repo.count_hunks_by_file(comparison_mode).await {
                Ok(counts) => counts,
                Err(_) => return Some(()),
            };

            // Preserve file_idx by matching current file path in new list
            let current_path = files.get(file_idx).cloned();
            let new_file_idx = current_path
                .as_ref()
                .and_then(|current| new_files.iter().position(|p| p == current))
                .unwrap_or(0);

            // Clamp hunk_idx
            let clamped_hunk_idx = if let Some(ref current) = current_path {
                let max_hunks = new_counts.get(current).copied().unwrap_or(0);
                if max_hunks == 0 {
                    0
                } else if hunk_idx >= max_hunks {
                    max_hunks.saturating_sub(1)
                } else {
                    hunk_idx
                }
            } else {
                0
            };

            // Reload current file content for refresh_git_diff
            let current_file_data = if let Some(ref current) = current_path {
                let abs_path = repo.workdir().join(current);
                let content = fs.read_to_string(&abs_path).await.ok();
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
                let working = fs.read_to_string(&abs_path).await.ok();
                Some((abs_path, content, mtime, head, index, parent, working))
            } else {
                None
            };

            // Follow mode: find newest change
            let follow_target = if follow {
                Self::find_newest_change_async(&last_snapshot, &new_counts, repo.workdir(), &*fs)
                    .await
            } else {
                None
            };

            // If following, fetch target file data
            let follow_data = if let Some(ref target_path) = follow_target {
                let target_idx = new_files.iter().position(|p| *p == *target_path);
                let old_count = last_snapshot.get(target_path).copied().unwrap_or(0);

                if let Some(target_idx) = target_idx {
                    let abs_path = repo.workdir().join(target_path);
                    let content = fs.read_to_string(&abs_path).await.ok();
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
                    let working = fs.read_to_string(&abs_path).await.ok();

                    Some((
                        target_idx, old_count, abs_path, content, mtime, head, index, parent,
                        working,
                    ))
                } else {
                    None
                }
            } else {
                None
            };

            this.update(cx, |s, cx| {
                s.review_state.files = new_files;
                s.review_state.file_idx = new_file_idx;
                s.review_state.hunk_idx = clamped_hunk_idx;

                // Reload current file + refresh diff
                if let Some((abs_path, Some(content), mtime, head, index, parent, working)) =
                    current_file_data
                {
                    let _ = s.load_file_from_content(
                        &abs_path,
                        &content,
                        mtime,
                        head.as_deref(),
                        index.as_deref(),
                        cx,
                    );

                    let (d_head, d_index, d_parent, d_working) = match comparison_mode {
                        crate::git::diff_review::DiffComparisonMode::WorkingVsHead => {
                            (head.as_deref(), index.as_deref(), None, None)
                        },
                        crate::git::diff_review::DiffComparisonMode::WorkingVsIndex => {
                            (None, index.as_deref(), None, None)
                        },
                        crate::git::diff_review::DiffComparisonMode::IndexVsHead => {
                            (head.as_deref(), None, None, None)
                        },
                        crate::git::diff_review::DiffComparisonMode::HeadVsParent => {
                            (None, None, parent.as_deref(), working.as_deref())
                        },
                    };

                    if let Some((new_diff, staged_rows, staged_hunk_indices)) = s
                        .compute_diff_from_refs(&abs_path, d_head, d_index, d_parent, d_working, cx)
                    {
                        let buffer_item = s.active_buffer(cx);
                        buffer_item.update(cx, |item, _| {
                            item.set_diff(Some(new_diff));
                            item.set_staged_rows(staged_rows);
                            item.set_staged_hunk_indices(staged_hunk_indices);
                        });
                    }
                }

                // Follow mode: navigate to target
                if let Some((
                    target_idx,
                    old_count,
                    abs_path,
                    Some(content),
                    mtime,
                    head,
                    index,
                    parent,
                    working,
                )) = follow_data
                {
                    if s.load_file_from_content(
                        &abs_path,
                        &content,
                        mtime,
                        head.as_deref(),
                        index.as_deref(),
                        cx,
                    )
                    .is_ok()
                    {
                        match comparison_mode {
                            crate::git::diff_review::DiffComparisonMode::IndexVsHead => {
                                if let Some(ref idx_content) = index {
                                    let buffer_item = s.active_buffer(cx);
                                    s.replace_buffer_content(idx_content, &buffer_item, cx);
                                }
                            },
                            crate::git::diff_review::DiffComparisonMode::HeadVsParent => {
                                if let Some(ref h) = head {
                                    let buffer_item = s.active_buffer(cx);
                                    s.replace_buffer_content(h, &buffer_item, cx);
                                }
                            },
                            _ => {},
                        }

                        let (d_head, d_index, d_parent, d_working) = match comparison_mode {
                            crate::git::diff_review::DiffComparisonMode::WorkingVsHead => {
                                (head.as_deref(), index.as_deref(), None, None)
                            },
                            crate::git::diff_review::DiffComparisonMode::WorkingVsIndex => {
                                (None, index.as_deref(), None, None)
                            },
                            crate::git::diff_review::DiffComparisonMode::IndexVsHead => {
                                (head.as_deref(), None, None, None)
                            },
                            crate::git::diff_review::DiffComparisonMode::HeadVsParent => {
                                (None, None, parent.as_deref(), working.as_deref())
                            },
                        };

                        if let Some((diff, staged_rows, staged_hunk_indices)) = s
                            .compute_diff_from_refs(
                                &abs_path, d_head, d_index, d_parent, d_working, cx,
                            )
                        {
                            if !diff.hunks.is_empty() {
                                let buffer_item = s.active_buffer(cx);
                                buffer_item.update(cx, |item, _| {
                                    item.set_diff(Some(diff));
                                    item.set_staged_rows(staged_rows);
                                    item.set_staged_hunk_indices(staged_hunk_indices);
                                });

                                s.review_state.file_idx = target_idx;
                                s.review_state.hunk_idx = old_count;
                                s.jump_to_current_hunk(true, cx);
                            }
                        }
                    }
                }

                s.update_hunk_position_cache(&new_counts);
                s.review_state.last_hunk_snapshot = new_counts;
                cx.notify();
            })
            .ok();

            Some(())
        })
        .detach();
    }
}

/// Build the list of all available commands from action metadata.
/// including name, description, aliases, and TypeId for dispatch. This includes all
/// actions with metadata, regardless of whether they have keybindings.
///
/// # Returns
///
/// A vector of [`CommandInfo`] structs representing all available commands
pub fn build_command_list() -> Vec<crate::stoat::CommandInfo> {
    let mut commands = Vec::new();

    // Iterate through all actions with metadata
    for (type_id, name) in crate::actions::ACTION_NAMES.iter() {
        // Get description from new idiomatic registry
        // All actions have been migrated to use Action::documentation()
        let description = if let Some(doc) = crate::action_metadata::get_documentation(type_id) {
            doc
        } else {
            // No documentation available - skip this action
            continue;
        };

        // Get aliases (empty slice if none)
        let aliases = crate::actions::ALIASES
            .get(type_id)
            .copied()
            .unwrap_or(&[])
            .to_vec();

        if !aliases.is_empty() {
            tracing::info!("Command {} has aliases: {:?}", name, aliases);
        }

        // Get hidden flag (false if not in map)
        let hidden = crate::actions::HIDDEN
            .get(type_id)
            .copied()
            .unwrap_or(false);

        commands.push(crate::stoat::CommandInfo {
            name: name.to_string(),
            description: description.to_string(),
            aliases,
            type_id: *type_id,
            hidden,
        });
    }

    // Sort alphabetically by name
    commands.sort_by(|a, b| a.name.cmp(&b.name));

    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::TypeId;

    #[test]
    fn build_command_list_includes_movement_actions_from_new_registry() {
        let commands = build_command_list();

        let move_up_type_id = TypeId::of::<crate::actions::MoveUp>();
        let move_up_cmd = commands.iter().find(|cmd| cmd.type_id == move_up_type_id);

        assert!(move_up_cmd.is_some(), "MoveUp should be in command list");

        let cmd = move_up_cmd.unwrap();
        assert!(
            cmd.description.contains("Move cursor up"),
            "Description should come from Action::documentation(). Got: {:?}",
            cmd.description
        );
    }

    #[test]
    fn build_command_list_includes_all_movement_actions() {
        let commands = build_command_list();

        let movement_action_names = [
            "MoveUp",
            "MoveDown",
            "MoveLeft",
            "MoveRight",
            "MoveWordLeft",
            "MoveWordRight",
            "MoveToLineStart",
            "MoveToLineEnd",
            "MoveToFileStart",
            "MoveToFileEnd",
            "PageUp",
            "PageDown",
        ];

        for name in &movement_action_names {
            let found = commands.iter().any(|cmd| cmd.name == *name);
            assert!(found, "Movement action '{name}' should be in command list");
        }
    }

    #[test]
    fn build_command_list_includes_all_selection_actions() {
        let commands = build_command_list();

        let selection_action_names = [
            "MoveNextWordStart",
            "MovePrevWordStart",
            "MoveNextLongWordStart",
            "MovePrevLongWordStart",
            "SelectLeft",
            "SelectRight",
            "SelectUp",
            "SelectDown",
            "SelectToLineStart",
            "SelectToLineEnd",
        ];

        for name in &selection_action_names {
            let found = commands.iter().any(|cmd| cmd.name == *name);
            assert!(found, "Selection action '{name}' should be in command list");
        }

        let select_left_type_id = TypeId::of::<crate::actions::SelectLeft>();
        let select_left_cmd = commands
            .iter()
            .find(|cmd| cmd.type_id == select_left_type_id);

        assert!(
            select_left_cmd.is_some(),
            "SelectLeft should be in command list"
        );

        let cmd = select_left_cmd.unwrap();
        assert!(
            cmd.description.contains("Extend selection"),
            "Description should come from Action::documentation(). Got: {:?}",
            cmd.description
        );
    }

    #[test]
    fn build_command_list_includes_all_editing_actions() {
        let commands = build_command_list();

        let editing_action_names = [
            "DeleteLeft",
            "DeleteRight",
            "DeleteWordLeft",
            "DeleteWordRight",
            "NewLine",
            "DeleteLine",
            "DeleteToEndOfLine",
        ];

        for name in &editing_action_names {
            let found = commands.iter().any(|cmd| cmd.name == *name);
            assert!(found, "Editing action '{name}' should be in command list");
        }

        let delete_left_type_id = TypeId::of::<crate::actions::DeleteLeft>();
        let delete_left_cmd = commands
            .iter()
            .find(|cmd| cmd.type_id == delete_left_type_id);

        assert!(
            delete_left_cmd.is_some(),
            "DeleteLeft should be in command list"
        );

        let cmd = delete_left_cmd.unwrap();
        assert!(
            cmd.description.contains("Delete") && cmd.description.contains("character"),
            "Description should come from Action::documentation(). Got: {:?}",
            cmd.description
        );
    }

    #[test]
    fn build_command_list_includes_all_mode_actions() {
        let commands = build_command_list();

        let mode_action_names = [
            "EnterInsertMode",
            "EnterNormalMode",
            "EnterVisualMode",
            "EnterSpaceMode",
            "EnterPaneMode",
            "EnterGitFilterMode",
        ];

        for name in &mode_action_names {
            let found = commands.iter().any(|cmd| cmd.name == *name);
            assert!(found, "Mode action '{name}' should be in command list");
        }

        let insert_mode_type_id = TypeId::of::<crate::actions::EnterInsertMode>();
        let insert_mode_cmd = commands
            .iter()
            .find(|cmd| cmd.type_id == insert_mode_type_id);

        assert!(
            insert_mode_cmd.is_some(),
            "EnterInsertMode should be in command list"
        );

        let cmd = insert_mode_cmd.unwrap();
        assert!(
            cmd.description.contains("Enter insert mode"),
            "Description should come from Action::documentation(). Got: {:?}",
            cmd.description
        );
    }

    #[test]
    fn build_command_list_includes_all_file_finder_actions() {
        let commands = build_command_list();

        let file_finder_action_names = [
            "OpenFileFinder",
            "FileFinderNext",
            "FileFinderPrev",
            "FileFinderSelect",
            "FileFinderDismiss",
        ];

        for name in &file_finder_action_names {
            let found = commands.iter().any(|cmd| cmd.name == *name);
            assert!(
                found,
                "File finder action '{name}' should be in command list"
            );
        }

        let open_type_id = TypeId::of::<crate::actions::OpenFileFinder>();
        let open_cmd = commands.iter().find(|cmd| cmd.type_id == open_type_id);

        assert!(
            open_cmd.is_some(),
            "OpenFileFinder should be in command list"
        );

        let cmd = open_cmd.unwrap();
        assert!(
            cmd.description.contains("file finder"),
            "Description should come from Action::documentation(). Got: {:?}",
            cmd.description
        );
    }

    #[test]
    fn build_command_list_includes_all_buffer_finder_actions() {
        let commands = build_command_list();

        let buffer_finder_action_names = [
            "OpenBufferFinder",
            "BufferFinderNext",
            "BufferFinderPrev",
            "BufferFinderSelect",
            "BufferFinderDismiss",
        ];

        for name in &buffer_finder_action_names {
            let found = commands.iter().any(|cmd| cmd.name == *name);
            assert!(
                found,
                "Buffer finder action '{name}' should be in command list"
            );
        }

        let open_type_id = TypeId::of::<crate::actions::OpenBufferFinder>();
        let open_cmd = commands.iter().find(|cmd| cmd.type_id == open_type_id);

        assert!(
            open_cmd.is_some(),
            "OpenBufferFinder should be in command list"
        );

        let cmd = open_cmd.unwrap();
        assert!(
            cmd.description.contains("buffer finder"),
            "Description should come from Action::documentation(). Got: {:?}",
            cmd.description
        );
    }

    #[test]
    fn build_command_list_includes_command_palette_actions() {
        let commands = build_command_list();
        let names = [
            "OpenCommandPalette",
            "CommandPaletteNext",
            "CommandPalettePrev",
            "CommandPaletteExecute",
            "CommandPaletteDismiss",
            "ToggleCommandPaletteHidden",
        ];

        for name in &names {
            assert!(
                commands.iter().any(|cmd| cmd.name == *name),
                "Command palette action '{name}' should be in list"
            );
        }
    }

    #[test]
    fn build_command_list_includes_pane_actions() {
        let commands = build_command_list();
        let names = [
            "SplitUp",
            "SplitDown",
            "SplitLeft",
            "SplitRight",
            "Quit",
            "FocusPaneUp",
            "FocusPaneDown",
            "FocusPaneLeft",
            "FocusPaneRight",
        ];

        for name in &names {
            assert!(
                commands.iter().any(|cmd| cmd.name == *name),
                "Pane action '{name}' should be in list"
            );
        }
    }

    #[test]
    fn build_command_list_includes_application_actions() {
        let commands = build_command_list();
        let names = ["QuitAll", "WriteFile", "WriteAll"];

        for name in &names {
            assert!(
                commands.iter().any(|cmd| cmd.name == *name),
                "Application action '{name}' should be in list"
            );
        }
    }

    #[test]
    fn build_command_list_includes_view_actions() {
        let commands = build_command_list();
        let names = ["ToggleMinimap", "ShowMinimapOnScroll"];

        for name in &names {
            assert!(
                commands.iter().any(|cmd| cmd.name == *name),
                "View action '{name}' should be in list"
            );
        }
    }

    #[test]
    fn build_command_list_includes_help_actions() {
        let commands = build_command_list();
        let names = [
            "OpenHelpOverlay",
            "OpenHelpModal",
            "HelpModalDismiss",
            "OpenAboutModal",
            "AboutModalDismiss",
        ];

        for name in &names {
            assert!(
                commands.iter().any(|cmd| cmd.name == *name),
                "Help action '{name}' should be in list"
            );
        }
    }

    #[test]
    fn build_command_list_includes_git_status_actions() {
        let commands = build_command_list();
        let names = [
            "OpenGitStatus",
            "GitStatusNext",
            "GitStatusPrev",
            "GitStatusSelect",
            "GitStatusDismiss",
            "GitStatusCycleFilter",
            "GitStatusSetFilterAll",
            "GitStatusSetFilterStaged",
            "GitStatusSetFilterUnstaged",
            "GitStatusSetFilterUnstagedWithUntracked",
            "GitStatusSetFilterUntracked",
        ];

        for name in &names {
            assert!(
                commands.iter().any(|cmd| cmd.name == *name),
                "Git status action '{name}' should be in list"
            );
        }
    }

    #[test]
    fn build_command_list_includes_git_diff_hunk_actions() {
        let commands = build_command_list();
        let names = ["ToggleDiffHunk", "GotoNextHunk", "GotoPrevHunk"];

        for name in &names {
            assert!(
                commands.iter().any(|cmd| cmd.name == *name),
                "Git diff hunk action '{name}' should be in list"
            );
        }
    }

    #[test]
    fn build_command_list_includes_diff_review_actions() {
        let commands = build_command_list();
        let names = [
            "OpenDiffReview",
            "DiffReviewNextHunk",
            "DiffReviewPrevHunk",
            "DiffReviewApproveHunk",
            "DiffReviewToggleApproval",
            "DiffReviewNextUnreviewedHunk",
            "DiffReviewResetProgress",
            "DiffReviewDismiss",
            "DiffReviewCycleComparisonMode",
            "DiffReviewRevertHunk",
            "DiffReviewToggleFollow",
        ];

        for name in &names {
            assert!(
                commands.iter().any(|cmd| cmd.name == *name),
                "Diff review action '{name}' should be in list"
            );
        }
    }

    #[test]
    fn build_command_list_includes_git_repository_actions() {
        let commands = build_command_list();
        let names = [
            "GitStageFile",
            "GitStageAll",
            "GitUnstageFile",
            "GitUnstageAll",
            "GitStageHunk",
            "GitUnstageHunk",
            "GitToggleStageHunk",
        ];

        for name in &names {
            assert!(
                commands.iter().any(|cmd| cmd.name == *name),
                "Git repository action '{name}' should be in list"
            );
        }
    }
}
