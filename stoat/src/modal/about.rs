//! About modal displaying Stoat version and build information.
//!
//! This modal displays when user invokes the about command via the command palette
//! or a keybinding. Shows git commit hash and build status (clean or dirty).
//!
//! # Architecture
//!
//! About modal is a mode-based component like [`crate::modal::help::HelpModal`].
//! It's rendered when [`crate::Stoat::mode`] returns `"about_modal"`.

use gpui::{
    div, px, rgb, rgba, App, FontWeight, IntoElement, ParentElement, RenderOnce, Styled, Window,
};

/// About modal renderer showing build information.
///
/// This is a stateless component that renders the about UI. Interaction is handled
/// through the normal action system in about_modal mode:
/// - Escape dismisses via [`crate::actions::AboutModalDismiss`]
///
/// The about modal is displayed when [`crate::Stoat::mode`] returns `"about_modal"`.
///
/// # Content
///
/// Displays:
/// - Git commit hash (short form, 7 characters)
/// - Build status (clean or dirty)
/// - Information is captured at build time via build script
#[derive(IntoElement)]
pub struct AboutModal {}

impl Default for AboutModal {
    fn default() -> Self {
        Self::new()
    }
}

impl AboutModal {
    /// Create a new about modal renderer.
    pub fn new() -> Self {
        Self {}
    }

    /// Render the about content section.
    fn render_content(&self) -> impl IntoElement {
        let build_info = crate::build_info::build_info();

        div()
            .flex()
            .flex_col()
            .gap_6()
            .p(px(32.0))
            .child(
                div()
                    .text_size(px(24.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(0xd4d4d4))
                    .child("About Stoat"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(0xd4d4d4))
                            .child("Build Information"),
                    )
                    .child(self.render_info_row("Commit", build_info.commit_hash))
                    .child(self.render_info_row(
                        "Status",
                        if build_info.dirty { "dirty" } else { "clean" },
                    )),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(0x808080))
                    .child("Press Esc to close this modal"),
            )
    }

    /// Helper to render an information row.
    fn render_info_row(&self, label: &str, value: &str) -> impl IntoElement {
        div()
            .flex()
            .gap_3()
            .child(
                div()
                    .min_w(px(80.0))
                    .text_size(px(12.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(0x569CD6)) // Blue for labels
                    .child(format!("{}:", label)),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(0xc0c0c0))
                    .child(value.to_string()),
            )
    }
}

impl RenderOnce for AboutModal {
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
                    .w_1_2()
                    .max_w(px(500.0))
                    .h(px(viewport_height * 0.3))
                    .bg(rgb(0x1e1e1e)) // Dark background matching VS Code theme
                    .border_1()
                    .border_color(rgb(0x3e3e42)) // Subtle border
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .child(self.render_content()),
            )
    }
}
