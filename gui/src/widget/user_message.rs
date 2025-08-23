use crate::widget::theme::{Colors, Style};
use iced::{
    Background, Border, Element, Length,
    widget::{Column, button, container, text},
};
use stoat_core::nodes::UserMessageNode;

/// Messages that can be emitted by the user message widget
#[derive(Debug, Clone)]
pub enum UserMessageMessage {
    /// User clicked to expand/collapse the message
    ToggleExpanded,
    /// User clicked the message (for selection/highlighting)
    MessageClicked,
}

/// A widget that displays a user message from conversation history
#[derive(Debug)]
pub struct UserMessageWidget {
    /// The core user message node
    user_message_node: UserMessageNode,
    /// Whether the message is expanded to show full content
    expanded: bool,
    /// Maximum length for truncated display
    max_truncated_length: usize,
}

impl UserMessageWidget {
    /// Create a new user message widget
    pub fn new(user_message_node: UserMessageNode) -> Self {
        Self {
            user_message_node,
            expanded: false,
            max_truncated_length: 50,
        }
    }

    /// Update the widget state
    pub fn update(&mut self, message: UserMessageMessage) {
        match message {
            UserMessageMessage::ToggleExpanded => {
                self.expanded = !self.expanded;
            },
            UserMessageMessage::MessageClicked => {
                // For now, just toggle expanded on click
                // Later this could emit events for canvas selection
                self.expanded = !self.expanded;
            },
        }
    }

    /// Create the view for this widget
    pub fn view(&self) -> Element<'_, UserMessageMessage> {
        // Determine what text to display
        let display_text = if self.expanded {
            self.user_message_node.content().to_string()
        } else {
            self.user_message_node
                .truncated_content(self.max_truncated_length)
        };

        // Format timestamp for display
        let timestamp_text = match self.user_message_node.timestamp().elapsed() {
            Ok(duration) => {
                let secs = duration.as_secs();
                if secs < 60 {
                    format!("{}s ago", secs)
                } else if secs < 3600 {
                    format!("{}m ago", secs / 60)
                } else {
                    format!("{}h ago", secs / 3600)
                }
            },
            Err(_) => "now".to_string(),
        };

        // Create the content column
        let mut content_column = Column::new().spacing(4);

        // Add header with "User" label and timestamp
        let header_text = text(format!("User - {}", timestamp_text))
            .size(Style::TEXT_SIZE_SMALL)
            .color(Colors::TEXT_TERTIARY);
        content_column = content_column.push(header_text);

        // Add message content
        let message_text = text(display_text)
            .size(Style::TEXT_SIZE_REGULAR)
            .color(Colors::TEXT_PRIMARY);
        content_column = content_column.push(message_text);

        // Add expand/collapse indicator if needed
        if !self.expanded && self.user_message_node.content().len() > self.max_truncated_length {
            let expand_text = text("Click to expand...")
                .size(Style::TEXT_SIZE_SMALL)
                .color(Colors::TEXT_SECONDARY);
            content_column = content_column.push(expand_text);
        } else if self.expanded {
            let collapse_text = text("Click to collapse")
                .size(Style::TEXT_SIZE_SMALL)
                .color(Colors::TEXT_SECONDARY);
            content_column = content_column.push(collapse_text);
        }

        // Wrap in clickable button
        let clickable_content = button(
            container(content_column)
                .padding(Style::SPACING_MEDIUM)
                .width(Length::Fill),
        )
        .on_press(UserMessageMessage::MessageClicked)
        .padding(0);

        // Style the container with user message appearance
        container(clickable_content)
            .width(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(Background::Color(iced::Color::from_rgb(0.2, 0.3, 0.6))), // Blue tint for user messages
                border: Border {
                    color: iced::Color::from_rgb(0.3, 0.4, 0.7),
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..Default::default()
            })
            .into()
    }
}

impl Default for UserMessageWidget {
    fn default() -> Self {
        // Create a default user message node for default widget
        // FIXME: This should be updated when UserMessageNode is refactored to use BufferId
        let default_node = UserMessageNode::new(
            stoat_core::node::NodeId(0),
            "Default User Message".to_string(),
            "This is a default user message".to_string(),
        );
        Self::new(default_node)
    }
}
