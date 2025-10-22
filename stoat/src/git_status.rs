//! Git status tracking for modified files with UI rendering.
//!
//! This module provides data structures, functions for gathering git repository status
//! information, and rendering components for the git status modal overlay. Following Zed's
//! pattern where a feature combines state and UI in one module.
//!
//! # Architecture
//!
//! The status system has two main components:
//!
//! 1. [`GitStatusEntry`] - Represents a single file with its git status
//! 2. [`gather_git_status`] - Function that queries git and builds the entry list
//! 3. [`GitStatus`] - Rendering component for the modal UI
//!
//! ## How Status Works
//!
//! Status is gathered by discovering the git repository, then calling `statuses()`:
//!
//! ```ignore
//! let repo = Repository::discover(current_path)?;
//! let entries = gather_git_status(&repo)?;
//! ```
//!
//! # Status Types
//!
//! Status entries track both index (staged) and working tree changes:
//! - **Modified** (M) - File has changes
//! - **Added** (A) - New file
//! - **Deleted** (D) - File removed
//! - **Renamed** (R) - File renamed
//! - **Conflicted** (!) - Merge conflict
//! - **Untracked** (??) - Not tracked by git
//!
//! # Usage
//!
//! ```ignore
//! use crate::git_status::{GitStatusEntry, gather_git_status};
//! use crate::git_repository::Repository;
//!
//! let repo = Repository::discover(Path::new("."))?;
//! let entries = gather_git_status(repo.inner())?;
//!
//! for entry in &entries {
//!     println!("{} {}", entry.status_display(), entry.path.display());
//! }
//! ```
//!
//! # Related
//!
//! - [`git_repository::Repository`](crate::git_repository::Repository) - Git repository wrapper
//! - [`Stoat`](crate::Stoat) - Uses this for git status modal state

use gpui::{
    div, point, prelude::FluentBuilder, px, rgb, rgba, App, Bounds, Element, Font, FontStyle,
    FontWeight, GlobalElementId, InspectorElementId, InteractiveElement, IntoElement, LayoutId,
    PaintQuad, ParentElement, Pixels, RenderOnce, ScrollHandle, ShapedLine, SharedString,
    StatefulInteractiveElement, Style, Styled, TextRun, Window,
};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors that can occur during git status gathering.
#[derive(Debug, Error)]
pub enum GitStatusError {
    /// Git operation failed
    #[error("Git status error: {0}")]
    GitError(String),
}

/// Filter mode for git status display.
///
/// Determines which files are shown in the git status modal. Filters can be applied
/// to focus on specific types of changes (staged, unstaged, untracked).
///
/// # Variants
///
/// - [`All`](Self::All) - Show all modified files (default)
/// - [`Staged`](Self::Staged) - Show only staged changes (in index)
/// - [`Unstaged`](Self::Unstaged) - Show unstaged changes (modified working tree, excludes
///   untracked)
/// - [`UnstagedWithUntracked`](Self::UnstagedWithUntracked) - Show unstaged + untracked files
/// - [`Untracked`](Self::Untracked) - Show only untracked files
///
/// # Usage
///
/// Used by [`Stoat`](crate::Stoat) to filter the git status file list based on user selection.
/// The filter is applied when opening git status or when cycling through filter modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitStatusFilter {
    /// Show all files (staged, unstaged, untracked)
    All,
    /// Show only staged changes (in index)
    Staged,
    /// Show only unstaged changes (modified working tree, excludes untracked)
    Unstaged,
    /// Show unstaged changes and untracked files
    UnstagedWithUntracked,
    /// Show only untracked files
    Untracked,
}

