//! Bottom status bar showing file path, git status, and mode.
//!
//! Displays essential context information in a 3-section layout:
//! - Active file path (left, with overflow truncation)
//! - Diff review info (center, only when in review mode)
//!   - Comparison mode: [WorkingVsHead], [WorkingVsIndex], or [IndexVsHead]
//!   - Patch position: Patch X/Y (current hunk across all files)
//!   - File progress: File X/Y
//! - Git branch name and dirty status (right)
//! - Current mode (right - NORMAL, INSERT, etc.)
//!
//! Renders as a small fixed-height bar at the bottom of the window.

use gpui::{div, px, rgb, IntoElement, ParentElement, RenderOnce, Styled};

/// Status bar component showing file path, git status, and mode.
///
/// Small single-line bar at bottom of window displaying:
/// - File path (left, truncated if needed)
/// - Diff review info (center, when in review mode)
/// - Git branch and status (right)
/// - Mode indicator (right, last)
#[derive(IntoElement)]
pub struct StatusBar {
    /// Mode display name (e.g., "NORMAL", "INSERT")
    mode_display: String,
    /// Git branch info (if in repo)
    branch_info: Option<crate::git_status::GitBranchInfo>,
    /// Git status entries for detailed status
    git_status_files: Vec<crate::git_status::GitStatusEntry>,
    /// Current file path for display
    file_path: Option<String>,
    /// File progress: (current_file, total_files)
    review_file_progress: Option<(usize, usize)>,
    /// Hunk position: (current_hunk, total_hunks) across all files
    hunk_position: Option<(usize, usize)>,
    /// Diff comparison mode (only shown in diff_review mode)
    comparison_mode: Option<crate::diff_review::DiffComparisonMode>,
}

impl StatusBar {
    /// Create a new status bar.
    pub fn new(
        mode_display: String,
        branch_info: Option<crate::git_status::GitBranchInfo>,
        git_status_files: Vec<crate::git_status::GitStatusEntry>,
        file_path: Option<String>,
        _review_progress: Option<(usize, usize)>,
        review_file_progress: Option<(usize, usize)>,
        hunk_position: Option<(usize, usize)>,
        comparison_mode: Option<crate::diff_review::DiffComparisonMode>,
    ) -> Self {
        Self {
            mode_display,
            branch_info,
            git_status_files,
            file_path,
            review_file_progress,
            hunk_position,
            comparison_mode,
        }
    }

    /// Truncate path from the beginning to fit within max characters.
    ///
    /// Preserves filename and as much parent path as possible.
    /// Example: `long/path/to/parent/file.rs` with max 20 becomes `...parent/file.rs`
    fn truncate_path_from_start(path: &str, max_chars: usize) -> String {
        if path.len() <= max_chars {
            return path.to_string();
        }

        let components: Vec<&str> = path.split('/').collect();
        if components.is_empty() {
            return path.to_string();
        }

        let filename = components.last().unwrap();

        // If just the filename is too long, truncate it from the end
        if filename.len() + 3 > max_chars {
            return format!(
                "...{}",
                &filename[filename.len().saturating_sub(max_chars - 3)..]
            );
        }

        // Try to include parent directories, working backwards
        let mut result = filename.to_string();
        let ellipsis = "...";

        for component in components.iter().rev().skip(1) {
            let candidate = format!("{component}/{result}");
            if candidate.len() + ellipsis.len() <= max_chars {
                result = candidate;
            } else {
                break;
            }
        }

        format!("{ellipsis}{result}")
    }

    /// Format git branch and ahead/behind for display.
    fn git_branch_display(&self) -> String {
        if let Some(info) = &self.branch_info {
            let mut parts = vec![info.branch_name.clone()];

            // Add ahead/behind indicators using Unicode arrows
            if info.ahead > 0 {
                parts.push(format!("\u{2191}{}", info.ahead));
            }
            if info.behind > 0 {
                parts.push(format!("\u{2193}{}", info.behind));
            }

            parts.join(" ")
        } else {
            "Not in git repo".to_string()
        }
    }

    /// Format working tree status for display.
    fn working_tree_status_display(&self) -> String {
        if self.branch_info.is_none() {
            return String::new();
        }

        if self.git_status_files.is_empty() {
            return "clean".to_string();
        }

        let mut staged = 0;
        let mut unstaged = 0;
        let mut untracked = 0;

        for entry in &self.git_status_files {
            match entry.status.as_str() {
                "??" => untracked += 1,
                _ if entry.staged => staged += 1,
                _ => unstaged += 1,
            }
        }

        let mut parts = Vec::new();
        if staged > 0 {
            parts.push(format!("{staged} staged"));
        }
        if unstaged > 0 {
            parts.push(format!("{unstaged} unstaged"));
        }
        if untracked > 0 {
            parts.push(format!("{untracked} untracked"));
        }

        parts.join(", ")
    }
}

