//! Compact help popup component for displaying essential keybindings.
//!
//! This module provides a small, non-intrusive help popup that appears
//! in the corner of the editor view. It follows GPUI component patterns
//! for stateless rendering and theme integration.

use crate::theme::EditorTheme;
use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window, div, px};

/// Position where the help popup should appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupPosition {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// Compact help popup component that displays essential keybindings.
///
/// This component is designed to be small and unobtrusive, showing
/// only the most important commands in a corner of the screen.
pub struct HelpSmall {
    /// Editor theme for consistent styling
    theme: EditorTheme,
    /// Position of the popup on screen
    position: PopupPosition,
}

impl HelpSmall {
    /// Creates a new help popup with the given theme.
    pub fn new(theme: EditorTheme) -> Self {
        Self {
            theme,
            position: PopupPosition::BottomRight,
        }
    }

    /// Sets the position of the popup.
    pub fn position(mut self, position: PopupPosition) -> Self {
        self.position = position;
        self
    }

    /// Renders the popup header.
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
                    .child("QUICK HELP"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(self.theme.status_bar_fg)
                    .child("F1"),
            )
    }

    /// Renders a key-value help item.
    fn render_item(&self, key: &'static str, desc: &'static str) -> impl IntoElement {
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
                    .child(key),
            )
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(self.theme.status_bar_fg)
                    .child(desc),
            )
    }

    /// Renders the compact help content.
    fn render_content(&self) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            // Movement
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(self.theme.status_bar_fg)
                    .mt_1()
                    .child("Movement"),
            )
            .child(self.render_item("hjkl", "Move cursor"))
            // Modes
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(self.theme.status_bar_fg)
                    .mt_2()
                    .child("Modes"),
            )
            .child(self.render_item("i", "Insert mode"))
            .child(self.render_item("v", "Visual mode"))
            .child(self.render_item("Esc", "Normal mode"))
            // Commands
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(self.theme.status_bar_fg)
                    .mt_2()
                    .child("Commands"),
            )
            .child(self.render_item(":q", "Quit"))
            .child(self.render_item("?", "Toggle help"))
    }
}

impl Render for HelpSmall {
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
