use crate::widget::theme::{Colors, Style};
use iced::{
    alignment,
    widget::{column, container, row, scrollable, text, Column, Space},
    Background, Border, Element, Length, Padding,
};

/// Help modal state
#[derive(Debug, Clone, PartialEq)]
pub enum HelpState {
    /// No help modal shown
    Hidden,
    /// Basic help showing key bindings
    Basic,
    /// Extended help with detailed descriptions
    Extended,
    /// Extended help for a specific action
    ActionSpecific(String),
}

/// Help modal widget
pub struct HelpModal {
    state: HelpState,
    commands: Vec<CommandHelp>,
    mode_name: String,
    extended_help: Option<String>,
}

/// A command with help information
#[derive(Debug, Clone)]
pub struct CommandHelp {
    pub key: String,
    pub action: String,
    pub description: String,
}

impl Default for HelpModal {
    fn default() -> Self {
        Self {
            state: HelpState::Hidden,
            commands: Vec::new(),
            mode_name: String::new(),
            extended_help: None,
        }
    }
}

impl HelpModal {
    /// Create a new help modal
    pub fn new() -> Self {
        Self::default()
    }

    /// Update help content
    pub fn update_content(&mut self, mode: &str, commands: Vec<(String, String, String)>) {
        self.mode_name = mode.to_string();
        self.commands = commands
            .into_iter()
            .map(|(key, action, description)| CommandHelp {
                key,
                action,
                description,
            })
            .collect();
    }

    /// Set extended help text
    pub fn set_extended_help(&mut self, help_text: Option<String>) {
        self.extended_help = help_text;
    }

    /// Show basic help
    pub fn show_basic(&mut self) {
        self.state = HelpState::Basic;
    }

    /// Show extended help
    pub fn show_extended(&mut self) {
        self.state = HelpState::Extended;
    }

    /// Show help for specific action
    pub fn show_action_help(&mut self, action: String) {
        self.state = HelpState::ActionSpecific(action);
    }

    /// Hide the modal
    pub fn hide(&mut self) {
        self.state = HelpState::Hidden;
    }

    /// Check if modal is visible
    pub fn is_visible(&self) -> bool {
        self.state != HelpState::Hidden
    }

    /// Get current state
    pub fn state(&self) -> &HelpState {
        &self.state
    }

    /// Create the view for this widget
    pub fn view<'a, M>(&'a self) -> Element<'a, M>
    where
        M: 'a + Clone,
    {
        if !self.is_visible() {
            return Space::new(0, 0).into();
        }

        // Create modal background overlay (not needed, we'll use the modal itself)

        // Create modal content
        let content = match &self.state {
            HelpState::Hidden => return Space::new(0, 0).into(),
            HelpState::Basic => self.create_basic_help(),
            HelpState::Extended => self.create_extended_help(),
            HelpState::ActionSpecific(action) => self.create_action_help(action),
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

    fn create_basic_help<'a, M>(&'a self) -> Element<'a, M>
    where
        M: 'a + Clone,
    {
        let title = text(format!(
            "Help: {} Mode",
            self.mode_name
                .chars()
                .next()
                .unwrap()
                .to_uppercase()
                .collect::<String>()
                + &self.mode_name[1..]
        ))
        .size(Style::TEXT_SIZE_LARGE)
        .color(Colors::TEXT_PRIMARY);

        let subtitle = text("Press any key to see detailed help, Esc to close")
            .size(Style::TEXT_SIZE_SMALL)
            .color(Colors::TEXT_SECONDARY);

        let mut commands_column = Column::new().spacing(8);

        for cmd in &self.commands {
            let key_text = text(&cmd.key)
                .size(Style::TEXT_SIZE_REGULAR)
                .color(Colors::ACCENT_PRIMARY);

            let action_text = text(&cmd.action)
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

    fn create_extended_help<'a, M>(&'a self) -> Element<'a, M>
    where
        M: 'a + Clone,
    {
        let title = text(format!("{} Mode - Extended Help", self.mode_name))
            .size(Style::TEXT_SIZE_LARGE)
            .color(Colors::TEXT_PRIMARY);

        let subtitle = text("Press any key to see detailed help, Esc to close")
            .size(Style::TEXT_SIZE_SMALL)
            .color(Colors::TEXT_SECONDARY);

        let mut commands_column = Column::new().spacing(16);

        for cmd in &self.commands {
            let key_text = text(&cmd.key)
                .size(Style::TEXT_SIZE_REGULAR)
                .color(Colors::ACCENT_PRIMARY);

            let action_text = text(&cmd.action)
                .size(Style::TEXT_SIZE_REGULAR)
                .color(Colors::TEXT_PRIMARY);

            let description_text = text(&cmd.description)
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

    fn create_action_help<'a, M>(&'a self, action: &str) -> Element<'a, M>
    where
        M: 'a + Clone,
    {
        let title = text(format!("Help: {action}"))
            .size(Style::TEXT_SIZE_LARGE)
            .color(Colors::TEXT_PRIMARY);

        let subtitle = text("Press Esc to return to help overview")
            .size(Style::TEXT_SIZE_SMALL)
            .color(Colors::TEXT_SECONDARY);

        let help_content = if let Some(ref help_text) = self.extended_help {
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
