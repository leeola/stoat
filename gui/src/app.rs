use crate::{
    input,
    widget::{
        agentic_chat, node_canvas, AgenticChat, AgenticChatEvent, AgenticMessage, CommandInfo,
        HelpModal, NodeCanvas, NodeId, NodeWidget, PositionedNode,
    },
};
use iced::{Element, Point, Task};
use std::sync::Arc;
use stoat_agent_claude_code::{ClaudeCode, SessionConfig};
use stoat_core::{input::Action, Stoat};
use tokio::sync::Mutex;
use tracing::{debug, error, trace};

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
    /// Window resized
    WindowResized(iced::Size),
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

        // Create the node canvas with chat widget
        let mut node_canvas = NodeCanvas::new();
        let chat_widget = AgenticChat::new();
        let chat_node_id = NodeId(1);

        // Add chat node to GUI canvas
        node_canvas.add_node(PositionedNode {
            id: chat_node_id,
            position: Point::new(400.0, 100.0), // World coordinates
            widget: NodeWidget::Chat(chat_widget),
        });

        // Set up view state in core with integer positions
        let core_node_id = stoat_core::node::NodeId(chat_node_id.0);
        stoat.view_state_mut().set_position(
            core_node_id,
            stoat_core::view_state::Position::new(400, 100),
        );
        stoat
            .view_state_mut()
            .set_size(core_node_id, stoat_core::view_state::Size::new(400, 600));
        stoat.view_state_mut().select(core_node_id);

        // Set initial viewport size to match window
        stoat.view_state_mut().update_viewport_size(1280, 720);

        debug!("Created node canvas with chat at position (400, 100)");

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
                    if let Some(stoat_key) = input::convert_key(key.clone(), modifiers) {
                        debug!(
                            "Converted key: {:?} in mode: {}",
                            stoat_key,
                            self.stoat.current_mode().as_str()
                        );
                        // Process key through modal system
                        if let Some(action) = self.stoat.user_input(stoat_key) {
                            debug!("Got action: {:?}", action);
                            // Handle the action
                            self.handle_action(action)
                        } else {
                            debug!("No action for key");
                            Task::none()
                        }
                    } else {
                        debug!("Could not convert key: {:?}", key);
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
            Message::WindowResized(size) => {
                // Update viewport size in core's view state
                self.stoat
                    .view_state_mut()
                    .update_viewport_size(size.width as u32, size.height as u32);
                Task::none()
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

        // Get the view state from core and pass it to the node canvas for rendering
        let view_state = self.stoat.view_state();
        let canvas = self.node_canvas.view(view_state);

        // Get command info state and create view
        let command_info_state = self.stoat.get_command_info_state();
        let command_info = CommandInfo::view(command_info_state);

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

        // Get help state for lifetime management
        let help_state = self.stoat.get_help_state();

        // Stack canvas and command info
        let mut layers = vec![canvas, positioned_command_info.into()];

        // Add help modal if visible
        if help_state.visible {
            layers.push(HelpModal::view(help_state));
        }

        let main_content = stack(layers).width(Length::Fill).height(Length::Fill);

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
            // Window resize events
            iced::window::resize_events().map(|(_, size)| Message::WindowResized(size)),
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
                Task::none()
            },
            Action::GatherNodes => {
                trace!("Gather nodes in canvas");
                // GatherNodes is now handled in core lib.rs
                // The action has already been processed by stoat.user_input()
                Task::none()
            },
            Action::ShowHelp => {
                debug!("ShowHelp action - this should have been converted to ChangeMode(Help)");
                // This should not happen since modal system converts ShowHelp to ChangeMode(Help)
                Task::none()
            },
            Action::ShowActionHelp(_) | Action::ShowModeHelp(_) => {
                // These actions are now handled purely by the core state management
                // The view method automatically shows the correct help based on get_help_state()
                debug!("Help action handled by core state management");
                Task::none()
            },
        }
    }
}
