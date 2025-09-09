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
            .justify_between()
            .items_center()
            .pb_4()
            .mb_4()
            .border_b_1()
            .border_color(self.theme.line_number)
            .child(
                div()
                    .text_xl()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(self.theme.foreground)
                    .child("Stoat Editor Help"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(self.theme.comment)
                    .child("Press Esc to close"),
            )
    }

    /// Renders a section of help content.
    fn render_section(
        &self,
        title: &'static str,
        content: Vec<(&'static str, &'static str)>,
    ) -> impl IntoElement {
        div()
            .mb_6()
            .child(
                div()
                    .text_lg()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(self.theme.keyword)
                    .mb_3()
                    .child(title),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(content.into_iter().map(|(key, desc)| {
                        div()
                            .flex()
                            .flex_row()
                            .items_start()
                            .child(
                                div()
                                    .w(px(100.0))
                                    .flex_shrink_0()
                                    .font_family("JetBrains Mono")
                                    .text_color(self.theme.string)
                                    .child(key),
                            )
                            .child(div().flex_1().text_color(self.theme.foreground).child(desc))
                    })),
            )
    }

    /// Renders the main help content.
    fn render_content(&self) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .child(self.render_section(
                "Normal Mode",
                vec![
                    ("h, j, k, l", "Move cursor left, down, up, right"),
                    ("i", "Enter insert mode"),
                    ("v", "Enter visual mode"),
                    (":", "Enter command mode"),
                    ("?", "Toggle this help dialog"),
                    ("Esc", "Exit to normal mode / Close help"),
                ],
            ))
            .child(self.render_section(
                "Insert Mode",
                vec![
                    ("Esc", "Return to normal mode"),
                    ("Enter", "Insert newline"),
                    ("Backspace", "Delete character"),
                    ("Any text", "Insert at cursor position"),
                ],
            ))
            .child(self.render_section(
                "Visual Mode",
                vec![
                    ("Esc", "Return to normal mode"),
                    ("h, j, k, l", "Extend selection"),
                ],
            ))
            .child(self.render_section(
                "Command Mode",
                vec![
                    ("Esc", "Return to normal mode"),
                    (":q", "Quit editor"),
                    (":w", "Save file (planned)"),
                    (":wq", "Save and quit (planned)"),
                ],
            ))
            .child(self.render_section(
                "About Stoat",
                vec![
                    ("Version", "0.1.0 (experimental)"),
                    ("License", "See LICENSE file"),
                    ("Concept", "Canvas-based, node-oriented text editor"),
                    ("Status", "Prototype/exploration phase"),
                ],
            ))
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
                    .w(px(600.0))
                    .h(px(500.0))
                    .max_w_full()
                    .max_h_full()
                    .bg(self.theme.background)
                    .border_1()
                    .border_color(self.theme.line_number)
                    .rounded_lg()
                    .shadow_2xl()
                    .flex()
                    .flex_col()
                    .child(
                        // Dialog content with padding and scrolling
                        div()
                            .p_6()
                            .flex()
                            .flex_col()
                            .size_full()
                            .overflow_hidden()
                            .child(self.render_header())
                            .child(
                                // Scrollable content area
                                div()
                                    .flex_1()
                                    .overflow_y_hidden()
                                    .child(self.render_content()),
                            ),
                    ),
            )
    }
}
