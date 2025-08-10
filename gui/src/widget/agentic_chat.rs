use iced::{
    widget::{
        button, column, container, row, scrollable,
        scrollable::{AbsoluteOffset, Id},
        text, text_input, Column,
    },
    Element, Length, Task,
};
use std::collections::VecDeque;
use stoat_agent_claude_code::messages::{MessageContent, SdkMessage};
use uuid::Uuid;

/// Unique identifier for each message in the chat
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageId(Uuid);

impl MessageId {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Role of the message sender
#[derive(Debug, Clone, PartialEq)]
pub enum AgentRole {
    User,
    Agent,
    System,
    Tool { name: String },
}

/// Event types that can occur in the agent chat
/// These will eventually be tied to nodes in the graph
#[derive(Debug, Clone)]
pub enum EventType {
    /// User sent a message
    UserInput,
    /// Agent responded with text
    AgentResponse,
    /// Agent invoked a tool
    ToolInvocation { tool_name: String, tool_id: String },
    /// Tool returned a result
    ToolResult { tool_id: String, success: bool },
    /// System event (initialization, errors, etc.)
    SystemEvent { event_type: String },
    /// Session lifecycle event
    SessionEvent { event_type: String },
}

/// A message in the agentic chat with associated metadata
#[derive(Debug, Clone)]
pub struct AgenticMessage {
    pub id: MessageId,
    pub role: AgentRole,
    pub content: String,
    pub timestamp: std::time::Instant,
    pub event_type: EventType,
    /// Parent message ID for tracking conversation flow
    pub parent_id: Option<MessageId>,
    /// Associated node ID (for future node graph integration)
    pub node_id: Option<String>,
}

impl AgenticMessage {
    pub fn new(role: AgentRole, content: String, event_type: EventType) -> Self {
        Self {
            id: MessageId::new(),
            role,
            content,
            timestamp: std::time::Instant::now(),
            event_type,
            parent_id: None,
            node_id: None,
        }
    }

    pub fn with_parent(mut self, parent_id: MessageId) -> Self {
        self.parent_id = Some(parent_id);
        self
    }

    pub fn with_node_id(mut self, node_id: String) -> Self {
        self.node_id = Some(node_id);
        self
    }
}

/// Events emitted by the agent chat widget
#[derive(Debug, Clone)]
pub enum AgenticChatEvent {
    /// User submitted a message
    MessageSubmitted(String),
    /// Request to scroll to a specific message
    ScrollToMessage(MessageId),
    /// Message selected (for future node highlighting)
    MessageSelected(MessageId),
    /// Clear chat history
    ClearHistory,
}

/// Internal messages for the widget
#[derive(Debug, Clone)]
pub enum Message {
    InputChanged(String),
    SendMessage,
    ScrollViewportChanged(scrollable::Viewport),
    SelectMessage(MessageId),
    ClearChat,
}

/// Configuration for the agent chat widget
#[derive(Debug, Clone)]
pub struct AgenticChatConfig {
    pub max_history: usize,
    pub auto_scroll: bool,
    pub show_timestamps: bool,
    pub show_event_types: bool,
}

impl Default for AgenticChatConfig {
    fn default() -> Self {
        Self {
            max_history: 1000,
            auto_scroll: true,
            show_timestamps: false,
            show_event_types: true,
        }
    }
}

/// Main agent chat widget
pub struct AgenticChat {
    /// Configuration
    config: AgenticChatConfig,
    /// Message history
    messages: VecDeque<AgenticMessage>,
    /// Current input value
    input_value: String,
    /// Selected message (for future node integration)
    selected_message: Option<MessageId>,
    /// Auto-scroll state
    auto_scroll: bool,
    /// Last message ID (for tracking parent relationships)
    last_message_id: Option<MessageId>,
    /// Callback for external events
    on_event: Option<Box<dyn Fn(AgenticChatEvent) -> Task<AgenticChatEvent>>>,
}

impl AgenticChat {
    /// Create a new agent chat widget
    pub fn new() -> Self {
        Self::with_config(AgenticChatConfig::default())
    }

    /// Create with custom configuration
    pub fn with_config(config: AgenticChatConfig) -> Self {
        Self {
            auto_scroll: config.auto_scroll,
            config,
            messages: VecDeque::new(),
            input_value: String::new(),
            selected_message: None,
            last_message_id: None,
            on_event: None,
        }
    }

    /// Set the event callback
    pub fn on_event<F>(mut self, f: F) -> Self
    where
        F: Fn(AgenticChatEvent) -> Task<AgenticChatEvent> + 'static,
    {
        self.on_event = Some(Box::new(f));
        self
    }

    /// Add a message to the chat
    pub fn add_message(&mut self, mut message: AgenticMessage) {
        // Set parent relationship if applicable
        if message.parent_id.is_none() {
            message.parent_id = self.last_message_id;
        }

        // Track the last message for parent relationships
        self.last_message_id = Some(message.id);

        // Add to history
        self.messages.push_back(message);

        // Trim history if needed
        while self.messages.len() > self.config.max_history {
            self.messages.pop_front();
        }
    }

