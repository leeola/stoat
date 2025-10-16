//! Full help modal with comprehensive keybinding reference.
//!
//! This modal displays when user presses `?` twice (once to show overlay, second time
//! to open this modal). Unlike the help overlay, this IS a proper mode that takes over
//! the editor until dismissed with Escape.
//!
//! # Architecture
//!
//! Help modal is a mode-based component like [`crate::command_palette::CommandPalette`]
//! and [`crate::file_finder::Finder`]. It's rendered when [`stoat::Stoat::mode`]
//! returns `"help_modal"`.

use gpui::{
    div, px, rgb, rgba, App, FontWeight, IntoElement, ParentElement, RenderOnce, Styled, Window,
};

/// Help modal renderer showing comprehensive keybinding reference.
///
/// This is a stateless component that renders the help UI. Interaction is handled
/// through the normal action system in help_modal mode:
/// - Escape dismisses via [`stoat::actions::HelpModalDismiss`]
///
/// The help modal is displayed when [`stoat::Stoat::mode`] returns `"help_modal"`.
///
/// # Content
///
/// Currently shows bare-bones help content. Will be expanded later with:
/// - Categorized keybinding lists
/// - Mode-specific help sections
/// - Search functionality
#[derive(IntoElement)]
pub struct HelpModal {}

impl Default for HelpModal {
    fn default() -> Self {
        Self::new()
    }
}

impl HelpModal {
    /// Create a new help modal renderer.
    pub fn new() -> Self {
        Self {}
    }

    /// Render the help content section.
    fn render_content(&self) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .p(px(24.0))
            .child(
                div()
                    .text_size(px(20.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(0xd4d4d4))
                    .child("Stoat Editor Help"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(0xd4d4d4))
                            .child("Essential Keybindings"),
                    )
                    .child(self.render_keybinding_row("i", "Enter Insert Mode"))
                    .child(self.render_keybinding_row("Esc", "Enter Normal Mode"))
                    .child(self.render_keybinding_row("v", "Enter Visual Mode"))
                    .child(self.render_keybinding_row("Space", "Leader Key (opens Space Mode)"))
                    .child(self.render_keybinding_row(":", "Open Command Palette"))
                    .child(self.render_keybinding_row("?", "Toggle Help")),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(0xd4d4d4))
                            .child("Navigation (Normal Mode)"),
                    )
                    .child(self.render_keybinding_row("h j k l", "Move cursor"))
                    .child(self.render_keybinding_row("w / b", "Next / Previous word"))
                    .child(self.render_keybinding_row("0 / $", "Line start / end"))
                    .child(self.render_keybinding_row("gg / G", "File start / end"))
                    .child(self.render_keybinding_row("Ctrl-f / Ctrl-b", "Page down / up")),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(0xd4d4d4))
                            .child("Space Mode Commands"),
                    )
                    .child(self.render_keybinding_row("Space p", "Open File Finder"))
                    .child(self.render_keybinding_row("Space b", "Open Buffer Finder"))
                    .child(self.render_keybinding_row("Space g", "Open Git Status"))
                    .child(self.render_keybinding_row("Space a", "Enter Pane Mode")),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(0x808080))
                    .child("Press Esc to close this help modal"),
            )
    }

    /// Helper to render a keybinding row.
    fn render_keybinding_row(&self, keys: &str, description: &str) -> impl IntoElement {
        div()
            .flex()
            .gap_3()
            .child(
                div()
                    .min_w(px(120.0))
                    .text_size(px(12.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(0x569CD6)) // Blue for keys
                    .child(keys.to_string()),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(0xc0c0c0))
                    .child(description.to_string()),
            )
    }
}

impl RenderOnce for HelpModal {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let viewport_height = f32::from(window.viewport_size().height);

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
                    .max_w(px(800.0))
                    .h(px(viewport_height * 0.7))
                    .bg(rgb(0x1e1e1e)) // Dark background matching VS Code theme
                    .border_1()
                    .border_color(rgb(0x3e3e42)) // Subtle border
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .child(self.render_content()),
            )
    }
}
