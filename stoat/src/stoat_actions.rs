//! Action implementations for Stoat.
//!
//! These demonstrate the Context<Self> pattern - methods can spawn self-updating tasks.

use crate::{
    file_finder::{load_file_preview, load_text_only, PreviewData},
    stoat::Stoat,
};
use gpui::Context;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use std::path::PathBuf;
use text::{Buffer, ToPoint};
use tracing::debug;

impl Stoat {
    // ==== File finder helper methods (not actions) ====

    /// Load preview for selected file.
    ///
    /// KEY METHOD: Demonstrates Context<Self> pattern with async tasks.
    /// Uses `cx.spawn` to get `WeakEntity<Self>` for self-updating.
    pub fn load_preview_for_selected(&mut self, cx: &mut Context<Self>) {
        // Cancel existing task
        self.file_finder_preview_task = None;

        // Get selected file path
        let relative_path = match self.file_finder_filtered.get(self.file_finder_selected) {
            Some(path) => path.clone(),
            None => {
                self.file_finder_preview = None;
                return;
            },
        };

        // Build absolute path
        let root = self.worktree.lock().snapshot().root().to_path_buf();
        let abs_path = root.join(&relative_path);
        let abs_path_for_highlight = abs_path.clone();

        // Spawn async task with WeakEntity<Self> handle
        // This is the key pattern: cx.spawn gives us self handle!
        self.file_finder_preview_task = Some(cx.spawn(async move |this, cx| {
            // Phase 1: Load plain text immediately
            if let Some(text) = load_text_only(&abs_path).await {
                // Update self through entity handle
                let _ = this.update(cx, |stoat, cx| {
                    stoat.file_finder_preview = Some(PreviewData::Plain(text));
                    cx.notify();
                });
            }

            // Phase 2: Load syntax-highlighted version
            if let Some(highlighted) = load_file_preview(&abs_path_for_highlight).await {
                let _ = this.update(cx, |stoat, cx| {
                    stoat.file_finder_preview = Some(highlighted);
                    cx.notify();
                });
            }
        }));
    }

    /// Filter files based on query
    pub fn filter_files(&mut self, query: &str, cx: &mut Context<Self>) {
        if query.is_empty() {
            // No query: show all files
            self.file_finder_filtered = self
                .file_finder_files
                .iter()
                .map(|e| PathBuf::from(e.path.as_unix_str()))
                .collect();
        } else {
            // Fuzzy match
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);

            let candidates: Vec<&str> = self
                .file_finder_files
                .iter()
                .map(|e| e.path.as_unix_str())
                .collect();

            let mut matches = pattern.match_list(candidates, &mut self.file_finder_matcher);
            matches.sort_by(|a, b| b.1.cmp(&a.1));
            matches.truncate(100);

            self.file_finder_filtered = matches
                .into_iter()
                .map(|(path, _score)| PathBuf::from(path))
                .collect();
        }

        // Reset selection
        self.file_finder_selected = 0;

        // Load preview for newly selected (top) file
        self.load_preview_for_selected(cx);