impl GitStatusFilter {
    /// Get display name for the filter.
    ///
    /// Returns a human-readable string for showing in the UI.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Staged => "Staged",
            Self::Unstaged => "Unstaged",
            Self::UnstagedWithUntracked => "Unstaged + Untracked",
            Self::Untracked => "Untracked",
        }
    }

    /// Cycle to the next filter mode.
    ///
    /// Rotates through filter modes in order: All, Staged, Unstaged,
    /// UnstagedWithUntracked, Untracked, and back to All.
    pub fn next(&self) -> Self {
        match self {
            Self::All => Self::Staged,
            Self::Staged => Self::Unstaged,
            Self::Unstaged => Self::UnstagedWithUntracked,
            Self::UnstagedWithUntracked => Self::Untracked,
            Self::Untracked => Self::All,
        }
    }

    /// Check if an entry matches this filter.
    ///
    /// Determines whether a [`GitStatusEntry`] should be included based on the filter mode.
    ///
    /// # Arguments
    ///
    /// * `entry` - The status entry to check
    ///
    /// # Returns
    ///
    /// `true` if the entry matches the filter criteria
    pub fn matches(&self, entry: &GitStatusEntry) -> bool {
        match self {
            Self::All => true,
            Self::Staged => entry.staged,
            Self::Unstaged => !entry.staged && entry.status != "??",
            Self::UnstagedWithUntracked => !entry.staged,
            Self::Untracked => entry.status == "??",
        }
    }
}

impl Default for GitStatusFilter {
    fn default() -> Self {
        Self::All
    }
}

/// A git status entry for a single file.
///
/// Represents the status of one file in the working tree and/or index. Used by the
/// git status modal to display modified files for quick review.
///
/// # Status Representation
///
/// Status is simplified to a single character for display:
/// - `M` - Modified (in index or working tree)
/// - `A` - Added (new file in index)
/// - `D` - Deleted (removed from index or working tree)
/// - `R` - Renamed (file renamed in index)
/// - `!` - Conflicted (merge conflict)
/// - `??` - Untracked (not tracked by git)
///
/// # Staging
///
/// The `staged` flag indicates whether changes are in the index (staged for commit).
/// This allows different visual styling in the UI.
#[derive(Clone, Debug)]
pub struct GitStatusEntry {
    /// Path to the file, relative to repository root
    pub path: PathBuf,
    /// Status string for display ("M", "A", "D", "R", "!", "??")
    pub status: String,
    /// Whether changes are staged in index
    pub staged: bool,
}

impl GitStatusEntry {
    /// Create a new git status entry.
    pub fn new(path: PathBuf, status: String, staged: bool) -> Self {
        Self {
            path,
            status,
            staged,
        }
    }

    /// Get display string for status with staging indicator.
    ///
    /// Returns a two-character status like "M " (modified, staged) or
    /// " M" (modified, unstaged).
    pub fn status_display(&self) -> String {
        if self.staged {
            format!("{} ", self.status)
        } else {
            format!(" {}", self.status)
        }
    }
}

