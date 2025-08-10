use crate::{
    canvas, input,
    state::RenderState,
    widget::{agentic_chat, AgenticChat, AgenticChatEvent, AgenticMessage},
};
use iced::{Border, Element, Task};
use std::sync::Arc;
use stoat_agent_claude_code::{ClaudeCode, SessionConfig};
use stoat_core::{input::Action, Stoat};
use tokio::sync::Mutex;
use tracing::{debug, error, trace, warn};

/// Main application state
pub struct App {
    /// The render state containing all visual data
    render_state: RenderState,
    /// The Stoat editor instance
    stoat: Stoat,
    /// The ClaudeCode instance for agent chat
    claude: Arc<Mutex<Option<ClaudeCode>>>,
    /// The agentic chat widget
    chat_widget: AgenticChat,
    /// Process status
    process_alive: bool,
    /// Session ID for display
    session_id: Option<String>,
    /// Position of the chat node on canvas
    chat_node_position: (f32, f32),
    /// Size of the chat node
    chat_node_size: (f32, f32),
}

/// Application messages
#[derive(Debug, Clone)]
pub enum Message {
    /// Keyboard event received
    KeyPressed(iced::keyboard::Event),
    /// Chat widget's internal message
    ChatWidgetMessage(agentic_chat::Message),
    /// Chat widget event
    ChatEvent(AgenticChatEvent),
    /// Process status update
    ProcessStatusUpdate(bool),
    /// Session initialized
    SessionInitialized(String, bool),
    /// Message received from Claude
    MessageReceived(stoat_agent_claude_code::messages::SdkMessage),
    /// Tick for updating modal system and polling
    Tick,
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

        // OLD NODES DISABLED - Using agentic chat instead
        // // Create a text node with Hello World content
        // let node_id = NodeId(1);
        // // Create config as a simple String value since TextNodeInit supports that
        // let config = Value::String("Hello World!".into());
        // if let Ok(text_node) =
        //     create_node_from_registry("text", node_id, "hello_world".to_string(), config)
        // {
        //     // Add node to workspace
        //     stoat.workspace_mut().add_node(text_node);
        //     // Add node to view at grid position (0, 0)
        //     stoat.workspace_mut().view_mut().add_node_view(
        //         node_id,
        //         stoat_core::node::NodeType::Text,
        //         GridPosition::new(0, 0),
        //     );
        // }
        // // Create a text edit node with rope-based content
        // let text_edit_node_id = NodeId(2);
        // let text_edit_config =
        //     Value::String("Welcome to rope-based editing!\nThis is line 2\nThis is line
        // 3".into()); if let Ok(text_edit_node) = create_node_from_registry(
        //     "text_edit",
        //     text_edit_node_id,
        //     "rope_editor".to_string(),
        //     text_edit_config,
        // ) {
        //     // Add text edit node to workspace
        //     stoat.workspace_mut().add_node(text_edit_node);
        //     // Add text edit node to view at grid position (1, 0) - next to the text node
        //     stoat.workspace_mut().view_mut().add_node_view(
        //         text_edit_node_id,
        //         stoat_core::node::NodeType::TextEdit,
        //         GridPosition::new(1, 0),
        //     );
        // }

        // Create agentic chat widget
        let chat_widget = AgenticChat::new();

        // Create render state from workspace
        let render_state = Self::create_render_state(&stoat);

        debug!(
            "Created render state with {} nodes",
            render_state.nodes.len()
        );
        for node in &render_state.nodes {
            debug!("Node {}: {} at {:?}", node.id.0, node.title, node.position);
        }

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
                render_state,
                stoat,
                claude,
                chat_widget,
                process_alive: false,
                session_id: None,
                chat_node_position: (400.0, 100.0), // Position chat node on canvas
                chat_node_size: (400.0, 600.0),     // Size of chat node
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
                            let task = self.handle_action(action);

                            // Update render state after action
                            self.render_state = Self::create_render_state(&self.stoat);

                            task
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
            Message::ChatWidgetMessage(widget_msg) => {
                // Update the widget and get any events
                let event_task = self.chat_widget.update(widget_msg);
                // Map the event to our ChatEvent message
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
                AgenticChatEvent::MessageSelected(id) => {
                    // Future: highlight corresponding node in graph
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

                // Add initialization message
                self.chat_widget.add_message(AgenticMessage::new(
                    agentic_chat::AgentRole::System,
                    format!("Agent session initialized: {}", session_id),
                    agentic_chat::EventType::SessionEvent {
                        event_type: "initialized".to_string(),
                    },
                ));
                Task::none()
            },
            Message::MessageReceived(sdk_msg) => {
                debug!("Processing SDK message: {:?}", sdk_msg);
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
        }
    }

