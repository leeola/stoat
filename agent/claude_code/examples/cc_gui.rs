use iced::{
    Element, Length, Task, Theme,
    widget::{
        Column, button, column, container, row, scrollable,
        scrollable::{AbsoluteOffset, Id},
        text, text_input,
    },
};
use std::sync::Arc;
use stoat_agent_claude_code::{
    claude_code::{ClaudeCode, SessionConfig},
    messages::{MessageContent, SdkMessage, UserContent, UserContentBlock},
};
use tracing_subscriber::EnvFilter;

fn chat_scroll_id() -> Id {
    Id::new("chat_scroll")
}

struct App {
    claude: Arc<smol::lock::Mutex<Option<ClaudeCode>>>,
    input_value: String,
    messages: Vec<ChatMessage>,
    waiting_for_response: bool,
    process_alive: bool,
    auto_scroll: bool,
}

#[derive(Debug, Clone)]
struct ChatMessage {
    role: MessageRole,
    content: String,
}

#[derive(Debug, Clone)]
enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone)]
enum Message {
    InputChanged(String),
    Send,
    ResponseReceived(String),
    SdkMessageReceived(SdkMessage),
    ScrollViewportChanged(scrollable::Viewport),
    ProcessStatusUpdate(bool),
    Tick,
    Exit,
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("stoat_agent_claude_code=debug")),
            )
            .try_init();

        let app = Self {
            claude: Arc::new(smol::lock::Mutex::new(None)),
            input_value: String::new(),
            messages: vec![ChatMessage {
                role: MessageRole::System,
                content: "Initializing Claude Code...".to_string(),
            }],
            waiting_for_response: false,
            process_alive: false,
            auto_scroll: true,
        };

        let claude_arc = Arc::clone(&app.claude);
        let init_task = Task::perform(
            async move {
                let config = SessionConfig {
                    model: Some("sonnet".to_string()),
                    ..Default::default()
                };

                match ClaudeCode::new(config).await {
                    Ok(mut claude) => {
                        let session_id = claude.get_session_id().to_string();
                        let alive = claude.is_alive();
                        *claude_arc.lock().await = Some(claude);
                        (format!("Session started: {session_id}"), alive)
                    },
                    Err(e) => (format!("Failed to initialize: {e}"), false),
                }
            },
            |(msg, _alive)| Message::ResponseReceived(msg),
        );

        (app, init_task)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::InputChanged(value) => {
                self.input_value = value;
                Task::none()
            },
            Message::Send => {
                if self.input_value.trim().is_empty() {
                    return Task::none();
                }

                let content = self.input_value.clone();
                self.input_value.clear();

                self.messages.push(ChatMessage {
                    role: MessageRole::User,
                    content: content.clone(),
                });

                self.waiting_for_response = true;

                let claude = Arc::clone(&self.claude);
                Task::perform(
                    async move {
                        let mut claude_guard = claude.lock().await;
                        if let Some(claude) = claude_guard.as_mut() {
                            let _ = claude.send_message(&content).await;
                        }
                    },
                    |_| Message::Tick,
                )
                .chain(if self.auto_scroll {
                    scrollable::scroll_to(
                        chat_scroll_id(),
                        AbsoluteOffset {
                            x: 0.0,
                            y: f32::MAX,
                        },
                    )
                } else {
                    Task::none()
                })
            },
            Message::SdkMessageReceived(sdk_msg) => {
                match sdk_msg {
                    SdkMessage::Assistant { message, .. } => {
                        let mut content_parts = Vec::new();
                        for content in &message.content {
                            match content {
                                MessageContent::Text { text } => {
                                    content_parts.push(text.clone());
                                },
                                MessageContent::ToolUse { name, id, .. } => {
                                    content_parts.push(format!(
                                        "[Using tool: {} ({})]",
                                        name,
                                        &id[0..8.min(id.len())]
                                    ));
                                },
                            }
                        }

                        if !content_parts.is_empty() {
                            self.messages.push(ChatMessage {
                                role: MessageRole::Assistant,
                                content: content_parts.join(""),
                            });
                            self.waiting_for_response = false;
                        }
                    },
                    SdkMessage::System { session_id, .. } => {
                        self.messages.push(ChatMessage {
                            role: MessageRole::System,
                            content: format!("[System initialized for session: {session_id}]"),
                        });
                    },
                    SdkMessage::Result {
                        subtype,
                        duration_ms,
                        ..
                    } => {
                        let status = format!("[Session result: {subtype:?} in {duration_ms}ms]");
                        self.messages.push(ChatMessage {
                            role: MessageRole::System,
                            content: status,
                        });
                    },
                    SdkMessage::User { message, .. } => match &message.content {
                        UserContent::Text(_) => {},
                        UserContent::Blocks(blocks) => {
                            for block in blocks {
                                match block {
                                    UserContentBlock::ToolResult {
                                        tool_use_id,
                                        content,
                                    } => {
                                        let short_id = &tool_use_id[0..8.min(tool_use_id.len())];
                                        let preview = if content.len() > 100 {
                                            format!("{}...", &content[0..100])
                                        } else {
                                            content.clone()
                                        };
                                        self.messages.push(ChatMessage {
                                            role: MessageRole::System,
                                            content: format!(
                                                "[Tool result ({short_id})]: {preview}"
                                            ),
                                        });
                                    },
                                    UserContentBlock::Text { .. } => {},
                                }
                            }
                        },
                    },
                }
                if self.auto_scroll {
                    scrollable::scroll_to(
                        chat_scroll_id(),
                        AbsoluteOffset {
                            x: 0.0,
                            y: f32::MAX,
                        },
                    )
                } else {
                    Task::none()
                }
            },
            Message::ResponseReceived(response) => {
                self.messages.push(ChatMessage {
                    role: MessageRole::Assistant,
                    content: response,
                });
                self.waiting_for_response = false;
                if self.auto_scroll {
                    scrollable::scroll_to(
                        chat_scroll_id(),
                        AbsoluteOffset {
                            x: 0.0,
                            y: f32::MAX,
                        },
                    )
                } else {
                    Task::none()
                }
            },
            Message::ProcessStatusUpdate(alive) => {
                if self.process_alive != alive {
                    self.process_alive = alive;
                    let status = if alive {
                        "[ALIVE] Process running"
                    } else {
                        "[DEAD] Process not running"
                    };
                    self.messages.push(ChatMessage {
                        role: MessageRole::System,
                        content: status.to_string(),
                    });
                }
                if self.auto_scroll {
                    scrollable::scroll_to(
                        chat_scroll_id(),
                        AbsoluteOffset {
                            x: 0.0,
                            y: f32::MAX,
                        },
                    )
                } else {
                    Task::none()
                }
            },
            Message::ScrollViewportChanged(viewport) => {
                let at_bottom = viewport.relative_offset().y >= 0.95;
                self.auto_scroll = at_bottom;
                Task::none()
            },
            Message::Exit => iced::exit(),
            Message::Tick => {
                let claude = Arc::clone(&self.claude);
                Task::perform(
                    async move {
                        let mut claude_guard = claude.lock().await;
                        if let Some(claude) = claude_guard.as_mut() {
                            if let Ok(Some(msg)) = claude
                                .recv_any_message(std::time::Duration::from_millis(100))
                                .await
                            {
                                return Some((Some(msg), claude.is_alive()));
                            }
                            return Some((None, claude.is_alive()));
                        }
                        None
                    },
                    |result| {
                        if let Some((msg, alive)) = result {
                            if let Some(message) = msg {
                                Message::SdkMessageReceived(message)
                            } else {
                                Message::ProcessStatusUpdate(alive)
                            }
                        } else {
                            Message::Tick
                        }
                    },
                )
            },
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let title = row![
            text("Claude Code Interactive Test").size(24),
            text(if self.process_alive {
                " [ALIVE]"
            } else {
                " [DEAD]"
            })
            .size(20),
        ]
        .spacing(10);

        let mut chat_column = Column::new().spacing(10).padding(10);
        for msg in &self.messages {
            let label = match msg.role {
                MessageRole::User => "You",
                MessageRole::Assistant => "Claude",
                MessageRole::System => "System",
            };

            chat_column = chat_column.push(
                container(column![text(label).size(12), text(&msg.content).size(16),].spacing(5))
                    .padding(10),
            );
        }

        if self.waiting_for_response {
            chat_column = chat_column.push(text("Claude is typing...").size(14));
        }

        let chat_scroll = scrollable(chat_column)
            .height(Length::Fill)
            .id(chat_scroll_id())
            .on_scroll(Message::ScrollViewportChanged);

        let input_row = row![
            text_input("Type a message...", &self.input_value)
                .on_input(Message::InputChanged)
                .on_submit(Message::Send)
                .padding(10)
                .size(16),
            button(text("Send").size(16))
                .on_press(Message::Send)
                .padding(10),
        ]
        .spacing(10)
        .padding(10);

        container(
            column![title, chat_scroll, input_row,]
                .spacing(10)
                .padding(20),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        let interval = if self.waiting_for_response {
            std::time::Duration::from_millis(100)
        } else {
            std::time::Duration::from_millis(500)
        };

        iced::Subscription::batch([
            iced::time::every(interval).map(|_| Message::Tick),
            iced::keyboard::on_key_press(|key, _modifiers| match key {
                iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape) => {
                    Some(Message::Exit)
                },
                _ => None,
            }),
        ])
    }
}

fn main() -> iced::Result {
    iced::application("Claude Code GUI Test", App::update, App::view)
        .subscription(App::subscription)
        .theme(|_| Theme::Dark)
        .window_size(iced::Size::new(800.0, 600.0))
        .run_with(App::new)
}
