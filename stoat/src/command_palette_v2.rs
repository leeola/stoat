//! CommandPaletteV2 - Entity-based command palette using InlineEditor.
//!
//! Unlike the original CommandPalette which stores state in AppState, this version
//! is a GPUI entity that owns its input field and manages its own state. It uses
//! InlineEditor for text input and subscribes to buffer changes for live filtering.

use crate::{
    actions::DismissCommandPaletteV2, inline_editor::InlineEditor, stoat::KeyContext, CommandInfo,
};
use gpui::{
    div, prelude::FluentBuilder, px, rgb, rgba, App, AppContext, Context, Entity, EventEmitter,
    FontWeight, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, ScrollHandle,
    StatefulInteractiveElement, Styled, Subscription, Window,
};

/// CommandPaletteV2 entity for fuzzy command search.
///
/// This is a GPUI entity that owns an InlineEditor for input and manages
/// command filtering. Filtering is triggered manually via `update_filter()`.
pub struct CommandPaletteV2 {
    /// Input field using InlineEditor
    input: Entity<InlineEditor>,
    /// All available commands
    all_commands: Vec<CommandInfo>,
    /// Filtered commands matching current query
    filtered_commands: Vec<CommandInfo>,
    /// Selected index in filtered list
    selected_index: usize,
    /// Previous KeyContext to restore on dismiss
    previous_key_context: Option<KeyContext>,
    /// Scroll handle for command list
    scroll_handle: ScrollHandle,
    /// Subscription to buffer changes for automatic filtering
    _buffer_subscription: Subscription,
}

impl CommandPaletteV2 {
    /// Create a new CommandPaletteV2 with the given commands.
    ///
    /// Initializes with all commands visible (no filter) and first command selected.
    /// Subscribes to InlineEditor changes to automatically trigger filtering.
    pub fn new(commands: Vec<CommandInfo>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InlineEditor::new_single_line(cx));

        // Subscribe to InlineEditor events for automatic filtering when text changes
        let input_subscription = cx.subscribe(&input, |this, _input, _event, cx| {
            this.update_filter(cx);
            cx.notify();
        });

        Self {
            input,
            all_commands: commands.clone(),
            filtered_commands: commands,
            selected_index: 0,
            previous_key_context: None,
            scroll_handle: ScrollHandle::new(),
            _buffer_subscription: input_subscription,
        }
    }

    /// Set the previous KeyContext to restore when dismissed.
    pub fn set_previous_key_context(&mut self, context: KeyContext) {
        self.previous_key_context = Some(context);
    }

    /// Get the previous KeyContext.
    pub fn previous_key_context(&self) -> Option<KeyContext> {
        self.previous_key_context
    }

    /// Update filter based on current input text.
    ///
    /// Reads the current text from the input buffer and filters the command list.
    /// Should be called after text input to update the filtered results.
    pub fn update_filter(&mut self, cx: &App) {
        let query = self.input.read(cx).text(cx);
        self.filter_commands(&query);
    }

    /// Filter commands based on query string.
    ///
    /// Uses fuzzy matching to find commands whose name or aliases contain
    /// the query characters in order (case-insensitive).
    fn filter_commands(&mut self, query: &str) {
        if query.is_empty() {
            self.filtered_commands = self.all_commands.clone();
            self.selected_index = 0;
            return;
        }

        let query_lower = query.to_lowercase();
        self.filtered_commands = self
            .all_commands
            .iter()
            .filter(|cmd| {
                // Match against command name
                let name_lower = cmd.name.to_lowercase();
                if fuzzy_match(&name_lower, &query_lower) {
                    return true;
                }

                // Match against aliases
                cmd.aliases
                    .iter()
                    .any(|alias| fuzzy_match(&alias.to_lowercase(), &query_lower))
            })
            .cloned()
            .collect();

        // Reset selection to first match
        self.selected_index = 0;
    }

    /// Select the next command in the filtered list.
    pub fn select_next(&mut self) {
        if !self.filtered_commands.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.filtered_commands.len();
        }
    }

    /// Select the previous command in the filtered list.
    pub fn select_prev(&mut self) {
        if !self.filtered_commands.is_empty() {
            if self.selected_index == 0 {
                self.selected_index = self.filtered_commands.len() - 1;
            } else {
                self.selected_index -= 1;
            }
        }
    }

    /// Get the currently selected command, if any.
    pub fn selected_command(&self) -> Option<&CommandInfo> {
        self.filtered_commands.get(self.selected_index)
    }

    /// Get reference to the input buffer for text editing actions.
    pub fn input_buffer(&self, cx: &App) -> Entity<text::Buffer> {
        self.input.read(cx).buffer().clone()
    }

    /// Get reference to the input InlineEditor.
    pub fn input(&self) -> &Entity<InlineEditor> {
        &self.input
    }

    /// Handle key down events on the overlay.
    ///
    /// Checks for Escape key and dispatches DismissCommandPaletteV2 action.
    fn handle_overlay_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" {
            // Dispatch the dismiss action
            window.dispatch_action(Box::new(DismissCommandPaletteV2), cx);
        }
    }
}

/// Simple fuzzy matching: check if all characters of needle appear in haystack in order.
fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    let mut hay_chars = haystack.chars();
    for needle_char in needle.chars() {
        if !hay_chars.any(|c| c == needle_char) {
            return false;
        }
    }
    true
}

impl EventEmitter<()> for CommandPaletteV2 {}

