//! Command info widget that displays available commands for the current mode.

use crate::{messages::Message, theme::EditorTheme};
use iced::{
    widget::{column, container, text},
    Element, Length,
};
use stoat::{actions::EditMode, Keymap};

/// Command info widget that shows available key bindings.
pub struct CommandInfo<'a> {
    mode: EditMode,
    keymap: &'a Keymap,
    theme: &'a EditorTheme,
}

impl<'a> CommandInfo<'a> {
    /// Creates a new command info widget.
    pub fn new(mode: EditMode, keymap: &'a Keymap, theme: &'a EditorTheme) -> Self {
        Self {
            mode,
            keymap,
            theme,
        }
    }

    /// Builds the widget into an iced Element.
    pub fn build(self) -> Element<'a, Message> {
        let bindings = self.keymap.get_bindings_for_mode(self.mode);

        if bindings.is_empty() {
            return container(text("No commands available")).padding(8).into();
        }

        let mut items = Vec::new();

        // Add a title showing current mode
        let mode_text = match self.mode {
            EditMode::Normal => "NORMAL MODE",
            EditMode::Insert => "INSERT MODE",
            EditMode::Visual { .. } => "VISUAL MODE",
            EditMode::Command => "COMMAND MODE",
        };

        items.push(text(mode_text).size(self.theme.small_font_size()).into());

        // Sort bindings by key for consistent display
        let mut sorted_bindings = bindings;
        sorted_bindings.sort_by(|a, b| a.0.cmp(&b.0));

        // Add each key binding
        for (key_str, command) in sorted_bindings {
            let binding_text = format!("{} - {}", key_str, command.short_name());
            items.push(text(binding_text).size(self.theme.small_font_size()).into());
        }

        let content = column(items).spacing(2);

        container(content)
            .padding(8)
            .width(Length::Shrink)
            .height(Length::Shrink)
            .into()
    }
}
