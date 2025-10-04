//! File finder modal for quick file navigation
//!
//! Provides a Ctrl-p style file picker with fuzzy filtering. The file finder displays
//! as a modal overlay showing a list of files that can be filtered by typing.

use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyContext, KeyDownEvent, Modifiers, ParentElement, Render, Styled, Window, div,
    prelude::FluentBuilder, px, rgb, rgba,
};
use std::path::PathBuf;

/// File finder modal for quick file navigation.
///
/// Displays an overlay modal with:
/// - Input box at the top for filtering
/// - List of files below that updates as you type
/// - Keyboard navigation with arrow keys
/// - Escape to dismiss
pub struct FileFinder {
    /// All available files
    files: Vec<PathBuf>,
    /// Currently visible files after filtering
    filtered_files: Vec<PathBuf>,
    /// Index of currently selected file
    selected_index: usize,
    /// Focus handle for the file finder
    focus_handle: FocusHandle,
    /// Previous focus to restore on dismiss
    previous_focus: Option<FocusHandle>,
}

impl FileFinder {
    /// Create a new file finder with the given list of files.
    ///
    /// # Arguments
    /// * `files` - List of file paths to display
    /// * `previous_focus` - Focus handle to restore when dismissed
    /// * `window` - GPUI window reference
    /// * `cx` - Context for creating the file finder
    pub fn new(
        files: Vec<PathBuf>,
        previous_focus: Option<FocusHandle>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let filtered_files = files.clone();
        let focus_handle = cx.focus_handle();

        Self {
            files,
            filtered_files,
            selected_index: 0,
            focus_handle,
            previous_focus,
        }
    }

    /// Update the filtered file list based on the query string.
    ///
    /// Performs simple substring matching (case-insensitive) to filter files.
    fn update_filter(&mut self, query: &str) {
        if query.is_empty() {
            self.filtered_files = self.files.clone();
        } else {
            let query_lower = query.to_lowercase();
            self.filtered_files = self
                .files
                .iter()
                .filter(|path| path.to_string_lossy().to_lowercase().contains(&query_lower))
                .cloned()
                .collect();
        }

        // Reset selection to top
        self.selected_index = 0;
    }

    /// Render the list of filtered files
    fn render_file_list(&self) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .overflow_y_hidden()
            .max_h(px(300.0))
            .children(self.filtered_files.iter().enumerate().map(|(i, path)| {
                div()
                    .px(px(12.0))
                    .py(px(6.0))
                    .when(i == self.selected_index, |div| {
                        div.bg(rgb(0x3b4261)) // Subtle blue-gray highlight
                    })
                    .text_color(rgb(0xd4d4d4)) // Light gray text
                    .child(
                        path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string(),
                    )
            }))
    }

    /// Handle keyboard navigation
    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event.keystroke.key.as_str() {
            "escape" if event.keystroke.modifiers == Modifiers::default() => {
                if let Some(ref previous_focus) = self.previous_focus {
                    window.focus(previous_focus);
                }
                cx.emit(DismissEvent);
            },
            "up" if event.keystroke.modifiers == Modifiers::default() => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                    cx.notify();
                }
            },
            "down" if event.keystroke.modifiers == Modifiers::default() => {
                if self.selected_index + 1 < self.filtered_files.len() {
                    self.selected_index += 1;
                    cx.notify();
                }
            },
            _ => {},
        }
    }
}

impl Focusable for FileFinder {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for FileFinder {}

impl Render for FileFinder {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .bg(rgba(0x00000030)) // Lighter background overlay
            .flex()
            .items_center()
            .justify_center()
            .key_context({
                let mut ctx = KeyContext::new_with_defaults();
                ctx.add("FileFinder");
                ctx
            })
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(600.0))
                    .max_h(px(400.0))
                    .bg(rgb(0x1e1e1e)) // Dark gray background matching VS Code theme
                    .border_1()
                    .border_color(rgb(0x3e3e42)) // Subtle border
                    .rounded(px(8.0))
                    .overflow_hidden()
                    // FIXME: Add input box for filtering
                    .child(
                        // File list
                        self.render_file_list(),
                    ),
            )
    }
}
