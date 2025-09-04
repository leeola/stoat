//! Custom text editor widget with cosmic-text integration.
//!
//! This module provides a high-performance text editor widget that properly
//! handles tabs, complex text shaping, and efficient rendering using cosmic-text.

mod buffer;
mod cache;
mod event_handler;
mod layout;
mod renderer;
mod simple_widget;
mod widget;

// Re-export the main widgets
pub use buffer::{calculate_visual_column, visual_column_to_byte_offset};
pub use simple_widget::SimpleCustomTextEditor;
pub use widget::CustomTextEditor;

/// Creates a new custom text editor widget
///
/// # Example
/// ```no_run
/// use stoat_gui::editor;
///
/// let widget = editor::custom_text_editor(&state, &theme)
///     .on_input(Message::EditorInput)
///     .show_line_numbers(true)
///     .highlight_current_line(true);
/// ```
pub fn custom_text_editor<'a>(
    state: &'a stoat::EditorState,
    theme: &'a crate::theme::EditorTheme,
) -> CustomTextEditor<'a> {
    CustomTextEditor::new(state, theme)
}
