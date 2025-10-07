//! Command palette modal for fuzzy command search
//!
//! Renders the command palette modal overlay based on state from [`stoat::Stoat`]. This component
//! is stateless - all state management and input handling happens in the core via the mode system.

use gpui::{
    div, prelude::FluentBuilder, px, rgb, rgba, App, IntoElement, ParentElement, RenderOnce,
    Styled, Window,
};
use stoat::CommandInfo;

/// Command palette modal renderer.
///
/// This is a stateless component that renders the command palette UI based on state from
/// [`stoat::Stoat`]. All interaction is handled through the normal action system in
/// command_palette mode:
///
/// - Text input goes to the input buffer via [`stoat::Stoat::insert_text`]
/// - Backspace deletes from input buffer via [`stoat::Stoat::delete_left`]
/// - Arrow keys navigate via
///   [`stoat::Stoat::command_palette_next`]/[`stoat::Stoat::command_palette_prev`]
/// - Escape dismisses via [`stoat::Stoat::command_palette_dismiss`]
/// - Enter executes via [`stoat::Stoat::command_palette_execute`]
///
/// The command palette is displayed when [`stoat::Stoat::mode`] returns `"command_palette"`.
#[derive(IntoElement)]
pub struct CommandPalette {
    query: String,
    commands: Vec<CommandInfo>,
    selected: usize,
}

impl CommandPalette {
    /// Create a new command palette renderer with the given state.
    pub fn new(query: String, commands: Vec<CommandInfo>, selected: usize) -> Self {
        Self {
            query,
            commands,
            selected,
        }
    }

    /// Render the input box showing the current query.
    fn render_input(&self) -> impl IntoElement {
        let query = self.query.clone();

        div()
            .p(px(6.0))
            .border_b_1()
            .border_color(rgb(0x3e3e42))
            .bg(rgb(0x252526))
            .text_color(rgb(0xd4d4d4))
            .child(if query.is_empty() {
                "Type to search commands...".to_string()
            } else {
                query
            })
    }

    /// Render the list of filtered commands.
    fn render_command_list(&self) -> impl IntoElement {
        let commands = &self.commands;
        let selected = self.selected;

        div()
            .flex()
            .flex_col()
            .flex_1()
            .children(commands.iter().enumerate().map(|(i, cmd)| {
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .px(px(12.0))
                    .py(px(4.0))
                    .when(i == selected, |div| {
                        div.bg(rgb(0x3b4261)) // Blue-gray highlight for selected command
                    })
                    .child(
                        div()
                            .text_color(rgb(0xd4d4d4))
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(cmd.name.clone()),
                    )
                    .child(
                        div()
                            .text_color(rgb(0x808080))
                            .text_size(px(10.0))
                            .child(cmd.description.clone()),
                    )
            }))
    }
}

impl RenderOnce for CommandPalette {
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
                    .h(px(viewport_height * 0.6))
                    .bg(rgb(0x1e1e1e)) // Dark background matching VS Code theme
                    .border_1()
                    .border_color(rgb(0x3e3e42)) // Subtle border
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .child(self.render_input())
                    .child(self.render_command_list()),
            )
    }
}
