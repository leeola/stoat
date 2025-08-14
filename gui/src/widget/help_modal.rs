use crate::widget::theme::{Colors, Style};
use iced::{
    alignment,
    widget::{column, container, row, scrollable, text, Column, Space},
    Background, Border, Element, Length, Padding,
};
use stoat_core::input::{HelpDisplayState, HelpType};

/// Help modal widget - purely presentational, no internal state
pub struct HelpModal;

impl HelpModal {
    /// Create a new help modal
    pub fn new() -> Self {
        Self
    }

    /// Create the view for this widget
    pub fn view<M>(help_state: HelpDisplayState) -> Element<'static, M>
    where
        M: Clone + 'static,
    {
        if !help_state.visible {
            return Space::new(0, 0).into();
        }

        // Create modal content based on help type
        let content = match help_state.help_type {
            HelpType::Mode => Self::create_mode_help(help_state.clone()),
            HelpType::ExtendedMode => Self::create_extended_mode_help(help_state.clone()),
            HelpType::Action => Self::create_action_help(help_state),
        };

        // Center the modal
        let modal = container(content)
            .width(Length::Fixed(800.0))
            .max_height(600.0)
            .padding(Padding::from(20))
            .style(|_theme: &iced::Theme| container::Style {
                background: Some(Background::Color(Colors::NODE_BACKGROUND)),
                border: Border {
                    color: Colors::BORDER_DEFAULT,
                    width: 2.0,
                    radius: 8.0.into(),
                },
                ..Default::default()
            });

        // Stack overlay and modal
        container(modal)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(alignment::Horizontal::Center)
            .align_y(alignment::Vertical::Center)
            .into()
    }

    fn create_mode_help<M>(help_state: HelpDisplayState) -> Element<'static, M>
    where
        M: Clone + 'static,
    {
        let title = text(format!("Help: {}", help_state.title))
            .size(Style::TEXT_SIZE_LARGE)
            .color(Colors::TEXT_PRIMARY);

        let subtitle = text("Press any key to see detailed help, Esc to close")
            .size(Style::TEXT_SIZE_SMALL)
            .color(Colors::TEXT_SECONDARY);

        let mut commands_column = Column::new().spacing(8);

        for (key, action, _description) in help_state.commands {
            let key_text = text(key)
                .size(Style::TEXT_SIZE_REGULAR)
                .color(Colors::ACCENT_PRIMARY);

            let action_text = text(action)
                .size(Style::TEXT_SIZE_REGULAR)
                .color(Colors::TEXT_PRIMARY);

            let command_row =
                row![container(key_text).width(Length::Fixed(100.0)), action_text,].spacing(16);

            commands_column = commands_column.push(command_row);
        }

        let scrollable_content = scrollable(commands_column)
            .height(Length::Fill)
            .width(Length::Fill);

        column![title, subtitle, Space::new(0, 10), scrollable_content,]
            .spacing(10)
            .into()
    }

    fn create_extended_mode_help<M>(help_state: HelpDisplayState) -> Element<'static, M>
    where
        M: Clone + 'static,
    {
        let title = text(format!("{} - Extended Help", help_state.title))
            .size(Style::TEXT_SIZE_LARGE)
            .color(Colors::TEXT_PRIMARY);

        let subtitle = text("Press any key to see detailed help, Esc to close")
            .size(Style::TEXT_SIZE_SMALL)
            .color(Colors::TEXT_SECONDARY);

        let mut commands_column = Column::new().spacing(16);

        for (key, action, description) in help_state.commands {
            let key_text = text(key)
                .size(Style::TEXT_SIZE_REGULAR)
                .color(Colors::ACCENT_PRIMARY);

            let action_text = text(action)
                .size(Style::TEXT_SIZE_REGULAR)
                .color(Colors::TEXT_PRIMARY);

            let description_text = text(description)
                .size(Style::TEXT_SIZE_SMALL)
                .color(Colors::TEXT_SECONDARY);

            let command_section = column![
                row![container(key_text).width(Length::Fixed(100.0)), action_text,].spacing(16),
                container(description_text).padding(Padding {
                    left: 116.0,
                    right: 0.0,
                    top: 4.0,
                    bottom: 0.0,
                }),
            ]
            .spacing(4);

            commands_column = commands_column.push(command_section);
        }

        let scrollable_content = scrollable(commands_column)
            .height(Length::Fill)
            .width(Length::Fill);

        column![title, subtitle, Space::new(0, 10), scrollable_content,]
            .spacing(10)
            .into()
    }

    fn create_action_help<M>(help_state: HelpDisplayState) -> Element<'static, M>
    where
        M: Clone + 'static,
    {
        let title = text(help_state.title.clone())
            .size(Style::TEXT_SIZE_LARGE)
            .color(Colors::TEXT_PRIMARY);

        let subtitle = text("Press Esc to return to help overview")
            .size(Style::TEXT_SIZE_SMALL)
            .color(Colors::TEXT_SECONDARY);

        let help_content = if let Some(help_text) = help_state.extended_help {
            text(help_text)
                .size(Style::TEXT_SIZE_REGULAR)
                .color(Colors::TEXT_PRIMARY)
        } else {
            text("No extended help available for this command")
                .size(Style::TEXT_SIZE_REGULAR)
                .color(Colors::TEXT_SECONDARY)
        };

        let scrollable_content =
            scrollable(container(help_content).width(Length::Fill).padding(10))
                .height(Length::Fill)
                .width(Length::Fill);

        column![title, subtitle, Space::new(0, 10), scrollable_content,]
            .spacing(10)
            .into()
    }
}
