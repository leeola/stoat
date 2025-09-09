//! Command panel component for displaying available commands and keybindings.
//!
//! This module provides a compact, non-intrusive panel that appears
//! in the corner of the editor view showing the current mode's available
//! commands. It receives dynamic command data from the editor state.

use crate::theme::EditorTheme;
use gpui::{div, px, Context, IntoElement, ParentElement, Render, Styled, Window};

/// Position where the help popup should appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupPosition {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

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
    /// Position of the panel on screen
    position: PopupPosition,
}

impl CommandPanel {
    /// Creates a new command panel with dynamic command data.
    pub fn new(theme: EditorTheme, mode: String, commands: Vec<(String, String)>) -> Self {
        Self {
            theme,
            mode,
            commands,
            position: PopupPosition::BottomRight,
        }
    }

    /// Sets the position of the panel.
    pub fn position(mut self, position: PopupPosition) -> Self {
        self.position = position;
        self
    }

    /// Renders the panel header.
    fn render_header(&self) -> impl IntoElement {
        div()
            .flex()
            .justify_between()
            .items_center()
            .pb_1()
            .mb_2()
            .border_b_1()
            .border_color(self.theme.line_number)
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(self.theme.status_bar_fg)
                    .child(format!("{} MODE", self.mode.to_uppercase())),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(self.theme.status_bar_fg)
                    .child("? toggle"),
            )
    }

    /// Renders a key-value command item.
    fn render_item(&self, key: &str, desc: &str) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .gap_3()
            .py_0p5()
            .child(
                div()
                    .w(px(35.0))
                    .flex_shrink_0()
                    .font_family("JetBrains Mono")
                    .text_xs()
                    .text_color(self.theme.status_bar_fg)
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
        let mut content = div().flex().flex_col().gap_0p5();

        // Display commands with a maximum to keep panel compact
        const MAX_COMMANDS: usize = 10;

        if self.commands.is_empty() {
            content = content.child(
                div()
                    .text_xs()
                    .text_color(self.theme.comment)
                    .child("No commands available"),
            );
        } else {
            // Group commands for better organization
            let mut movement_cmds = Vec::new();
            let mut mode_cmds = Vec::new();
            let mut other_cmds = Vec::new();

            for (key, desc) in &self.commands {
                if desc.contains("cursor") || desc.contains("Move") {
                    movement_cmds.push((key.as_str(), desc.as_str()));
                } else if desc.contains("mode") || desc.contains("Mode") {
                    mode_cmds.push((key.as_str(), desc.as_str()));
                } else {
                    other_cmds.push((key.as_str(), desc.as_str()));
                }
            }

            // Display movement commands
            if !movement_cmds.is_empty() {
                content = content.child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(self.theme.status_bar_fg)
                        .mt_1()
                        .child("Movement"),
                );
                for (key, desc) in movement_cmds.iter().take(3) {
                    content = content.child(self.render_item(key, desc));
                }
            }

            // Display mode commands
            if !mode_cmds.is_empty() {
                content = content.child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(self.theme.status_bar_fg)
                        .mt_2()
                        .child("Modes"),
                );
                for (key, desc) in mode_cmds.iter().take(3) {
                    content = content.child(self.render_item(key, desc));
                }
            }

            // Display other commands
            if !other_cmds.is_empty() {
                content = content.child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(self.theme.status_bar_fg)
                        .mt_2()
                        .child("Commands"),
                );
                for (key, desc) in other_cmds.iter().take(4) {
                    content = content.child(self.render_item(key, desc));
                }
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
            .w(px(280.0))
            .bg(self.theme.status_bar_bg) // Match status bar background
            .border_t_1()
            .border_l_1()
            .border_color(self.theme.line_number)
            .px_3()
            .py_2()
            .child(
                div().flex().flex_col().child(self.render_header()).child(
                    // Content area with max height and scroll
                    div()
                        .max_h(px(200.0))
                        .overflow_y_hidden()
                        .child(self.render_content()),
                ),
            )
    }
}
