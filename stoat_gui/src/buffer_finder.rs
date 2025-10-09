//! Buffer finder modal for quick buffer switching.
//!
//! Renders a modal overlay for fuzzy buffer finding. All state management and input handling
//! happens in [`stoat::Stoat`] core - this is just the presentation layer.
//!
//! Similar to [`crate::file_finder::FileFinder`] but for switching between open buffers.

use gpui::{
    div, prelude::FluentBuilder, px, rgb, rgba, App, InteractiveElement, IntoElement,
    ParentElement, RenderOnce, ScrollHandle, StatefulInteractiveElement, Styled, Window,
};
use std::path::PathBuf;

/// Buffer finder modal renderer.
///
/// Stateless component that renders buffer finder UI. All interaction is handled through
/// the action system in buffer_finder mode. Displays a searchable list of open buffers.
#[derive(IntoElement)]
pub struct BufferFinder {
    /// Current search query from input buffer
    query: String,
    /// Filtered list of buffer paths to display
    buffers: Vec<PathBuf>,
    /// Index of currently selected buffer
    selected: usize,
    /// Scroll handle for buffer list
    scroll_handle: ScrollHandle,
}

impl BufferFinder {
    /// Create a new buffer finder renderer with the given state.
    ///
    /// All state is passed in from [`stoat::Stoat`] which owns the actual buffer finder state.
    /// This component is purely presentational.
    pub fn new(
        query: String,
        buffers: Vec<PathBuf>,
        selected: usize,
        scroll_handle: ScrollHandle,
    ) -> Self {
        Self {
            query,
            buffers,
            selected,
            scroll_handle,
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
                "Type to search buffers...".to_string()
            } else {
                query
            })
    }

    /// Render the list of filtered buffers.
    fn render_buffer_list(&self) -> impl IntoElement {
        let buffers = &self.buffers;
        let selected = self.selected;

        div()
            .id("buffer-list")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .children(buffers.iter().enumerate().map(|(i, path)| {
                div()
                    .px(px(8.0))
                    .py(px(3.0))
                    .when(i == selected, |div| {
                        div.bg(rgb(0x3b4261)) // Blue-gray highlight for selected buffer
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

impl RenderOnce for BufferFinder {
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
                    .h(px(viewport_height * 0.85))
                    .bg(rgb(0x1e1e1e))
                    .border_1()
                    .border_color(rgb(0x3e3e42))
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .child(self.render_input())
                    .child(
                        div().flex().flex_row().flex_1().overflow_hidden().child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .child(self.render_buffer_list()),
                        ),
                    ),
            )
    }
}
