use crate::widget::theme::{Colors, Style};
use iced::{
    alignment::{Horizontal, Vertical},
    widget::{container, row, text},
    Background, Border, Element, Length,
};
use stoat_core::input::{CommandInputState, Mode};

/// A widget that displays the command palette for entering commands
pub struct CommandPalette;

impl Default for CommandPalette {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandPalette {
    /// Create a new command palette widget
    pub fn new() -> Self {
        Self
    }

    /// Create the view for this widget - shows only when in Command mode
    pub fn view<'a, M>(
        current_mode: &Mode,
        command_input_state: &'a CommandInputState,
    ) -> Element<'a, M>
    where
        M: Clone + 'a,
    {
        // Only show when in Command mode
        if *current_mode != Mode::Command {
            return container(text("")).width(Length::Fixed(0.0)).into();
        }

        // Create the command input display
        let prompt_text = text("M-x ")
            .size(Style::TEXT_SIZE_SMALL)
            .color(Colors::TEXT_PRIMARY);

        let input_text = text(&command_input_state.buffer)
            .size(Style::TEXT_SIZE_SMALL)
            .color(Colors::TEXT_PRIMARY);

        // Add cursor indicator (simple underscore at the end)
        let cursor_text = text("_")
            .size(Style::TEXT_SIZE_SMALL)
            .color(Colors::TEXT_PRIMARY);

        let command_row = row![prompt_text, input_text, cursor_text].spacing(0);

        // Style similar to status bar
        container(command_row)
            .width(Length::Fill)
            .height(Length::Fixed(24.0))
            .padding([0, Style::SPACING_MEDIUM as u16])
            .align_x(Horizontal::Left)
            .align_y(Vertical::Center)
            .style(|_theme| container::Style {
                background: Some(Background::Color(Colors::NODE_TITLE_BACKGROUND)),
                border: Border {
                    color: Colors::BORDER_DEFAULT,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            })
            .into()
    }
}
