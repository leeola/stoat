use crate::{
    input,
    widget::{
        agentic_chat, node_canvas, AgenticChat, AgenticChatEvent, AgenticMessage, CommandInfo,
        NodeCanvas, NodeId, NodeWidget, PositionedNode,
    },
};
use iced::{Element, Point, Task};
use std::sync::Arc;
use stoat_agent_claude_code::{ClaudeCode, SessionConfig};
use stoat_core::{input::Action, Stoat};
use tokio::sync::Mutex;
use tracing::{debug, error, trace, warn};

/// Main application state
pub struct App {
    /// The spatial node canvas
    node_canvas: NodeCanvas,
    /// The Stoat editor instance
    stoat: Stoat,
    /// The ClaudeCode instance for agent chat
    claude: Arc<Mutex<Option<ClaudeCode>>>,
    /// ID of the chat node
    chat_node_id: NodeId,
    /// Process status
    process_alive: bool,
    /// Session ID for display
    session_id: Option<String>,
    /// Command info widget
    command_info: CommandInfo,
}

/// Application messages
#[derive(Debug, Clone)]
pub enum Message {
    /// Keyboard event received
    KeyPressed(iced::keyboard::Event),
    /// Node canvas message (contains chat messages)
    NodeCanvasMessage(node_canvas::Message),
    /// Process status update
    ProcessStatusUpdate(bool),
    /// Session initialized
    SessionInitialized(String, bool),
    /// Message received from Claude
    MessageReceived(stoat_agent_claude_code::messages::SdkMessage),
    /// Tick for updating modal system and polling
    Tick,
}

impl From<node_canvas::Message> for Message {
    fn from(msg: node_canvas::Message) -> Self {
        Message::NodeCanvasMessage(msg)
    }
}

impl App {
    /// Run the application
    pub fn run() -> iced::Result {
        iced::application("Stoat - Node Editor Prototype", Self::update, Self::view)
            .subscription(Self::subscription)
            .window_size(iced::Size::new(1280.0, 720.0))
            .run_with(Self::new)
    }

