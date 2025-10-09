//! Git status modal for viewing modified files.
//!
//! Renders a modal overlay for git status viewing. All state management and input handling
//! happens in [`stoat::Stoat`] core - this is just the presentation layer.

use gpui::{
    div, prelude::FluentBuilder, px, rgb, rgba, App, FontWeight, InteractiveElement, IntoElement,
    ParentElement, RenderOnce, ScrollHandle, StatefulInteractiveElement, Styled, Window,
};
use stoat::git_status::GitStatusEntry;

/// Git status modal renderer.
///
/// Stateless component that renders git status UI. All interaction is handled through
/// the action system in git_status mode:
///
/// - Up/Down keys navigate via [`stoat::Stoat::git_status_next`]/[`stoat::Stoat::git_status_prev`]
/// - Escape dismisses via [`stoat::Stoat::git_status_dismiss`]
/// - Enter opens file via git_status_select handler in GUI
///
/// The git status modal is displayed when [`stoat::Stoat::mode`] returns `"git_status"`.
#[derive(IntoElement)]
pub struct GitStatus {
    files: Vec<GitStatusEntry>,
    selected: usize,
    scroll_handle: ScrollHandle,
}

impl GitStatus {
    /// Create a new git status renderer with the given state.
    pub fn new(files: Vec<GitStatusEntry>, selected: usize, scroll_handle: ScrollHandle) -> Self {
        Self {
            files,
            selected,
            scroll_handle,
        }
    }

    /// Render the header showing git status title.
    fn render_header(&self) -> impl IntoElement {
        div()
            .p(px(6.0))
            .border_b_1()
            .border_color(rgb(0x3e3e42))
            .bg(rgb(0x252526))
            .text_color(rgb(0xd4d4d4))
            .font_weight(FontWeight::SEMIBOLD)
            .child("Git Status")
    }

    /// Render the list of modified files.
    fn render_file_list(&self) -> impl IntoElement {
        let files = &self.files;
        let selected = self.selected;

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
                    .px(px(12.0))
                    .py(px(4.0))
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
}

impl RenderOnce for GitStatus {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let viewport_height = f32::from(window.viewport_size().height);

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .bg(rgba(0x00000030)) // Light dimmed background overlay
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_3_4()
                    .max_w(px(800.0))
                    .h(px(viewport_height * 0.6))
                    .bg(rgb(0x1e1e1e)) // Dark background matching VS Code theme
                    .border_1()
                    .border_color(rgb(0x3e3e42)) // Subtle border
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .child(self.render_header())
                    .child(self.render_file_list()),
            )
    }
}
