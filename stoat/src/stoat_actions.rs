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
        let hunk_start = hunk.buffer_range.start.to_point(&buffer_snapshot);
        let hunk_end = hunk.buffer_range.end.to_point(&buffer_snapshot);

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
    /// Uses pre-computed indices from GitIndex for O(1) navigation to the next file with hunks.
    /// Wraps to first file if at the end.
    pub fn load_next_file(&mut self, cx: &mut Context<Self>) {
        if self.review_state.files.is_empty() {
            return;
        }

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git::repository::Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => return,
        };

        let file_count = self.review_state.files.len();
        let current_idx = self.review_state.file_idx;

        // Loop through files starting from next one, looking for one with hunks
        for offset in 1..=file_count {
            let next_idx = (current_idx + offset) % file_count;
            let file_path = &self.review_state.files[next_idx];
            let abs_path = repo.workdir().join(file_path);

            // Load file
            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {}", abs_path, e);
                continue;
            }

            // For IndexVsHead/HeadVsParent, replace buffer content so anchors resolve correctly
            match self.review_comparison_mode() {
                crate::git::diff_review::DiffComparisonMode::IndexVsHead => {
                    match repo.index_content(&abs_path) {
                        Ok(content) => {
                            let buffer_item = self.active_buffer(cx);
                            self.replace_buffer_content(&content, &buffer_item, cx);
                        },
                        Err(e) => {
                            tracing::warn!("Failed to read index content for {abs_path:?}: {e}")
                        },
                    }
                },
                crate::git::diff_review::DiffComparisonMode::HeadVsParent => {
                    match repo.head_content(&abs_path) {
                        Ok(content) => {
                            let buffer_item = self.active_buffer(cx);
                            self.replace_buffer_content(&content, &buffer_item, cx);
                        },
                        Err(e) => {
                            tracing::warn!("Failed to read head content for {abs_path:?}: {e}")
                        },
                    }
                },
                _ => {},
            }

            // Compute diff and check if it has hunks
            if let Some((diff, staged_rows, staged_hunk_indices)) =
                self.compute_diff_for_review_mode(&abs_path, cx)
            {
                if !diff.hunks.is_empty() {
                    let buffer_item = self.active_buffer(cx);
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

                    self.review_state.file_idx = next_idx;
                    self.review_state.hunk_idx = 0;
                    self.jump_to_current_hunk(true, cx);
                    return;
                }
            }
        }

        debug!("No more files with hunks in current comparison mode");

        // Clear old diff and reset cursor when no files have hunks
        let buffer_item = self.active_buffer(cx);
        buffer_item.update(cx, |item, _| {
            item.set_diff(None);
        });
        self.cursor.move_to(text::Point::new(0, 0));
    }

    /// Load previous file in diff review.
    ///
    /// Uses pre-computed indices from GitIndex for O(1) navigation to the previous file with hunks.
    /// Wraps to last file if at the beginning.
    pub fn load_prev_file(&mut self, cx: &mut Context<Self>) {
        if self.review_state.files.is_empty() {
            return;
        }

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git::repository::Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => return,
        };

        let file_count = self.review_state.files.len();
        let current_idx = self.review_state.file_idx;

        // Loop through files backwards starting from previous one, looking for one with hunks
        for offset in 1..=file_count {
            let prev_idx = if current_idx >= offset {
                current_idx - offset
            } else {
                file_count - (offset - current_idx)
            };

            let file_path = &self.review_state.files[prev_idx];
            let abs_path = repo.workdir().join(file_path);

            // Load file
            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {}", abs_path, e);
                continue;
            }

            // For IndexVsHead/HeadVsParent, replace buffer content so anchors resolve correctly
            match self.review_comparison_mode() {
                crate::git::diff_review::DiffComparisonMode::IndexVsHead => {
                    match repo.index_content(&abs_path) {
                        Ok(content) => {
                            let buffer_item = self.active_buffer(cx);
                            self.replace_buffer_content(&content, &buffer_item, cx);
                        },
                        Err(e) => {
                            tracing::warn!("Failed to read index content for {abs_path:?}: {e}")
                        },
                    }
                },
                crate::git::diff_review::DiffComparisonMode::HeadVsParent => {
                    match repo.head_content(&abs_path) {
                        Ok(content) => {
                            let buffer_item = self.active_buffer(cx);
                            self.replace_buffer_content(&content, &buffer_item, cx);
                        },
                        Err(e) => {
                            tracing::warn!("Failed to read head content for {abs_path:?}: {e}")
                        },
                    }
                },
                _ => {},
            }

            // Compute diff and check if it has hunks
            if let Some((diff, staged_rows, staged_hunk_indices)) =
                self.compute_diff_for_review_mode(&abs_path, cx)
            {
                if !diff.hunks.is_empty() {
                    let buffer_item = self.active_buffer(cx);
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
                    self.review_state.file_idx = prev_idx;
                    self.review_state.hunk_idx = last_hunk_idx;
                    self.jump_to_current_hunk(true, cx);
                    return;
                }
            }
        }

        debug!("No more files with hunks in current comparison mode");

        // Clear old diff and reset cursor when no files have hunks
        let buffer_item = self.active_buffer(cx);
        buffer_item.update(cx, |item, _| {
            item.set_diff(None);
        });
        self.cursor.move_to(text::Point::new(0, 0));
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
            "DiffReviewPreviousCommit",
            "DiffReviewRevertHunk",
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
