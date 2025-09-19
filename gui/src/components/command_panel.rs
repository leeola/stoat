//! Command panel component for displaying available commands and keybindings.
//!
//! This module provides a compact, non-intrusive panel that appears
//! in the corner of the editor view showing the current mode's available
//! commands. It receives dynamic command data from the editor state.

use crate::theme::EditorTheme;
use gpui::{div, px, Context, IntoElement, ParentElement, Render, Styled, Window};

/// Command panel component that displays available commands for the current mode.
///
/// This component shows a dynamic list of commands based on the current
/// editor mode, with their associated key bindings and descriptions.
pub struct CommandPanel {
    /// Editor theme for consistent styling
    theme: EditorTheme,
    /// Current editor mode
    mode: String,
    /// Available commands as (key_binding, description) pairs
    commands: Vec<(String, String)>,
}

impl CommandPanel {
    /// Creates a new command panel with dynamic command data.
    pub fn new(theme: EditorTheme, mode: String, commands: Vec<(String, String)>) -> Self {
        Self {
            theme,
            mode,
            commands,
        }
    }

    /// Renders the panel header.
    fn render_header(&self) -> impl IntoElement {
        div()
            .flex()
            .justify_between()
            .items_center()
            .mb_0p5()
            .border_b_1()
            .border_color(self.theme.line_number)
            .child(
                div()
                    .text_xs()
                    .text_color(self.theme.status_bar_fg)
                    .child(self.mode.to_uppercase()),
            )
            .child(div().text_xs().text_color(self.theme.comment).child("?"))
    }

    /// Renders a key-value command item.
    fn render_item(&self, key: &str, desc: &str) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .gap_2()
            .py_0p5()
            .child(
                div()
                    .w(px(30.0))
                    .flex_shrink_0()
                    .font_family("JetBrains Mono")
                    .text_xs()
                    .text_color(self.theme.foreground)
                    .text_right()
                    .child(key.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(self.theme.status_bar_fg)
                    .child(desc.to_string()),
            )
    }

    /// Renders the command panel content with dynamic commands.
    fn render_content(&self) -> impl IntoElement {
        let mut content = div().flex().flex_col();

        if self.commands.is_empty() {
            content = content.child(
                div()
                    .text_xs()
                    .text_color(self.theme.comment)
                    .child("No commands"),
            );
        } else {
            // Display all commands as a simple list, up to 12 items
            for (key, desc) in self.commands.iter().take(12) {
                content = content.child(self.render_item(key, desc));
            }
        }

        content
    }
}

impl Render for CommandPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Position directly above status bar (24px height) with no gap
        div()
            .absolute()
            .bottom(px(24.0)) // Height of status bar
            .right_0()
            .w(px(220.0))
            .bg(self.theme.status_bar_bg) // Match status bar background
            .border_t_1()
            .border_l_1()
            .border_color(self.theme.line_number)
            .px_2()
            .py_1()
            .child(div().flex().flex_col().child(self.render_header()).child(
                // Content area
                self.render_content(),
            ))
    }
}