    fn new() -> (Self, Task<Message>) {
        // Initialize Stoat with default configuration
        let mut stoat = Stoat::new();

        // Try to load the keymap configuration
        if let Ok(keymap_path) = std::env::current_dir().map(|d| d.join("keymap.ron")) {
            if keymap_path.exists() {
                if let Err(e) = stoat.load_modal_config_from_file(&keymap_path) {
                    warn!("Failed to load keymap.ron: {e}");
                }
            }
        }

        // Create the node canvas with chat widget
        let mut node_canvas = NodeCanvas::new();
        let chat_widget = AgenticChat::new();
        let chat_node_id = NodeId(1);

        // Add chat node to canvas at world position
        node_canvas.add_node(PositionedNode {
            id: chat_node_id,
            position: Point::new(400.0, 100.0), // World coordinates
            widget: NodeWidget::Chat(chat_widget),
        });

        debug!("Created node canvas with chat at position (400, 100)");

        // Initialize command info widget with actual bindings
        let mut command_info = CommandInfo::new();
        let bindings = stoat.get_display_bindings();
        command_info.update_from_bindings(stoat.current_mode().as_str(), bindings);

        // Initialize ClaudeCode asynchronously
        let claude = Arc::new(Mutex::new(None));
        let claude_arc = Arc::clone(&claude);
        let init_task = Task::perform(
            async move {
                let config = SessionConfig {
                    model: Some("sonnet".to_string()),
                    ..Default::default()
                };

                match ClaudeCode::new(config).await {
                    Ok(mut claude_instance) => {
                        let session_id = claude_instance.get_session_id();
                        let alive = claude_instance.is_alive().await;
                        *claude_arc.lock().await = Some(claude_instance);
                        (session_id, alive)
                    },
                    Err(e) => {
                        error!("Failed to initialize ClaudeCode: {}", e);
                        (String::new(), false)
                    },
                }
            },
            |(session_id, alive)| Message::SessionInitialized(session_id, alive),
        );

        (
            Self {
                node_canvas,
                stoat,
                claude,
                chat_node_id,
                process_alive: false,
                session_id: None,
                command_info,
            },
            init_task,
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::KeyPressed(event) => {
                // Update tick before processing key
                self.stoat.tick();

                if let iced::keyboard::Event::KeyPressed { key, modifiers, .. } = event {
                    // Convert Iced key to Stoat key
                    if let Some(stoat_key) = input::convert_key(key, modifiers) {
                        // Process key through modal system
                        if let Some(action) = self.stoat.user_input(stoat_key) {
                            // Handle the action
                            self.handle_action(action)
                        } else {
                            Task::none()
                        }
                    } else {
                        Task::none()
                    }
                } else {
                    Task::none()
                }
            },
            Message::NodeCanvasMessage(canvas_msg) => {
                match canvas_msg {
                    node_canvas::Message::ChatMessage(_) => {
                        // Update the node canvas (which will update the chat widget)
                        let task = self.node_canvas.update(canvas_msg.clone());
                        task.map(Message::NodeCanvasMessage)
                    },
                    node_canvas::Message::ChatEvent(event) => match event {
                        AgenticChatEvent::MessageSubmitted(content) => {
                            debug!("User submitted message: {}", content);
                            // Send message to Claude
                            let claude = Arc::clone(&self.claude);
                            Task::perform(
                                async move {
                                    let mut claude_guard = claude.lock().await;
                                    if let Some(claude) = claude_guard.as_mut() {
                                        debug!("Sending message to Claude");
                                        if let Err(e) = claude.send_message(&content).await {
                                            error!("Failed to send message to Claude: {}", e);
                                        }
                                    } else {
                                        error!("Claude not initialized");
                                    }
                                },
                                |_| Message::Tick,
                            )
                        },
                        AgenticChatEvent::MessageSelected(id) => {
                            // Future: highlight corresponding node in graph
                            debug!("Message selected: {:?}", id);
                            Task::none()
                        },
                        AgenticChatEvent::ScrollToMessage(_) | AgenticChatEvent::ClearHistory => {
                            Task::none()
                        },
                    },
                }
            },
            Message::ProcessStatusUpdate(alive) => {
                if self.process_alive != alive {
                    self.process_alive = alive;
                    let status = if alive {
                        "Agent process is running"
                    } else {
                        "Agent process stopped"
                    };
                    // Find and update the chat widget in the node canvas
                    if let Some(chat) = self.node_canvas.find_chat_mut(self.chat_node_id) {
                        chat.add_message(AgenticMessage::new(
                            agentic_chat::AgentRole::System,
                            status.to_string(),
                            agentic_chat::EventType::SystemEvent {
                                event_type: "process_status".to_string(),
                            },
                        ));
                    }
                }
                Task::none()
            },
            Message::SessionInitialized(session_id, alive) => {
                self.session_id = Some(session_id.clone());
                self.process_alive = alive;

                // Add initialization message to chat in node canvas
                if let Some(chat) = self.node_canvas.find_chat_mut(self.chat_node_id) {
                    chat.add_message(AgenticMessage::new(
                        agentic_chat::AgentRole::System,
                        format!("Agent session initialized: {session_id}"),
                        agentic_chat::EventType::SessionEvent {
                            event_type: "initialized".to_string(),
                        },
                    ));
                }
                Task::none()
            },
            Message::MessageReceived(sdk_msg) => {
                debug!("Processing SDK message: {:?}", sdk_msg);
                // Process SDK message in chat widget within node canvas
                if let Some(chat) = self.node_canvas.find_chat_mut(self.chat_node_id) {
                    chat.process_sdk_message(sdk_msg);
                }
                Task::none()
            },
            Message::Tick => {
                // Update the modal system's timeout handling
                self.stoat.tick();

                // Check for responses and process status
                let claude = Arc::clone(&self.claude);
                Task::perform(
                    async move {
                        let mut claude_guard = claude.lock().await;
                        if let Some(claude) = claude_guard.as_mut() {
                            // Check for any message
                            if let Ok(Some(msg)) = claude
                                .recv_any_message(tokio::time::Duration::from_millis(100))
                                .await
                            {
                                debug!("Received message from Claude: {:?}", msg);
                                return Some((Some(msg), claude.is_alive().await));
                            }
                            let alive = claude.is_alive().await;
                            return Some((None, alive));
                        }
                        None
                    },
                    |result| {
                        if let Some((msg, alive)) = result {
                            if let Some(message) = msg {
                                Message::MessageReceived(message)
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
        use crate::widget::StatusBar;
        use iced::{
            alignment,
            widget::{column, container, stack},
            Length, Padding,
        };

        // Create enhanced status bar
        let status_bar = StatusBar::create(
            self.stoat.current_mode().as_str(),
            Some("Stoat Editor - Node Canvas".to_string()),
        );

        // Get the node canvas view
        let canvas = self.node_canvas.view();

        // Get the command info view
        let command_info = self.command_info.view();

        // Position command info to appear as extension of status bar
        let positioned_command_info = container(command_info)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(alignment::Horizontal::Right)
            .align_y(alignment::Vertical::Bottom)
            .padding(Padding {
                top: 0.0,
                right: 0.0,  // Flush with right edge
                bottom: 1.0, // Overlap status bar border by 1px to avoid double border
                left: 0.0,
            });

        // Stack canvas and command info
        let main_content = stack![canvas, positioned_command_info]
            .width(Length::Fill)
            .height(Length::Fill);

        // Combine with status bar
        column![main_content, status_bar].into()
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        iced::Subscription::batch([
            // Keyboard subscription
            iced::keyboard::on_key_press(|key, modifiers| {
                Some(Message::KeyPressed(iced::keyboard::Event::KeyPressed {
                    key: key.clone(),
                    modified_key: key.clone(),
                    physical_key: iced::keyboard::key::Physical::Code(
                        iced::keyboard::key::Code::KeyA,
                    ),
                    location: iced::keyboard::Location::Standard,
                    modifiers,
                    text: None,
                }))
            }),
            // Poll every 100ms for messages from Claude
            iced::time::every(std::time::Duration::from_millis(100)).map(|_| Message::Tick),
        ])
    }

    fn handle_action(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::ExitApp => {
                // Exit the application
                iced::exit()
            },
            Action::ChangeMode(mode) => {
                // Mode change is handled internally by ModalSystem
                debug!("Changed to {} mode", mode.as_str());
                // Update command info with actual bindings for the new mode
                let bindings = self.stoat.get_display_bindings();
                self.command_info
                    .update_from_bindings(mode.as_str(), bindings);
                Task::none()
            },
            Action::Move(direction) => {
                trace!("Move {direction:?}");
                // TODO: Implement movement in the canvas
                Task::none()
            },
            Action::Delete => {
                trace!("Delete");
                Task::none()
            },
            Action::DeleteLine => {
                trace!("Delete line");
                Task::none()
            },
            Action::Yank => {
                trace!("Yank");
                Task::none()
            },
            Action::YankLine => {
                trace!("Yank line");
                Task::none()
            },
            Action::Paste => {
                trace!("Paste");
                Task::none()
            },
            Action::Jump(target) => {
                trace!("Jump to {target:?}");
                Task::none()
            },
            Action::InsertChar => {
                trace!("Insert character");
                // TODO: Get the actual character from the last key press
                Task::none()
            },
            Action::CommandInput => {
                trace!("Command input");
                Task::none()
            },
            Action::ExecuteCommand => {
                trace!("Execute command");
                Task::none()
            },
            Action::ShowActionList => {
                trace!("Show action list");
                // TODO: Display available actions
                Task::none()
            },
            Action::ShowCommandPalette => {
                trace!("Show command palette");
                // TODO: Display command palette
                Task::none()
            },
            Action::AlignNodes => {
                trace!("Align nodes in canvas");
                // TODO: Implement node alignment in canvas view
                Task::none()
            },
        }
    }
}