/// Gather git status entries from a repository.
///
/// Queries the git repository for file statuses and returns a list of entries
/// for files that have changes. Ignores clean files and sorts results by path.
///
/// # Arguments
///
/// * `repo` - Git repository to query
///
/// # Returns
///
/// Vector of status entries for changed files, sorted by path
///
/// # Status Priorities
///
/// When a file has both index and working tree changes, index status takes priority
/// for the display character. The `staged` flag indicates index changes.
///
/// # Errors
///
/// Returns error if git status query fails.
pub fn gather_git_status(repo: &git2::Repository) -> Result<Vec<GitStatusEntry>, GitStatusError> {
    let mut entries = Vec::new();

    let statuses = repo
        .statuses(None)
        .map_err(|e| GitStatusError::GitError(e.message().to_string()))?;

    for entry in statuses.iter() {
        let status = entry.status();
        let path = entry
            .path()
            .ok_or_else(|| GitStatusError::GitError("Invalid UTF-8 path".to_string()))?;

        let path_buf = PathBuf::from(path);

        // Check for staged changes
        if status.is_index_new() {
            entries.push(GitStatusEntry::new(path_buf.clone(), "A".to_string(), true));
        } else if status.is_index_modified() {
            entries.push(GitStatusEntry::new(path_buf.clone(), "M".to_string(), true));
        } else if status.is_index_deleted() {
            entries.push(GitStatusEntry::new(path_buf.clone(), "D".to_string(), true));
        } else if status.is_index_renamed() {
            entries.push(GitStatusEntry::new(path_buf.clone(), "R".to_string(), true));
        }

        // Check for unstaged changes (can happen in addition to staged changes)
        if status.is_wt_new() {
            entries.push(GitStatusEntry::new(
                path_buf.clone(),
                "??".to_string(),
                false,
            ));
        } else if status.is_wt_modified() {
            entries.push(GitStatusEntry::new(
                path_buf.clone(),
                "M".to_string(),
                false,
            ));
        } else if status.is_wt_deleted() {
            entries.push(GitStatusEntry::new(
                path_buf.clone(),
                "D".to_string(),
                false,
            ));
        } else if status.is_wt_renamed() {
            entries.push(GitStatusEntry::new(
                path_buf.clone(),
                "R".to_string(),
                false,
            ));
        } else if status.is_conflicted() {
            entries.push(GitStatusEntry::new(path_buf, "!".to_string(), false));
        }
    }

    // Sort by path for consistent display
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(entries)
}

/// Git diff preview data for the status modal.
///
/// Contains the diff patch text for a file, showing what has changed.
/// Similar to [`crate::file_finder::PreviewData`] but for git diffs instead of file content.
#[derive(Clone)]
pub struct DiffPreviewData {
    /// The diff patch text in unified diff format
    pub text: String,
}

impl DiffPreviewData {
    /// Create a new diff preview with the given patch text.
    pub fn new(text: String) -> Self {
        Self { text }
    }

    /// Get the diff text.
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Git branch information for the status modal.
///
/// Contains the current branch name and tracking information (ahead/behind upstream).
/// Used by the git status modal to display branch context alongside file changes.
#[derive(Clone, Debug)]
pub struct GitBranchInfo {
    /// Name of the current branch
    pub branch_name: String,
    /// Number of commits ahead of upstream
    pub ahead: u32,
    /// Number of commits behind upstream
    pub behind: u32,
}

impl GitBranchInfo {
    /// Create new branch info.
    pub fn new(branch_name: String, ahead: u32, behind: u32) -> Self {
        Self {
            branch_name,
            ahead,
            behind,
        }
    }
}

/// Gather git branch information from a repository.
///
/// Queries the current branch name and upstream tracking status (ahead/behind).
/// Returns [`None`] if the repository is in detached HEAD state or if branch
/// information cannot be determined.
///
/// # Arguments
///
/// * `repo` - Git repository to query
///
/// # Returns
///
/// [`Some(GitBranchInfo)`] if on a branch with tracking info, [`None`] otherwise.
pub fn gather_git_branch_info(repo: &git2::Repository) -> Option<GitBranchInfo> {
    let head = repo.head().ok()?;

    if !head.is_branch() {
        return None;
    }

    let branch_name = head.shorthand()?.to_string();

    let (ahead, behind) = if let Some(local_oid) = head.target() {
        let branch = repo
            .find_branch(&branch_name, git2::BranchType::Local)
            .ok()?;

        if let Ok(upstream) = branch.upstream() {
            if let Some(upstream_oid) = upstream.get().target() {
                repo.graph_ahead_behind(local_oid, upstream_oid)
                    .ok()
                    .map(|(a, b)| (a as u32, b as u32))
                    .unwrap_or((0, 0))
            } else {
                (0, 0)
            }
        } else {
            (0, 0)
        }
    } else {
        (0, 0)
    };

    Some(GitBranchInfo::new(branch_name, ahead, behind))
}

/// Load git diff preview for a file.
///
/// Computes the diff between HEAD and working tree for the specified file,
/// returning the patch in unified diff format. Both git operations and diff
/// computation run on thread pool via `smol::unblock` to avoid blocking executor.
///
/// # Arguments
///
/// * `repo_path` - Path to repository root (used to discover repository)
/// * `file_path` - Path to file relative to repository root
///
/// # Returns
///
/// Optional diff preview containing patch text, or None if diff computation fails.
pub async fn load_git_diff(repo_path: &Path, file_path: &Path) -> Option<DiffPreviewData> {
    let repo_path = repo_path.to_path_buf();
    let file_path = file_path.to_path_buf();

    smol::unblock(move || {
        // Open repository
        let repo = git2::Repository::open(&repo_path).ok()?;

        // Get HEAD tree
        let head = repo.head().ok()?;
        let head_tree = head.peel_to_tree().ok()?;

        // Get working tree diff
        let mut diff_options = git2::DiffOptions::new();
        diff_options.pathspec(&file_path);

        let diff = repo
            .diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut diff_options))
            .ok()?;

        // Convert diff to patch text
        let mut patch_text = String::new();
        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            let origin = line.origin();
            let content = std::str::from_utf8(line.content()).unwrap_or("");

            match origin {
                '+' | '-' | ' ' => {
                    patch_text.push(origin);
                    patch_text.push_str(content);
                },
                '>' | '<' => {
                    // File mode changes, context markers
                    patch_text.push_str(content);
                },
                'F' => {
                    // File header
                    patch_text.push_str("diff --git ");
                    patch_text.push_str(content);
                },
                'H' => {
                    // Hunk header
                    patch_text.push_str("@@ ");
                    patch_text.push_str(content);
                },
                _ => {
                    // Other lines (index, file names, etc)
                    patch_text.push_str(content);
                },
            }

            true // Continue iteration
        })
        .ok()?;

        if patch_text.is_empty() {
            None
        } else {
            Some(DiffPreviewData::new(patch_text))
        }
    })
    .await
}