    fn view(&self) -> Element<'_, Message> {
        use crate::widget::StatusBar;
        use iced::widget::{column, container, stack};

        // Create enhanced status bar
        let status_bar = StatusBar::create(
            self.stoat.current_mode().as_str(),
            Some("Stoat Editor - Agentic Chat".to_string()),
        );

        // Create the main canvas that fills the entire view
        let canvas = iced::widget::canvas(canvas::NodeCanvas::new(&self.render_state))
            .width(iced::Length::Fill)
            .height(iced::Length::Fill);

        // Map the chat widget's internal messages to our app messages
        let chat_element = self.chat_widget.view().map(Message::ChatWidgetMessage);

        // Style the chat widget as a floating node with fixed size
        let chat_panel = container(chat_element)
            .width(iced::Length::Fixed(self.chat_node_size.0))
            .height(iced::Length::Fixed(self.chat_node_size.1))
            .style(|_theme| {
                let mut style = container::Style::default();
                style.background = Some(iced::Background::Color(iced::Color::from_rgb(
                    0.15, 0.15, 0.17,
                )));
                style.border = Border {
                    color: iced::Color::from_rgb(0.3, 0.3, 0.35),
                    width: 1.0,
                    radius: 8.0.into(),
                };
                style
            })
            .padding(10);

        // Position the chat panel absolutely using a container
        let positioned_chat = container(chat_panel)
            .width(iced::Length::Fill)
            .height(iced::Length::Fill)
            .padding(iced::Padding {
                top: self.chat_node_position.1,
                right: 0.0,
                bottom: 0.0,
                left: self.chat_node_position.0,
            })
            .align_x(iced::alignment::Horizontal::Left)
            .align_y(iced::alignment::Vertical::Top);

        // Layer the canvas and chat using a stack
        let main_content = stack![canvas, positioned_chat]
            .width(iced::Length::Fill)
            .height(iced::Length::Fill);

        // Combine main content and status bar (status bar at bottom)
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
        }
    }

    /// Create render state from the current workspace
    fn create_render_state(stoat: &Stoat) -> RenderState {
        use crate::{
            grid_layout::GridLayout,
            state::{NodeContent, NodeId as GuiNodeId, NodeRenderData, NodeState},
        };

        let grid_layout = GridLayout::new();
        let view = stoat.view();
        let workspace = stoat.workspace();

        let nodes: Vec<NodeRenderData> = view
            .nodes
            .iter()
            .filter_map(|node_view| {
                // Get the actual node from workspace
                if let Some(node) = workspace.get_node(node_view.id) {
                    let position = grid_layout.grid_to_screen(node_view.pos);
                    let size = grid_layout.cell_size();

                    // Convert content based on node type
                    let content = if let Some(text_node) =
                        node.as_any().downcast_ref::<stoat_core::nodes::TextNode>()
                    {
                        NodeContent::Text {
                            lines: text_node.content().lines().map(|s| s.to_string()).collect(),
                            cursor_position: None,
                            selection: None,
                        }
                    } else if let Some(text_edit_node) =
                        node.as_any()
                            .downcast_ref::<stoat_core::nodes::TextEditNode>()
                    {
                        // Convert TextEditNode to InteractiveText content
                        Self::create_interactive_text_content(text_edit_node)
                    } else {
                        NodeContent::Empty
                    };

                    Some(NodeRenderData {
                        id: GuiNodeId(node_view.id.0),
                        position,
                        size,
                        title: node.name().to_string(),
                        content,
                        state: NodeState::Normal,
                    })
                } else {
                    None
                }
            })
            .collect();

        // Don't add the chat node to the canvas nodes since it's rendered as an overlay
        // This prevents double rendering (once as a canvas node, once as the actual widget)

        // Center viewport on (0,0) with some offset to show the node nicely
        let viewport = crate::state::Viewport {
            offset: (100.0, 100.0), // Small offset so node isn't at edge
            zoom: 1.0,
        };

        RenderState {
            viewport,
            nodes,
            focused_node: None,
            grid_layout,
        }
    }

    /// Convert TextEditNode to InteractiveText content for rendering
    fn create_interactive_text_content(
        text_edit_node: &stoat_core::nodes::TextEditNode,
    ) -> crate::state::NodeContent {
        use crate::state::NodeContent;

        // Get buffer information
        let buffer = text_edit_node.buffer();
        let buffer_id = buffer.id();

        // Get current text and cursor position
        let text = text_edit_node.content();
        let cursor_position = text.len(); // Start at end for simplicity

        NodeContent::InteractiveText {
            text,
            cursor_position,
            focused: false, // Initial state, will be managed by focus system
            placeholder: "Enter text...".to_string(),
            buffer_id,
        }
    }
}
