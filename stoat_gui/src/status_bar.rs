//! Bottom status bar showing mode, git status, and file path.
//!
//! Displays essential context information:
//! - Current mode (NORMAL, INSERT, etc.)
//! - Git branch name and dirty status
//! - Active file path
//!
//! Renders as a small fixed-height bar at the bottom of the window.

use gpui::{IntoElement, ParentElement, RenderOnce, Styled, div, px, rgb};

/// Status bar component showing mode, git status, and file path.
///
/// Small single-line bar at bottom of window displaying:
/// - Mode indicator (left)
/// - Git branch and status (middle)
/// - File path (right)
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
}

impl StatusBar {
    /// Create a new status bar.
    pub fn new(
        mode_display: String,
        branch_info: Option<stoat::git_status::GitBranchInfo>,
        git_status_files: Vec<stoat::git_status::GitStatusEntry>,
        file_path: Option<String>,
    ) -> Self {
        Self {
            mode_display,
            branch_info,
            git_status_files,
            file_path,
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
}

impl RenderOnce for StatusBar {
    fn render(self, _window: &mut gpui::Window, _cx: &mut gpui::App) -> impl IntoElement {
        let git_branch = self.git_branch_display();
        let git_wt_status = self.working_tree_status_display();
        let file_display = self.file_path.unwrap_or_else(|| "[No file]".to_string());

        // Build left side with mode and git info
        let mut left_div = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().text_color(rgb(0xd4d4d4)).child(self.mode_display))
            .child(div().text_color(rgb(0x808080)).child("|"))
            .child(div().text_color(rgb(0xd4d4d4)).child(git_branch));

        // Add working tree status column if present
        if !git_wt_status.is_empty() {
            left_div = left_div
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
            .child(left_div)
            .child(
                // Right: File path
                div()
                    .flex_1()
                    .flex()
                    .justify_end()
                    .text_color(rgb(0xd4d4d4))
                    .child(file_display),
            )
    }
}