/// Git status modal renderer.
///
/// Stateless component that renders git status UI similar to [`crate::file_finder::Finder`].
/// Two-panel layout: file list on left (45%), diff preview on right (55%).
#[derive(IntoElement)]
pub struct GitStatus {
    files: Vec<GitStatusEntry>,
    filtered: Vec<GitStatusEntry>,
    filter: GitStatusFilter,
    total_count: usize,
    selected: usize,
    preview: Option<DiffPreviewData>,
    branch_info: Option<GitBranchInfo>,
    scroll_handle: ScrollHandle,
}

impl GitStatus {
    /// Create a new git status renderer with the given state.
    pub fn new(
        files: Vec<GitStatusEntry>,
        filtered: Vec<GitStatusEntry>,
        filter: GitStatusFilter,
        total_count: usize,
        selected: usize,
        preview: Option<DiffPreviewData>,
        branch_info: Option<GitBranchInfo>,
        scroll_handle: ScrollHandle,
    ) -> Self {
        Self {
            files,
            filtered,
            filter,
            total_count,
            selected,
            preview,
            branch_info,
            scroll_handle,
        }
    }

    /// Render the header bar showing title and filter info.
    fn render_header(&self) -> impl IntoElement {
        let filter_text = if self.filter == GitStatusFilter::All {
            format!("Git Status - {} files", self.total_count)
        } else {
            format!(
                "Git Status - {} - {}/{} files",
                self.filter.display_name(),
                self.filtered.len(),
                self.total_count
            )
        };

        div()
            .p(px(8.0))
            .border_b_1()
            .border_color(rgb(0x3e3e42))
            .bg(rgb(0x252526))
            .text_color(rgb(0xd4d4d4))
            .font_weight(FontWeight::SEMIBOLD)
            .child(filter_text)
    }