impl Render for CommandPaletteV2 {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let commands = &self.filtered_commands;
        let selected = self.selected_index;
        let viewport_height = f32::from(window.viewport_size().height);

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .bg(rgba(0x00000030)) // Dimmed background overlay
            .flex()
            .items_center()
            .justify_center()
            .on_key_down(cx.listener(Self::handle_overlay_key_down))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_3_4()
                    .max_w(px(800.0))
                    .h(px(viewport_height * 0.6))
                    .bg(rgb(0x1e1e1e))
                    .border_1()
                    .border_color(rgb(0x3e3e42))
                    .rounded(px(8.0))
                    .overflow_hidden()
                    // Input field at top - render the actual InlineEditor
                    .child(
                        div()
                            .p(px(6.0))
                            .border_b_1()
                            .border_color(rgb(0x3e3e42))
                            .bg(rgb(0x252526))
                            .child(self.input.clone()),
                    )
                    // Command list below
                    .child(
                        div()
                            .id("command-list-v2")
                            .flex()
                            .flex_col()
                            .flex_1()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .children(commands.iter().enumerate().map(|(i, cmd)| {
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .px(px(12.0))
                                    .py(px(4.0))
                                    .when(i == selected, |div| {
                                        div.bg(rgb(0x3b4261)) // Blue-gray highlight
                                    })
                                    .child(
                                        div()
                                            .text_color(rgb(0xd4d4d4))
                                            .text_size(px(12.0))
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(cmd.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_color(rgb(0x808080))
                                            .text_size(px(10.0))
                                            .child(cmd.description.clone()),
                                    )
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn creates_palette_with_commands(cx: &mut gpui::TestAppContext) {
        let commands = vec![
            CommandInfo {
                name: "Test1".to_string(),
                description: "First test".to_string(),
                aliases: vec![],
                type_id: std::any::TypeId::of::<()>(),
                hidden: false,
            },
            CommandInfo {
                name: "Test2".to_string(),
                description: "Second test".to_string(),
                aliases: vec![],
                type_id: std::any::TypeId::of::<()>(),
                hidden: false,
            },
        ];

        let palette = cx.new(|cx| CommandPaletteV2::new(commands.clone(), cx));

        assert_eq!(
            cx.read_entity(&palette, |p, _| p.filtered_commands.len()),
            2
        );
        assert_eq!(cx.read_entity(&palette, |p, _| p.selected_index), 0);
    }

    #[gpui::test]
    fn filters_commands_by_name(cx: &mut gpui::TestAppContext) {
        let commands = vec![
            CommandInfo {
                name: "MoveLeft".to_string(),
                description: "Move cursor left".to_string(),
                aliases: vec![],
                type_id: std::any::TypeId::of::<()>(),
                hidden: false,
            },
            CommandInfo {
                name: "MoveRight".to_string(),
                description: "Move cursor right".to_string(),
                aliases: vec![],
                type_id: std::any::TypeId::of::<()>(),
                hidden: false,
            },
            CommandInfo {
                name: "DeleteLeft".to_string(),
                description: "Delete character left".to_string(),
                aliases: vec![],
                type_id: std::any::TypeId::of::<()>(),
                hidden: false,
            },
        ];

        let palette = cx.new(|cx| CommandPaletteV2::new(commands, cx));

        palette.update(cx, |p, _| {
            p.filter_commands("move");
        });

        let count = cx.read_entity(&palette, |p, _| p.filtered_commands.len());
        assert_eq!(count, 2); // MoveLeft and MoveRight
    }

    #[gpui::test]
    fn navigates_filtered_list(cx: &mut gpui::TestAppContext) {
        let commands = vec![
            CommandInfo {
                name: "First".to_string(),
                description: "".to_string(),
                aliases: vec![],
                type_id: std::any::TypeId::of::<()>(),
                hidden: false,
            },
            CommandInfo {
                name: "Second".to_string(),
                description: "".to_string(),
                aliases: vec![],
                type_id: std::any::TypeId::of::<()>(),
                hidden: false,
            },
            CommandInfo {
                name: "Third".to_string(),
                description: "".to_string(),
                aliases: vec![],
                type_id: std::any::TypeId::of::<()>(),
                hidden: false,
            },
        ];

        let palette = cx.new(|cx| CommandPaletteV2::new(commands, cx));

        // Start at 0
        assert_eq!(cx.read_entity(&palette, |p, _| p.selected_index), 0);

        // Next -> 1
        palette.update(cx, |p, _| p.select_next());
        assert_eq!(cx.read_entity(&palette, |p, _| p.selected_index), 1);

        // Next -> 2
        palette.update(cx, |p, _| p.select_next());
        assert_eq!(cx.read_entity(&palette, |p, _| p.selected_index), 2);

        // Next -> wraps to 0
        palette.update(cx, |p, _| p.select_next());
        assert_eq!(cx.read_entity(&palette, |p, _| p.selected_index), 0);

        // Prev -> wraps to 2
        palette.update(cx, |p, _| p.select_prev());
        assert_eq!(cx.read_entity(&palette, |p, _| p.selected_index), 2);
    }

    #[test]
    fn fuzzy_match_works() {
        assert!(fuzzy_match("moveleft", "ml"));
        assert!(fuzzy_match("moveleft", "move"));
        assert!(fuzzy_match("moveleft", "left"));
        assert!(!fuzzy_match("moveleft", "mz"));
        assert!(!fuzzy_match("moveleft", "lm")); // Order matters
    }
}