        cx.notify();
    }

    // ==== File finder state accessors ====

    /// Get file finder input buffer
    pub fn file_finder_input(&self) -> Option<&gpui::Entity<Buffer>> {
        self.file_finder_input.as_ref()
    }

    /// Get filtered files
    pub fn file_finder_filtered(&self) -> &[PathBuf] {
        &self.file_finder_filtered
    }

    /// Get selected index
    pub fn file_finder_selected(&self) -> usize {
        self.file_finder_selected
    }

    /// Get preview data
    pub fn file_finder_preview(&self) -> Option<&PreviewData> {
        self.file_finder_preview.as_ref()
    }

    // ==== File navigation actions ====

    /// Move cursor to the start of the file.
    ///
    /// Positions the cursor at the very beginning of the buffer (row 0, column 0),
    /// regardless of current position.
    ///
    /// # Behavior
    ///
    /// - Moves cursor to (0, 0)
    /// - Resets goal column for vertical movement
    /// - Works from any position in the buffer
    /// - Triggers scroll animation to make cursor visible
    ///
    /// # Related
    ///
    /// See also [`Self::move_to_file_end`] for end-of-file movement.

    // ==== Command palette helper methods (not actions) ====

    /// Filter commands based on fuzzy search query.
    ///
    /// Uses nucleo fuzzy matching to filter the command list based on the query string.
    /// Searches both command name and description for matches.
    ///
    /// # Arguments
    ///
    /// * `query` - The search query string
    ///
    /// # Behavior
    ///
    /// - If query is empty, shows all commands
    /// - Otherwise, fuzzy matches against "name description" for each command
    /// - Sorts results by match score (best matches first)
    /// - Limits to top 50 results
    /// - Resets selection to first item
    pub fn filter_commands(&mut self, query: &str) {
        tracing::info!("filter_commands called with query: '{}'", query);
        if query.is_empty() {
            // No query: show all commands
            self.command_palette_filtered = self.command_palette_commands.clone();
        } else {
            // Parse pattern for smart fuzzy matching
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);

            // Create a temporary matcher for commands (uses default config, not path-specific)
            let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);

            // Build indexed search strings, including aliases
            let indexed_strings: Vec<(usize, String)> = self
                .command_palette_commands
                .iter()
                .enumerate()
                .map(|(idx, cmd)| {
                    let mut search_text = format!("{} {}", cmd.name, cmd.description);
                    // Include aliases in search text
                    for alias in &cmd.aliases {
                        search_text.push(' ');
                        search_text.push_str(alias);
                    }
                    (idx, search_text)
                })
                .collect();

            // Match and score all candidates, checking for exact alias matches
            let mut scored_commands: Vec<(usize, u32)> = indexed_strings
                .iter()
                .filter_map(|(idx, search_text)| {
                    let cmd = &self.command_palette_commands[*idx];

                    // Check for exact alias match (case-insensitive)
                    let query_lower = query.to_lowercase();
                    let has_exact_alias_match = cmd
                        .aliases
                        .iter()
                        .any(|alias| alias.to_lowercase() == query_lower);

                    if has_exact_alias_match {
                        // Perfect match - use maximum score to ensure it appears first
                        tracing::info!(
                            "Exact alias match for '{}': {} (aliases: {:?})",
                            query,
                            cmd.name,
                            cmd.aliases
                        );
                        Some((*idx, u32::MAX))
                    } else {
                        // Regular fuzzy matching
                        let candidates = vec![search_text.as_str()];
                        let matches = pattern.match_list(&candidates, &mut matcher);
                        let result = matches.first().map(|(_, score)| (*idx, *score));
                        if result.is_some() && query == ":q" {
                            tracing::info!(
                                "Fuzzy match for '{}': {} (score: {:?}, aliases: {:?})",
                                query,
                                cmd.name,
                                result.as_ref().map(|(_, s)| s),
                                cmd.aliases
                            );
                        }
                        result
                    }
                })
                .collect();

            // Sort by score (descending - higher score = better match)
            scored_commands.sort_by(|a, b| b.1.cmp(&a.1));

            // Limit to top 50 results
            scored_commands.truncate(50);

            // Convert back to CommandInfo
            self.command_palette_filtered = scored_commands
                .into_iter()
                .map(|(idx, _score)| self.command_palette_commands[idx].clone())
                .collect();
        }

        // Reset selection to first item
        self.command_palette_selected = 0;
    }

    // ==== Command palette state accessors ====

    /// Get the TypeId of the currently selected command.
    ///
    /// Returns the TypeId of the selected command's action for dispatch,
    /// or None if the command palette is not open or no command is selected.
    pub fn command_palette_selected_type_id(&self) -> Option<std::any::TypeId> {
        if self.mode() != "command_palette" {
            return None;
        }

        self.command_palette_filtered
            .get(self.command_palette_selected)
            .map(|cmd| cmd.type_id)
    }

    /// Accessor for command palette input buffer (for GUI layer).
    pub fn command_palette_input(&self) -> Option<&gpui::Entity<Buffer>> {
        self.command_palette_input.as_ref()
    }

    /// Accessor for filtered commands list (for GUI layer).
    pub fn command_palette_filtered(&self) -> &[crate::stoat::CommandInfo] {
        &self.command_palette_filtered
    }

    /// Accessor for selected command index (for GUI layer).
    pub fn command_palette_selected(&self) -> usize {
        self.command_palette_selected
    }

    // ==== Git status helper methods ====

    /// Load git diff preview for the currently selected file.
    ///
    /// Spawns an async task to load the diff patch for the selected file.
    /// The task updates the preview state when the diff is ready.
    pub fn load_git_diff_preview(&mut self, cx: &mut Context<Self>) {
        // Cancel existing task
        self.git_status_preview_task = None;

        // Get selected file entry from filtered list
        let entry = match self.git_status_filtered.get(self.git_status_selected) {
            Some(entry) => entry.clone(),
            None => {
                self.git_status_preview = None;
                return;
            },
        };

        // Get repository root path
        let root_path = self.worktree.lock().root().to_path_buf();
        let file_path = entry.path.clone();

        // Spawn async task to load diff
        self.git_status_preview_task = Some(cx.spawn(async move |this, cx| {
            // Load git diff
            if let Some(diff) = crate::git_status::load_git_diff(&root_path, &file_path).await {
                // Update self through entity handle
                let _ = this.update(cx, |stoat, cx| {
                    stoat.git_status_preview = Some(diff);
                    cx.notify();
                });
            }
        }));
    }

    /// Apply current filter to git status files.
    ///
    /// Filters the `git_status_files` list based on the current `git_status_filter` value
    /// and updates `git_status_filtered` with the results. Also resets selection to 0
    /// and loads preview for the first filtered file.
    ///
    /// This method is called:
    /// - When opening git status modal (with initial filter)
    /// - When cycling/changing the filter mode
    ///
    /// # Arguments
    ///
    /// * `cx` - GPUI context for spawning async tasks
    pub fn filter_git_status_files(&mut self, cx: &mut Context<Self>) {
        // Apply filter
        self.git_status_filtered = self
            .git_status_files
            .iter()
            .filter(|entry| self.git_status_filter.matches(entry))
            .cloned()
            .collect();

        // Reset selection to first item
        self.git_status_selected = 0;

        // Load preview for first filtered file
        self.load_git_diff_preview(cx);
    }

    /// Accessor for git status files (for GUI layer).
    pub fn git_status_files(&self) -> &[crate::git_status::GitStatusEntry] {
        &self.git_status_files
    }

    /// Accessor for filtered git status files (for GUI layer).
    ///
    /// Returns the list of files after the current filter has been applied.
    /// This is the list that should be displayed in the git status modal.
    pub fn git_status_filtered(&self) -> &[crate::git_status::GitStatusEntry] {
        &self.git_status_filtered
    }

    /// Accessor for current git status filter mode (for GUI layer).
    ///
    /// Returns the current filter mode being used to filter the git status files.
    pub fn git_status_filter(&self) -> crate::git_status::GitStatusFilter {
        self.git_status_filter
    }

    /// Accessor for git branch info (for GUI layer).
    pub fn git_status_branch_info(&self) -> Option<&crate::git_status::GitBranchInfo> {
        self.git_status_branch_info.as_ref()
    }

    /// Accessor for selected file index (for GUI layer).
    pub fn git_status_selected(&self) -> usize {
        self.git_status_selected
    }

    /// Accessor for git diff preview (for GUI layer).
    pub fn git_status_preview(&self) -> Option<&crate::git_status::DiffPreviewData> {
        self.git_status_preview.as_ref()
    }

    /// Accessor for git dirty count (number of modified files).
    pub fn git_dirty_count(&self) -> usize {
        self.git_dirty_count
    }

    /// Accessor for current file path (for status bar).
    pub fn current_file_path(&self) -> Option<&std::path::Path> {
        self.current_file_path.as_deref()
    }

    // ==== Buffer finder helper methods (not actions) ====

    /// Filter buffers based on query string.
    pub fn filter_buffers(&mut self, query: &str, cx: &mut Context<Self>) {
        if query.is_empty() {
            // No query: show all buffers
            self.buffer_finder_filtered = self.buffer_finder_buffers.clone();
        } else {
            // Fuzzy match on buffer display names
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);

            let candidates: Vec<&str> = self
                .buffer_finder_buffers
                .iter()
                .map(|entry| entry.display_name.as_str())
                .collect();

            let mut matches = pattern.match_list(candidates, &mut self.file_finder_matcher);
            matches.sort_by(|a, b| b.1.cmp(&a.1));

            self.buffer_finder_filtered = matches
                .into_iter()
                .map(|(display_name, _score)| {
                    // Find the original BufferListEntry by display_name
                    self.buffer_finder_buffers
                        .iter()
                        .find(|entry| entry.display_name == display_name)
                        .cloned()
                        .expect("Matched entry should exist in buffer_finder_buffers")
                })
                .collect();
        }

        // Reset selection
        self.buffer_finder_selected = 0;

        cx.notify();
    }

    // ==== Buffer finder state accessors ====

    /// Get buffer finder input buffer.
    pub fn buffer_finder_input(&self) -> Option<&gpui::Entity<Buffer>> {
        self.buffer_finder_input.as_ref()
    }

    /// Get filtered buffer list.
    pub fn buffer_finder_filtered(&self) -> &[crate::buffer_store::BufferListEntry] {
        &self.buffer_finder_filtered
    }

    /// Get selected buffer index.
    pub fn buffer_finder_selected(&self) -> usize {
        self.buffer_finder_selected
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
    pub fn jump_to_current_hunk(&mut self, cx: &mut Context<Self>) {
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

        if self.diff_review_current_hunk_idx >= diff.hunks.len() {
            return;
        }

        let hunk = &diff.hunks[self.diff_review_current_hunk_idx];

        // Convert hunk anchors to points
        let hunk_start = hunk.buffer_range.start.to_point(&buffer_snapshot);
        let hunk_end = hunk.buffer_range.end.to_point(&buffer_snapshot);

        let hunk_idx = self.diff_review_current_hunk_idx;
        let start_row = hunk_start.row;

        // Move cursor to hunk start
        self.cursor.move_to(hunk_start);

        // Smart scrolling based on hunk size
        if let Some(viewport_lines) = self.viewport_lines {
            let hunk_height = (hunk_end.row - hunk_start.row) as f32;

            // Only center small hunks (less than ~40% of viewport)
            // Larger hunks get positioned near top with padding
            let target_scroll_y = if hunk_height < viewport_lines * 0.4 {
                // Small hunk - center it
                let hunk_middle = hunk_start.row as f32 + (hunk_height / 2.0);
                (hunk_middle - (viewport_lines / 2.0)).max(0.0)
            } else {
                // Larger hunk - position near top with padding (like normal cursor)
                const TOP_PADDING: f32 = 3.0;
                (hunk_start.row as f32 - TOP_PADDING).max(0.0)
            };

            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        } else {
            // No viewport info - fall back to basic visibility check
            self.ensure_cursor_visible();
        }

        debug!(hunk = hunk_idx, line = start_row, "Jumped to hunk");
    }

    /// Load next file in diff review.
    ///
    /// Uses pre-computed indices from GitIndex for O(1) navigation to the next file with hunks.
    /// Wraps to first file if at the end.
    pub fn load_next_file(&mut self, cx: &mut Context<Self>) {
        if self.diff_review_files.is_empty() {
            return;
        }

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git_repository::Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => return,
        };

        let file_count = self.diff_review_files.len();
        let current_idx = self.diff_review_current_file_idx;

        // Loop through files starting from next one, looking for one with hunks
        for offset in 1..=file_count {
            let next_idx = (current_idx + offset) % file_count;
            let file_path = &self.diff_review_files[next_idx];
            let abs_path = repo.workdir().join(file_path);

            // Load file
            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {}", abs_path, e);
                continue;
            }

            // Compute diff and check if it has hunks
            if let Some(diff) = self.compute_diff_for_review_mode(&abs_path, cx) {
                if !diff.hunks.is_empty() {
                    // Found file with hunks - set it and jump to first hunk
                    let buffer_item = self.active_buffer(cx);
                    buffer_item.update(cx, |item, _| {
                        item.set_diff(Some(diff.clone()));
                    });

                    debug!(
                        "Loaded next file with {} hunks at idx={}",
                        diff.hunks.len(),
                        next_idx
                    );

                    self.diff_review_current_file_idx = next_idx;
                    self.diff_review_current_hunk_idx = 0;
                    self.jump_to_current_hunk(cx);
                    return;
                }
            }
        }

        debug!("No more files with hunks in current comparison mode");
    }

    /// Load previous file in diff review.
    ///
    /// Uses pre-computed indices from GitIndex for O(1) navigation to the previous file with hunks.
    /// Wraps to last file if at the beginning.
    pub fn load_prev_file(&mut self, cx: &mut Context<Self>) {
        if self.diff_review_files.is_empty() {
            return;
        }

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git_repository::Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => return,
        };

        let file_count = self.diff_review_files.len();
        let current_idx = self.diff_review_current_file_idx;

        // Loop through files backwards starting from previous one, looking for one with hunks
        for offset in 1..=file_count {
            let prev_idx = if current_idx >= offset {
                current_idx - offset
            } else {
                file_count - (offset - current_idx)
            };

            let file_path = &self.diff_review_files[prev_idx];
            let abs_path = repo.workdir().join(file_path);

            // Load file
            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {}", abs_path, e);
                continue;
            }

            // Compute diff and check if it has hunks
            if let Some(diff) = self.compute_diff_for_review_mode(&abs_path, cx) {
                if !diff.hunks.is_empty() {
                    // Found file with hunks - set it and jump to last hunk
                    let buffer_item = self.active_buffer(cx);
                    buffer_item.update(cx, |item, _| {
                        item.set_diff(Some(diff.clone()));
                    });

                    debug!(
                        "Loaded prev file with {} hunks at idx={}",
                        diff.hunks.len(),
                        prev_idx
                    );

                    // Jump to last hunk in previous file
                    let last_hunk_idx = diff.hunks.len().saturating_sub(1);
                    self.diff_review_current_file_idx = prev_idx;
                    self.diff_review_current_hunk_idx = last_hunk_idx;
                    self.jump_to_current_hunk(cx);
                    return;
                }
            }
        }

        debug!("No more files with hunks in current comparison mode");
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
        // Get description - skip if not available
        let Some(description) = crate::actions::DESCRIPTIONS.get(type_id) else {
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

        commands.push(crate::stoat::CommandInfo {
            name: name.to_string(),
            description: description.to_string(),
            aliases,
            type_id: *type_id,
        });
    }

    // Sort alphabetically by name
    commands.sort_by(|a, b| a.name.cmp(&b.name));

    commands
}