    /// Render branch information section (git-style formatting).
    fn render_branch_info(&self) -> Option<impl IntoElement> {
        let branch_info = self.branch_info.as_ref()?;

        let mut lines = vec![format!("On branch {}", branch_info.branch_name)];

        if branch_info.ahead > 0 && branch_info.behind > 0 {
            lines.push(format!(
                "Your branch is ahead by {} and behind by {} commits.",
                branch_info.ahead, branch_info.behind
            ));
        } else if branch_info.ahead > 0 {
            lines.push(format!(
                "Your branch is ahead of 'origin/{}' by {} commit{}.",
                branch_info.branch_name,
                branch_info.ahead,
                if branch_info.ahead == 1 { "" } else { "s" }
            ));
        } else if branch_info.behind > 0 {
            lines.push(format!(
                "Your branch is behind 'origin/{}' by {} commit{}.",
                branch_info.branch_name,
                branch_info.behind,
                if branch_info.behind == 1 { "" } else { "s" }
            ));
        }

        Some(
            div()
                .p(px(12.0))
                .border_b_1()
                .border_color(rgb(0x3e3e42))
                .bg(rgb(0x1e1e1e))
                .text_color(rgb(0x808080))
                .text_size(px(12.0))
                .flex()
                .flex_col()
                .gap_1()
                .children(lines.into_iter().map(|line| div().child(line))),
        )
    }

    /// Render the list of modified files with status indicators.
    fn render_file_list(&self) -> impl IntoElement {
        let files = &self.filtered;
        let selected = self.selected;

        if files.is_empty() {
            let empty_message = if self.filter == GitStatusFilter::All {
                "nothing to commit, working tree clean"
            } else {
                "no files match current filter"
            };

            return div()
                .id("git-status-list")
                .flex()
                .flex_col()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(rgb(0x808080))
                        .text_size(px(13.0))
                        .child(empty_message),
                );
        }

        div()
            .id("git-status-list")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .children(files.iter().enumerate().map(|(i, entry)| {
                let status_color = match entry.status.as_str() {
                    "M" => rgb(0x4ec9b0),  // Teal for modified
                    "A" => rgb(0x6a9955),  // Green for added
                    "D" => rgb(0xf14c4c),  // Red for deleted
                    "R" => rgb(0xc586c0),  // Purple for renamed
                    "!" => rgb(0xf48771),  // Orange for conflicted
                    "??" => rgb(0x808080), // Gray for untracked
                    _ => rgb(0xd4d4d4),    // White for unknown
                };

                div()
                    .flex()
                    .gap_2()
                    .px(px(8.0))
                    .py(px(3.0))
                    .when(i == selected, |div| {
                        div.bg(rgb(0x3b4261)) // Blue-gray highlight for selected file
                    })
                    .child(
                        div()
                            .text_color(status_color)
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .w(px(16.0))
                            .child(entry.status_display()),
                    )
                    .child(
                        div()
                            .text_color(rgb(0xd4d4d4))
                            .text_size(px(11.0))
                            .child(entry.path.to_string_lossy().to_string()),
                    )
            }))
    }

    /// Render the diff preview panel.
    fn render_preview(&self) -> DiffPreviewElement {
        DiffPreviewElement::new(self.preview.clone())
    }
}

/// Custom element for rendering git diff preview with colored lines.
///
/// Implements GPUI's low-level Element trait to render diff text with proper coloring:
/// - Lines starting with '+' in green
/// - Lines starting with '-' in red
/// - Hunk headers (@@) in gray
/// - Context lines in default color
struct DiffPreviewElement {
    preview: Option<DiffPreviewData>,
}

struct DiffPreviewLayout {
    lines: Vec<ShapedLineWithPosition>,
    bounds: Bounds<Pixels>,
}

struct ShapedLineWithPosition {
    shaped: ShapedLine,
    position: gpui::Point<Pixels>,
}

impl DiffPreviewElement {
    fn new(preview: Option<DiffPreviewData>) -> Self {
        Self { preview }
    }
}

impl Element for DiffPreviewElement {
    type RequestLayoutState = ();
    type PrepaintState = DiffPreviewLayout;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Request full-size layout
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = gpui::relative(1.).into();
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let Some(preview) = &self.preview else {
            return DiffPreviewLayout {
                lines: Vec::new(),
                bounds,
            };
        };

