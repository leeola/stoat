use crate::{
    input,
    widget::{
        agentic_chat, create_editor, update_editor_state, AgenticChat, AgenticChatEvent,
        AgenticMessage, CommandInfo, CommandPalette, EditorMessage, EditorState, HelpModal,
    },
};
use iced::{Element, Task};
use std::sync::Arc;
use stoat_agent_claude_code::{ClaudeCode, SessionConfig};
use stoat_core::{input::Action, Stoat};
use tokio::sync::Mutex;
use tracing::{debug, error, trace};

/// Main application state
pub struct App {
    /// The Stoat editor instance
    stoat: Stoat,
    /// Editor widget state
    editor_state: EditorState,
    /// The ClaudeCode instance for agent chat
    claude: Arc<Mutex<Option<ClaudeCode>>>,
    /// The agentic chat widget
    chat_widget: AgenticChat,
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
    /// Editor message
    EditorMessage(EditorMessage),
    /// Chat message
    ChatMessage(agentic_chat::Message),
    /// Chat event
    ChatEvent(AgenticChatEvent),
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

impl From<EditorMessage> for Message {
    fn from(msg: EditorMessage) -> Self {
        Message::EditorMessage(msg)
    }
}

impl From<agentic_chat::Message> for Message {
    fn from(msg: agentic_chat::Message) -> Self {
        Message::ChatMessage(msg)
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

        // Create initial editor state
        let mut editor_state = EditorState::default();

        // Create a welcome buffer
        let welcome_buffer_id = stoat.create_buffer_with_content(
            "*Welcome*".to_string(),
            "Welcome to Stoat Editor\n\nUse Ctrl+O to open files\nUse Ctrl+S to save\n".to_string(),
        );
        editor_state.set_active_buffer(Some(welcome_buffer_id));
        editor_state.set_focused(true);

        // Create the chat widget
        let chat_widget = AgenticChat::new();

        // Set initial viewport size to match window
        stoat.view_state_mut().update_viewport_size(1280, 720);

        debug!("Created editor with welcome buffer");

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
                stoat,
                editor_state,
                claude,
                chat_widget,
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
            Message::EditorMessage(editor_msg) => {
                // Update the editor state
                update_editor_state(&mut self.editor_state, editor_msg, self.stoat.buffers_mut());
                Task::none()
            },
            Message::ChatMessage(chat_msg) => {
                // Update the chat widget
                let event_task = self.chat_widget.update(chat_msg);
                event_task.map(Message::ChatEvent)
            },
            Message::ChatEvent(event) => match event {
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
                AgenticChatEvent::UserMessageForNode(message_id, content) => {
                    debug!("Creating user message buffer for: {}", content);

                    // Create a buffer for the user message content
                    let buffer_name = format!("User Message {}", message_id.uuid());
                    let buffer_id = self
                        .stoat
                        .create_buffer_with_content(buffer_name, content.clone());

                    // Switch the editor to show this buffer
                    self.editor_state.set_active_buffer(Some(buffer_id));

                    debug!("Created user message buffer and switched to it");
                    Task::none()
                },
                AgenticChatEvent::MessageSelected(id) => {
                    // Future: switch to corresponding buffer
                    debug!("Message selected: {:?}", id);
                    Task::none()
                },
                AgenticChatEvent::ScrollToMessage(_) | AgenticChatEvent::ClearHistory => {
                    Task::none()
                },
            },
            Message::ProcessStatusUpdate(alive) => {
                if self.process_alive != alive {
                    self.process_alive = alive;
                    let status = if alive {
                        "Agent process is running"
                    } else {
                        "Agent process stopped"
                    };
                    // Update the chat widget directly
                    self.chat_widget.add_message(AgenticMessage::new(
                        agentic_chat::AgentRole::System,
                        status.to_string(),
                        agentic_chat::EventType::SystemEvent {
                            event_type: "process_status".to_string(),
                        },
                    ));
                }
                Task::none()
            },
            Message::SessionInitialized(session_id, alive) => {
                self.session_id = Some(session_id.clone());
                self.process_alive = alive;

                // Add initialization message to chat widget
                self.chat_widget.add_message(AgenticMessage::new(
                    agentic_chat::AgentRole::System,
                    format!("Agent session initialized: {session_id}"),
                    agentic_chat::EventType::SessionEvent {
                        event_type: "initialized".to_string(),
                    },
                ));
                Task::none()
            },
            Message::MessageReceived(sdk_msg) => {
                debug!("Processing SDK message: {:?}", sdk_msg);
                // Process SDK message in chat widget directly
                self.chat_widget.process_sdk_message(sdk_msg);
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

        // Create enhanced status bar with buffer name if available
        let buffer_info = if let Some(buffer_id) = self.editor_state.active_buffer {
            if let Some(info) = self.stoat.buffers().get_info(buffer_id) {
                Some(format!("Stoat Editor - {}", info.name))
            } else {
                Some("Stoat Editor - [Invalid Buffer]".to_string())
            }
        } else {
            Some("Stoat Editor - No Buffer".to_string())
        };

        let status_bar = StatusBar::create(self.stoat.current_mode().as_str(), buffer_info);

        // Create the editor view
        let editor = create_editor(&self.editor_state, self.stoat.buffers(), |msg| {
            Message::EditorMessage(msg)
        });

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

        // Stack editor and command info
        let mut layers = vec![editor, positioned_command_info.into()];

        // Add help modal if visible
        if help_state.visible {
            layers.push(HelpModal::view(help_state));
        }

        let main_content = stack(layers).width(Length::Fill).height(Length::Fill);

        // Create command palette
        let command_palette =
            CommandPalette::view(self.stoat.current_mode(), self.stoat.command_input_state());

        // Combine with command palette and status bar
        column![main_content, command_palette, status_bar].into()
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
                trace!("Gather nodes (deprecated in buffer mode)");
                // This action is deprecated in buffer-centric mode
                Task::none()
            },
            Action::AlignNodes => {
                trace!("Align nodes (deprecated in buffer mode)");
                // This action is deprecated in buffer-centric mode
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
            Action::ExecuteCommand(name, args) => {
                debug!("Execute command: {} with {} arguments", name, args.len());
                // Command execution is handled internally by Stoat core
                // Results could be processed here if needed
                Task::none()
            },
        }
    }
}