    /// Process an SDK message from the agent
    pub fn process_sdk_message(&mut self, sdk_msg: SdkMessage) {
        match sdk_msg {
            SdkMessage::Assistant { message, .. } => {
                for content in &message.content {
                    match content {
                        MessageContent::Text { text } => {
                            let msg = AgenticMessage::new(
                                AgentRole::Agent,
                                text.clone(),
                                EventType::AgentResponse,
                            );
                            self.add_message(msg);
                        },
                        MessageContent::ToolUse { name, id, .. } => {
                            let msg = AgenticMessage::new(
                                AgentRole::Agent,
                                format!("Invoking tool: {}", name),
                                EventType::ToolInvocation {
                                    tool_name: name.clone(),
                                    tool_id: id.clone(),
                                },
                            );
                            self.add_message(msg);
                        },
                    }
                }
            },
            SdkMessage::System { session_id, .. } => {
                let msg = AgenticMessage::new(
                    AgentRole::System,
                    format!("Session initialized: {}", session_id),
                    EventType::SessionEvent {
                        event_type: "initialized".to_string(),
                    },
                );
                self.add_message(msg);
            },
            SdkMessage::Result { subtype, .. } => {
                let msg = AgenticMessage::new(
                    AgentRole::System,
                    format!("Session result: {:?}", subtype),
                    EventType::SessionEvent {
                        event_type: "completed".to_string(),
                    },
                );
                self.add_message(msg);
            },
            SdkMessage::User { .. } => {
                // User messages are handled separately
            },
        }
    }

    /// Clear all messages
    pub fn clear(&mut self) {
        self.messages.clear();
        self.last_message_id = None;
        self.selected_message = None;
    }

    /// Get the scroll ID for this widget
    fn scroll_id() -> Id {
        Id::new("agentic_chat_scroll")
    }

    /// Update the widget state
    pub fn update(&mut self, message: Message) -> Task<AgenticChatEvent> {
        match message {
            Message::InputChanged(value) => {
                self.input_value = value;
                Task::none()
            },
            Message::SendMessage => {
                if self.input_value.trim().is_empty() {
                    return Task::none();
                }

                let content = self.input_value.clone();
                self.input_value.clear();

                // Add user message
                let user_msg =
                    AgenticMessage::new(AgentRole::User, content.clone(), EventType::UserInput);
                self.add_message(user_msg);

                // Emit event
                let event_task = Task::done(AgenticChatEvent::MessageSubmitted(content));
                if self.auto_scroll {
                    event_task.chain(scrollable::scroll_to(
                        Self::scroll_id(),
                        AbsoluteOffset {
                            x: 0.0,
                            y: f32::MAX,
                        },
                    ))
                } else {
                    event_task
                }
            },
            Message::ScrollViewportChanged(viewport) => {
                // Check if we're at the bottom
                let at_bottom = viewport.relative_offset().y >= 0.95;
                self.auto_scroll = at_bottom;
                Task::none()
            },
            Message::SelectMessage(id) => {
                self.selected_message = Some(id);
                Task::done(AgenticChatEvent::MessageSelected(id))
            },
            Message::ClearChat => {
                self.clear();
                Task::done(AgenticChatEvent::ClearHistory)
            },
        }
    }

    /// Render the widget
    pub fn view(&self) -> Element<'_, Message> {
        // Build chat history
        let mut chat_column = Column::new().spacing(10).padding(10);

        for msg in &self.messages {
            let role_label = match &msg.role {
                AgentRole::User => "User",
                AgentRole::Agent => "Agent",
                AgentRole::System => "System",
                AgentRole::Tool { name } => name.as_str(),
            };

            let mut message_content =
                column![text(role_label).size(12), text(&msg.content).size(16),].spacing(5);

            // Add event type if configured
            if self.config.show_event_types {
                let event_label = match &msg.event_type {
                    EventType::UserInput => "Input",
                    EventType::AgentResponse => "Response",
                    EventType::ToolInvocation { tool_name, .. } => tool_name.as_str(),
                    EventType::ToolResult { success, .. } => {
                        if *success {
                            "Success"
                        } else {
                            "Failed"
                        }
                    },
                    EventType::SystemEvent { event_type } => event_type.as_str(),
                    EventType::SessionEvent { event_type } => event_type.as_str(),
                };
                message_content = message_content.push(text(format!("[{}]", event_label)).size(10));
            }

            // Add timestamp if configured
            if self.config.show_timestamps {
                let elapsed = msg.timestamp.elapsed();
                message_content = message_content
                    .push(text(format!("{:.1}s ago", elapsed.as_secs_f64())).size(10));
            }

            let message_container = container(message_content).padding(10);

            // Highlight selected message
            let message_container = if Some(msg.id) == self.selected_message {
                message_container.style(|_theme| {
                    let mut style = container::Style::default();
                    style.background = Some(iced::Background::Color(iced::Color::from_rgba(
                        0.3, 0.3, 0.5, 0.2,
                    )));
                    style
                })
            } else {
                message_container
            };

            chat_column = chat_column.push(
                button(message_container)
                    .on_press(Message::SelectMessage(msg.id))
                    .padding(0),
            );
        }

        let chat_scroll = scrollable(chat_column)
            .height(Length::Fill)
            .id(Self::scroll_id())
            .on_scroll(Message::ScrollViewportChanged);

        // Input area
        let input_row = row![
            text_input("Type a message...", &self.input_value)
                .on_input(Message::InputChanged)
                .on_submit(Message::SendMessage)
                .padding(10)
                .size(16),
            button(text("Send").size(16))
                .on_press(Message::SendMessage)
                .padding(10),
            button(text("Clear").size(16))
                .on_press(Message::ClearChat)
                .padding(10),
        ]
        .spacing(10)
        .padding(10);

        container(column![chat_scroll, input_row].spacing(10))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

impl Default for AgenticChat {
    fn default() -> Self {
        Self::new()
    }
}