        // Font configuration
        let font = Font {
            family: ".AppleSystemUIFontMonospaced".into(),
            features: Default::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: None,
        };
        let font_size = px(12.0);
        let line_height = px(18.0);

        // Calculate viewport culling
        let visible_height = f32::from(bounds.size.height);
        let max_visible_lines = (visible_height / f32::from(line_height)).ceil() as usize + 2;

        // Diff colors
        let color_added = rgb(0x6a9955); // Green for +
        let color_removed = rgb(0xf14c4c); // Red for -
        let color_hunk = rgb(0x808080); // Gray for @@
        let color_default = rgb(0xd4d4d4); // White for context

        // Shape each line with appropriate color
        let mut lines = Vec::new();
        let mut y_offset = bounds.origin.y + px(12.0);

        for (line_idx, line_text) in preview.text().lines().enumerate() {
            if line_idx >= max_visible_lines {
                break;
            }

            // Determine color based on line prefix
            let color = if line_text.starts_with('+') {
                color_added
            } else if line_text.starts_with('-') {
                color_removed
            } else if line_text.starts_with("@@") {
                color_hunk
            } else {
                color_default
            };

            let text = if line_text.is_empty() {
                SharedString::from(" ")
            } else {
                SharedString::from(line_text.to_string())
            };

            let text_run = TextRun {
                len: text.len(),
                font: font.clone(),
                color: color.into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };

            let shaped = window
                .text_system()
                .shape_line(text, font_size, &[text_run], None);

            lines.push(ShapedLineWithPosition {
                shaped,
                position: point(bounds.origin.x + px(12.0), y_offset),
            });

            y_offset += line_height;
        }

        DiffPreviewLayout { lines, bounds }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        layout: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Paint background
        window.paint_quad(PaintQuad {
            bounds: layout.bounds,
            corner_radii: Default::default(),
            background: rgb(0x1e1e1e).into(),
            border_color: Default::default(),
            border_widths: Default::default(),
            border_style: Default::default(),
        });

        // Paint each shaped line
        let line_height = px(18.0);
        for line in &layout.lines {
            line.shaped
                .paint(line.position, line_height, window, cx)
                .unwrap_or_else(|err| {
                    eprintln!("Failed to paint diff line: {err:?}");
                });
        }
    }
}

impl IntoElement for DiffPreviewElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl RenderOnce for GitStatus {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let viewport_width = f32::from(window.viewport_size().width);
        let viewport_height = f32::from(window.viewport_size().height);
        let show_preview = viewport_width > 1000.0 && self.preview.is_some();
        let is_clean = self.files.is_empty();

        let branch_info_elem = self.render_branch_info();

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .bg(rgba(0x00000030)) // Dimmed background overlay
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .when(is_clean, |div| div.w(px(500.0)).h(px(200.0)))
                    .when(!is_clean, |div| div.w_3_4().h(px(viewport_height * 0.85)))
                    .bg(rgb(0x1e1e1e))
                    .border_1()
                    .border_color(rgb(0x3e3e42))
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .child(self.render_header())
                    .when_some(branch_info_elem, |div, elem| div.child(elem))
                    .child(if show_preview {
                        // Two-panel layout: file list on left, preview on right
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                // Left panel: file list (45%)
                                div()
                                    .flex()
                                    .flex_col()
                                    .w(px(viewport_width * 0.75 * 0.45))
                                    .border_r_1()
                                    .border_color(rgb(0x3e3e42))
                                    .child(self.render_file_list()),
                            )
                            .child(
                                // Right panel: diff preview (55%)
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .child(self.render_preview()),
                            )
                    } else {
                        // Single panel: just file list
                        div().flex().flex_row().flex_1().overflow_hidden().child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .child(self.render_file_list()),
                        )
                    }),
            )
    }
}
