//! Bottom status bar showing file path, git status, and mode.
//!
//! Displays essential context information:
//! - Active file path (left)
//! - Diff review progress (right, when in review mode)
//! - Git branch name and dirty status (right)
//! - Current mode (right - NORMAL, INSERT, etc.)
//!
//! Renders as a small fixed-height bar at the bottom of the window.

use gpui::{IntoElement, ParentElement, RenderOnce, Styled, div, px, rgb};

/// Status bar component showing file path, git status, and mode.
///
/// Small single-line bar at bottom of window displaying:
/// - File path (left)
/// - Diff review progress (right, first when in review mode)
/// - Git branch and status (right)
/// - Mode indicator (right, last)
#[derive(IntoElement)]
pub struct StatusBar {
    /// Mode display name (e.g., "NORMAL", "INSERT")
    mode_display: String,
    /// Git branch info (if in repo)
    branch_info: Option<stoat::git_status::GitBranchInfo>,
    /// Git status entries for detailed status
    git_status_files: Vec<stoat::git_status::GitStatusEntry>,
    /// Current file path for display
    file_path: Option<String>,
    /// Review progress: (reviewed_count, total_count)
    review_progress: Option<(usize, usize)>,
    /// File progress: (current_file, total_files)
    review_file_progress: Option<(usize, usize)>,
}

impl StatusBar {
    /// Create a new status bar.
    pub fn new(
        mode_display: String,
        branch_info: Option<stoat::git_status::GitBranchInfo>,
        git_status_files: Vec<stoat::git_status::GitStatusEntry>,
        file_path: Option<String>,
        review_progress: Option<(usize, usize)>,
        review_file_progress: Option<(usize, usize)>,
    ) -> Self {
        Self {
            mode_display,
            branch_info,
            git_status_files,
            file_path,
            review_progress,
            review_file_progress,
        }
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

    /// Format review progress for display.
    ///
    /// Returns a string like "5/30 reviewed | File 2/5" showing both hunk and file progress.
    fn review_progress_display(&self) -> Option<String> {
        match (self.review_progress, self.review_file_progress) {
            (Some((reviewed, total)), Some((current, total_files))) => Some(format!(
                "{reviewed}/{total} reviewed | File {current}/{total_files}"
            )),
            (Some((reviewed, total)), None) => Some(format!("{reviewed}/{total} reviewed")),
            (None, Some((current, total_files))) => Some(format!("File {current}/{total_files}")),
            (None, None) => None,
        }
    }
}

impl RenderOnce for StatusBar {
    fn render(self, _window: &mut gpui::Window, _cx: &mut gpui::App) -> impl IntoElement {
        let git_branch = self.git_branch_display();
        let git_wt_status = self.working_tree_status_display();
        let review_progress = self.review_progress_display();
        let file_display = self.file_path.unwrap_or_else(|| "[No file]".to_string());

        // Build right section starting with review progress, then git info
        let mut right_div = div().flex().items_center().gap_2();

        // Add review progress first if present
        if let Some(progress) = review_progress {
            right_div = right_div
                .child(div().text_color(rgb(0x4ec9b0)).child(progress))
                .child(div().text_color(rgb(0x808080)).child("|"));
        }

        // Add git branch
        right_div = right_div.child(div().text_color(rgb(0xd4d4d4)).child(git_branch));

        // Add working tree status column if present
        if !git_wt_status.is_empty() {
            right_div = right_div
                .child(div().text_color(rgb(0x808080)).child("|"))
                .child(div().text_color(rgb(0xd4d4d4)).child(git_wt_status));
        }

        div()
            .flex()
            .items_center()
            .h(px(24.0))
            .px(px(16.0))
            .bg(rgb(0x1e1e1e))
            .border_t_1()
            .border_color(rgb(0x3e3e42))
            .text_size(px(11.0))
            .font_family(".AppleSystemUIFontMonospaced")
            .child(
                // Left: File path
                div().text_color(rgb(0xd4d4d4)).child(file_display),
            )
            .child(
                // Right: Review progress, git info, and mode (takes remaining space and pushes to
                // right)
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(right_div)
                    .child(div().text_color(rgb(0x808080)).child("|"))
                    .child(div().text_color(rgb(0xd4d4d4)).child(self.mode_display)),
            )
    }
}
