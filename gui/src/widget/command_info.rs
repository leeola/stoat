use crate::widget::theme::{Colors, Style};
use iced::{
    widget::{container, row, text, Column},
    Background, Border, Element, Length,
};
use stoat_core::input::CommandInfoState;

/// A widget that displays available commands for the current mode - purely presentational
pub struct CommandInfo;

impl CommandInfo {
    /// Create a new command info widget
    pub fn new() -> Self {
        Self
    }

    /// Create the view for this widget
    pub fn view<M>(command_info_state: CommandInfoState) -> Element<'static, M>
    where
        M: Clone + 'static,
    {
        if !command_info_state.visible {
            return container(text("")).width(Length::Fixed(0.0)).into();
        }

        // Create command list without header (mode is shown in status bar)
        let mut commands_column = Column::new().spacing(1);

        for (key, description) in command_info_state.commands {
            let key_text = text(key)
                .size(Style::TEXT_SIZE_SMALL)
                .color(Colors::TEXT_PRIMARY);

            let desc_text = text(description)
                .size(Style::TEXT_SIZE_SMALL)
                .color(Colors::TEXT_TERTIARY);

            let command_row =
                row![container(key_text).width(Length::Fixed(35.0)), desc_text,].spacing(8);

            commands_column = commands_column.push(command_row);
        }

        // Wrap content with padding
        let content = container(commands_column).padding([4, Style::SPACING_MEDIUM as u16]);

        // Final container styled like status bar
        // Only top and right borders to connect seamlessly with status bar
        container(content)
            .width(Length::Fixed(160.0))
            .style(|_theme| container::Style {
                background: Some(Background::Color(Colors::NODE_TITLE_BACKGROUND)),
                border: Border {
                    color: Colors::BORDER_DEFAULT,
                    width: 1.0,
                    radius: 0.0.into(), // No rounded corners
                },
                ..Default::default()
            })
            .into()
    }
}
