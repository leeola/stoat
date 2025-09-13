//! Help dialog for displaying keybindings and editor information.
//!
//! This module provides a modal help dialog that displays information about
//! Stoat's keybindings, modes, and features. The dialog appears as an overlay
//! centered on the screen and can be dismissed with Esc or by clicking outside.

use crate::theme::EditorTheme;
use gpui::{div, px, Context, IntoElement, ParentElement, Render, Styled, Window};

/// Help dialog component that displays editor information and keybindings.
pub struct HelpDialog {
    /// Editor theme for consistent styling
    theme: EditorTheme,
}

impl HelpDialog {
    /// Creates a new help dialog with the given theme.
    pub fn new(theme: EditorTheme) -> Self {
        Self { theme }
    }

    /// Renders the dialog header.
    fn render_header(&self) -> impl IntoElement {
        div()
            .flex()
            .justify_end()
            .pb_2()
            .mb_2()
            .border_b_1()
            .border_color(self.theme.line_number)
            .child(
                div()
                    .text_sm()
                    .text_color(self.theme.comment)
                    .child("Esc to close"),
            )
    }

    /// Renders a key binding row.
    fn render_binding(&self, key: &'static str, desc: &'static str) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .py_0p5()
            .child(
                div()
                    .w(px(80.0))
                    .flex_shrink_0()
                    .font_family("JetBrains Mono")
                    .text_sm()
                    .text_color(self.theme.string)
                    .child(key),
            )
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(self.theme.foreground)
                    .child(desc),
            )
    }

    /// Renders the main help content.
    fn render_content(&self) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            // Normal mode
            .child(self.render_binding("h j k l", "Move cursor"))
            .child(self.render_binding("i", "Insert mode"))
            .child(self.render_binding("v", "Visual mode"))
            .child(self.render_binding(":", "Command mode"))
            // Insert mode
            .child(self.render_binding("Esc", "Normal mode"))
            .child(self.render_binding("Enter", "New line"))
            .child(self.render_binding("Backspace", "Delete"))
            // Command mode
            .child(self.render_binding(":q", "Quit"))
            .child(self.render_binding(":w", "Save (planned)"))
            .child(self.render_binding(":wq", "Save & quit (planned)"))
            // Help
            .child(self.render_binding("?", "Toggle help"))
    }
}

impl Render for HelpDialog {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Semi-transparent overlay background
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(gpui::hsla(0.0, 0.0, 0.0, 0.5)) // Semi-transparent black overlay
            .flex()
            .justify_center()
            .items_center()
            .child(
                // Dialog box
                div()
                    .w(px(350.0))
                    .max_w_full()
                    .bg(self.theme.background)
                    .border_1()
                    .border_color(self.theme.line_number)
                    .rounded_lg()
                    .shadow_2xl()
                    .p_4()
                    .flex()
                    .flex_col()
                    .child(self.render_header())
                    .child(self.render_content()),
            )
    }
}
