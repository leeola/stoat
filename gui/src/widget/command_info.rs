use crate::widget::theme::{Colors, Style};
use iced::{
    widget::{container, row, text, Column},
    Background, Border, Element, Length,
};

/// A widget that displays available commands for the current mode
pub struct CommandInfo {
    commands: Vec<CommandEntry>,
    mode_name: String,
    visible: bool,
}

/// A single command entry showing key and description
#[derive(Debug, Clone)]
pub struct CommandEntry {
    pub key: String,
    pub description: String,
}

impl CommandInfo {
    /// Create a new command info widget
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
            mode_name: String::new(),
            visible: true,
        }
    }

    /// Update the command list based on the current mode
    pub fn update_for_mode(&mut self, mode: &str) {
        self.mode_name = mode.to_string();
        self.commands = Self::get_commands_for_mode(mode);
    }

    /// Update the command list from the actual modal system bindings
    pub fn update_from_bindings(&mut self, mode: &str, bindings: Vec<(String, String)>) {
        self.mode_name = mode.to_string();
        self.commands = bindings
            .into_iter()
            .take(5) // Limit display to 5 most relevant commands
            .map(|(key, description)| CommandEntry { key, description })
            .collect();
    }

    /// Get the list of commands for a specific mode
    fn get_commands_for_mode(mode: &str) -> Vec<CommandEntry> {
        match mode.to_lowercase().as_str() {
            "normal" => vec![
                CommandEntry {
                    key: "i".into(),
                    description: "Insert mode".into(),
                },
                CommandEntry {
                    key: "Esc".into(),
                    description: "Exit app".into(),
                },
                CommandEntry {
                    key: "hjkl".into(),
                    description: "Navigate".into(),
                },
                CommandEntry {
                    key: ":".into(),
                    description: "Command mode".into(),
                },
                CommandEntry {
                    key: "v".into(),
                    description: "Visual mode".into(),
                },
            ],
            "insert" => vec![
                CommandEntry {
                    key: "Esc".into(),
                    description: "Normal mode".into(),
                },
                CommandEntry {
                    key: "Type".into(),
                    description: "Insert text".into(),
                },
            ],
            "command" => vec![
                CommandEntry {
                    key: "Enter".into(),
                    description: "Execute".into(),
                },
                CommandEntry {
                    key: "Esc".into(),
                    description: "Cancel".into(),
                },
                CommandEntry {
                    key: "Tab".into(),
                    description: "Complete".into(),
                },
            ],
            "visual" => vec![
                CommandEntry {
                    key: "Esc".into(),
                    description: "Normal mode".into(),
                },
                CommandEntry {
                    key: "y".into(),
                    description: "Yank".into(),
                },
                CommandEntry {
                    key: "d".into(),
                    description: "Delete".into(),
                },
                CommandEntry {
                    key: "c".into(),
                    description: "Change".into(),
                },
            ],
            _ => vec![CommandEntry {
                key: "?".into(),
                description: "Unknown mode".into(),
            }],
        }
    }

    /// Create the view for this widget
    pub fn view<'a, M>(&'a self) -> Element<'a, M>
    where
        M: 'a,
    {
        if !self.visible {
            return container(text("")).width(Length::Fixed(0.0)).into();
        }

        // Create command list without header (mode is shown in status bar)
        let mut commands_column = Column::new().spacing(1);

        for entry in &self.commands {
            let key_text = text(&entry.key)
                .size(Style::TEXT_SIZE_SMALL)
                .color(Colors::TEXT_PRIMARY);

            let desc_text = text(&entry.description)
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
            .style(|_theme| {
                let mut style = container::Style::default();
                style.background = Some(Background::Color(Colors::NODE_TITLE_BACKGROUND));
                // Custom border - only top and right edges
                style.border = Border {
                    color: Colors::BORDER_DEFAULT,
                    width: 1.0,
                    radius: 0.0.into(), // No rounded corners
                };
                style
            })
            .into()
    }
}
