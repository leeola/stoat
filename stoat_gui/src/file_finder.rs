//! File finder modal for quick file navigation
//!
//! Renders the file finder modal overlay based on state from [`stoat::Stoat`]. This component
//! is stateless - all state management and input handling happens in the core via the mode system.

use gpui::{
    App, IntoElement, ParentElement, RenderOnce, Styled, Window, div, prelude::FluentBuilder, px,
    rgb, rgba,
};
use std::path::PathBuf;

/// File finder modal renderer.
///
/// This is a stateless component that renders the file finder UI based on state from
/// [`stoat::Stoat`]. All interaction is handled through the normal action system in file_finder
/// mode:
///
/// - Text input goes to the input buffer via [`stoat::Stoat::insert_text`]
/// - Backspace deletes from input buffer via [`stoat::Stoat::delete_left`]
/// - Arrow keys navigate via [`stoat::Stoat::file_finder_next`]/[`stoat::Stoat::file_finder_prev`]
/// - Escape dismisses via [`stoat::Stoat::file_finder_dismiss`]
///
/// The file finder is displayed when [`stoat::Stoat::mode`] returns `"file_finder"`.
#[derive(IntoElement)]
pub struct FileFinder {
    query: String,
    files: Vec<PathBuf>,
    selected: usize,
    preview: Option<String>,
}

impl FileFinder {
    /// Create a new file finder renderer with the given state.
    pub fn new(
        query: String,
        files: Vec<PathBuf>,
        selected: usize,
        preview: Option<String>,
    ) -> Self {
        Self {
            query,
            files,
            selected,
            preview,
        }
    }

    /// Render the input box showing the current query.
    fn render_input(&self) -> impl IntoElement {
        let query = self.query.clone();

        div()
            .p(px(8.0))
            .border_b_1()
            .border_color(rgb(0x3e3e42))
            .bg(rgb(0x252526))
            .text_color(rgb(0xd4d4d4))
            .child(if query.is_empty() {
                "Type to search files...".to_string()
            } else {
                query
            })
    }

    /// Render the list of filtered files.
    fn render_file_list(&self) -> impl IntoElement {
        let files = &self.files;
        let selected = self.selected;

        div()
            .flex()
            .flex_col()
            .flex_1()
            .children(files.iter().enumerate().map(|(i, path)| {
                div()
                    .px(px(12.0))
                    .py(px(6.0))
                    .when(i == selected, |div| {
                        div.bg(rgb(0x3b4261)) // Blue-gray highlight for selected file
                    })
                    .text_color(rgb(0xd4d4d4))
                    .child(
                        path.strip_prefix("./")
                            .unwrap_or(path)
                            .to_string_lossy()
                            .to_string(),
                    )
            }))
    }

    /// Render the file preview panel.
    fn render_preview(&self) -> impl IntoElement {
        let preview_text = self.preview.clone().unwrap_or_else(|| {
            "No preview available\n\n(File may be binary or too large)".to_string()
        });

        div()
            .flex()
            .flex_col()
            .flex_1()
            .p(px(12.0))
            .bg(rgb(0x1a1a1a))
            .text_color(rgb(0xd4d4d4))
            .font_family(".AppleSystemUIFontMonospaced")
            .text_size(px(12.0))
            .child(preview_text)
    }
}

impl RenderOnce for FileFinder {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
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
                    .h_3_4()
                    .bg(rgb(0x1e1e1e)) // Dark background matching VS Code theme
                    .border_1()
                    .border_color(rgb(0x3e3e42)) // Subtle border
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .child(self.render_input())
                    .child(
                        // Two-panel layout: file list on left, preview on right
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                // Left panel: file list (40%)
                                div()
                                    .flex()
                                    .flex_col()
                                    .w_2_5()
                                    .border_r_1()
                                    .border_color(rgb(0x3e3e42))
                                    .child(self.render_file_list()),
                            )
                            .child(
                                // Right panel: preview (60%)
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .child(self.render_preview()),
                            ),
                    ),
            )
    }
}
