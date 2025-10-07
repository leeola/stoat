//! File finder modal for quick file navigation.
//!
//! Renders a modal overlay for fuzzy file finding. All state management and input handling
//! happens in [`stoat_v4::Stoat`] core - this is just the presentation layer.

use gpui::{
    div, prelude::FluentBuilder, px, rgb, rgba, App, IntoElement, ParentElement, RenderOnce,
    Styled, Window,
};
use std::path::PathBuf;

/// File finder modal renderer.
///
/// Stateless component that renders file finder UI. All interaction is handled through
/// the action system in file_finder mode.
#[derive(IntoElement)]
pub struct FileFinder {
    query: String,
    files: Vec<PathBuf>,
    selected: usize,
}

impl FileFinder {
    /// Create a new file finder renderer with the given state.
    pub fn new(query: String, files: Vec<PathBuf>, selected: usize) -> Self {
        Self {
            query,
            files,
            selected,
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
            .overflow_hidden()
            .children(files.iter().enumerate().map(|(i, path)| {
                div()
                    .px(px(8.0))
                    .py(px(3.0))
                    .when(i == selected, |div| {
                        div.bg(rgb(0x3b4261)) // Blue-gray highlight for selected file
                    })
                    .text_color(rgb(0xd4d4d4))
                    .text_size(px(11.0))
                    .child(
                        path.strip_prefix("./")
                            .unwrap_or(path)
                            .to_string_lossy()
                            .to_string(),
                    )
            }))
    }
}

impl RenderOnce for FileFinder {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let viewport_height = f32::from(window.viewport_size().height);

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
                    .w_3_4()
                    .h(px(viewport_height * 0.7))
                    .bg(rgb(0x1e1e1e))
                    .border_1()
                    .border_color(rgb(0x3e3e42))
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .child(self.render_input())
                    .child(self.render_file_list()),
            )
    }
}