impl RenderOnce for StatusBar {
    fn render(self, _window: &mut gpui::Window, _cx: &mut gpui::App) -> impl IntoElement {
        let git_branch = self.git_branch_display();
        let git_wt_status = self.working_tree_status_display();

        // Check if we're in review mode (any review info present)
        let in_review_mode = self.comparison_mode.is_some()
            || self.hunk_position.is_some()
            || self.review_file_progress.is_some();

        // Apply path truncation based on available space
        // Review mode has center section, so less space for path
        let max_path_chars = if in_review_mode { 40 } else { 80 };
        let file_display = self
            .file_path
            .map(|p| Self::truncate_path_from_start(&p, max_path_chars))
            .unwrap_or_else(|| "[No file]".to_string());

        // Build center section for review info
        let center_div = if in_review_mode {
            let mut center = div().flex().items_center().gap_2();

            // Add comparison mode
            if let Some(mode) = self.comparison_mode {
                let mode_text = format!("[{}]", mode.display_name());
                center = center
                    .child(div().text_color(rgb(0x4ec9b0)).child(mode_text))
                    .child(div().text_color(rgb(0x808080)).child("|"));
            }

            // Add hunk position
            if let Some((current, total)) = self.hunk_position {
                let patch_text = format!("Patch {current}/{total}");
                center = center
                    .child(div().text_color(rgb(0x4ec9b0)).child(patch_text))
                    .child(div().text_color(rgb(0x808080)).child("|"));
            }

            // Add file progress
            if let Some((current, total_files)) = self.review_file_progress {
                let file_text = format!("File {current}/{total_files}");
                center = center.child(div().text_color(rgb(0x4ec9b0)).child(file_text));
            }

            Some(center)
        } else {
            None
        };

        // Build right section with git info and mode
        let mut right_div = div().flex().items_center().gap_2();

        // Add git branch
        right_div = right_div.child(div().text_color(rgb(0xd4d4d4)).child(git_branch));

        // Add working tree status if present
        if !git_wt_status.is_empty() {
            right_div = right_div
                .child(div().text_color(rgb(0x808080)).child("|"))
                .child(div().text_color(rgb(0xd4d4d4)).child(git_wt_status));
        }

        // Create 3-section layout: left, center, right
        let mut main_div = div()
            .flex()
            .items_center()
            .h(px(24.0))
            .px(px(16.0))
            .bg(rgb(0x1e1e1e))
            .border_t_1()
            .border_color(rgb(0x3e3e42))
            .text_size(px(11.0))
            .font_family(".AppleSystemUIFontMonospaced");

        // Left section: File path with truncation
        main_div = main_div.child(
            div()
                .flex()
                .flex_1()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(rgb(0xd4d4d4))
                .child(file_display),
        );

        // Center section: Review info (only when in review mode)
        if let Some(center) = center_div {
            main_div = main_div.child(
                div()
                    .flex()
                    .justify_center()
                    .px(px(16.0)) // Add padding for spacing
                    .child(center),
            );
        }

        // Right section: Git info and mode
        main_div = main_div.child(
            div()
                .flex()
                .flex_1()
                .justify_end()
                .items_center()
                .gap_2()
                .child(right_div)
                .child(div().text_color(rgb(0x808080)).child("|"))
                .child(div().text_color(rgb(0xd4d4d4)).child(self.mode_display)),
        );

        main_div
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_path_unchanged() {
        let result = StatusBar::truncate_path_from_start("foo/bar.rs", 20);
        assert_eq!(result, "foo/bar.rs");
    }

    #[test]
    fn long_path_truncated_from_start() {
        let result = StatusBar::truncate_path_from_start("very/long/path/to/some/file.rs", 20);
        assert_eq!(result, "...to/some/file.rs");
    }

    #[test]
    fn preserves_filename() {
        let result = StatusBar::truncate_path_from_start("a/b/c/d/e/f/g/important.rs", 25);
        assert!(result.ends_with("important.rs"));
    }

    #[test]
    fn very_long_filename_truncated() {
        let result = StatusBar::truncate_path_from_start(
            "path/very_long_filename_that_exceeds_limit.rs",
            20,
        );
        assert_eq!(result.len(), 20);
        assert!(result.starts_with("..."));
    }

    #[test]
    fn preserves_multiple_parent_dirs_when_space_available() {
        let result =
            StatusBar::truncate_path_from_start("stoat/src/actions/git/diff_review/select.rs", 40);
        assert_eq!(result, "...src/actions/git/diff_review/select.rs");
        assert!(
            result.len() <= 40,
            "Result is {} chars, expected <= 40",
            result.len()
        );
    }
}
