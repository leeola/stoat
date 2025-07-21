use crate::{canvas, input, state::RenderState};
use iced::Element;
use stoat_core::{input::Action, Stoat};
use tracing::{debug, trace, warn};

/// Main application state
pub struct App {
    /// The render state containing all visual data
    render_state: RenderState,
    /// The Stoat editor instance
    stoat: Stoat,
}

/// Application messages
#[derive(Debug, Clone)]
pub enum Message {
    /// Keyboard event received
    KeyPressed(iced::keyboard::Event),
    /// Tick for updating modal system
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

    fn new() -> (Self, iced::Task<Message>) {
        use stoat_core::{
            node::{create_node_from_registry, NodeId},
            value::Value,
            view::GridPosition,
        };

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

        // Create a text node with Hello World content
        let node_id = NodeId(1);

        // Create config as a simple String value since TextNodeInit supports that
        let config = Value::String("Hello World!".into());

        if let Ok(text_node) =
            create_node_from_registry("text", node_id, "hello_world".to_string(), config)
        {
            // Add node to workspace
            stoat.workspace_mut().add_node(text_node);

            // Add node to view at grid position (0, 0)
            stoat.workspace_mut().view_mut().add_node_view(
                node_id,
                stoat_core::node::NodeType::Text,
                GridPosition::new(0, 0),
            );
        }

        // Create a text edit node with rope-based content
        let text_edit_node_id = NodeId(2);
        let text_edit_config =
            Value::String("Welcome to rope-based editing!\nThis is line 2\nThis is line 3".into());

        if let Ok(text_edit_node) = create_node_from_registry(
            "text_edit",
            text_edit_node_id,
            "rope_editor".to_string(),
            text_edit_config,
        ) {
            // Add text edit node to workspace
            stoat.workspace_mut().add_node(text_edit_node);

            // Add text edit node to view at grid position (1, 0) - next to the text node
            stoat.workspace_mut().view_mut().add_node_view(
                text_edit_node_id,
                stoat_core::node::NodeType::TextEdit,
                GridPosition::new(1, 0),
            );
        }

        // Create render state from workspace
        let render_state = Self::create_render_state(&stoat);

        debug!(
            "Created render state with {} nodes",
            render_state.nodes.len()
        );
        for node in &render_state.nodes {
            debug!("Node {}: {} at {:?}", node.id.0, node.title, node.position);
        }

        (
            Self {
                render_state,
                stoat,
            },
            iced::Task::none(),
        )
    }

    fn update(&mut self, message: Message) -> iced::Task<Message> {
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
                            iced::Task::none()
                        }
                    } else {
                        iced::Task::none()
                    }
                } else {
                    iced::Task::none()
                }
            },
            Message::Tick => {
                // Update the modal system's timeout handling
                self.stoat.tick();
                iced::Task::none()
            },
        }
    }

    fn view(&self) -> Element<'_, Message> {
        use crate::widget::StatusBar;
        use iced::widget::column;

        // Create enhanced status bar
        let status_bar = StatusBar::create(
            self.stoat.current_mode().as_str(),
            Some("Stoat Editor".to_string()),
        );

        // Create the main content
        let canvas = iced::widget::canvas(canvas::NodeCanvas::new(&self.render_state))
            .width(iced::Length::Fill)
            .height(iced::Length::Fill);

        // Combine canvas and status bar (status bar at bottom)
        column![canvas, status_bar].into()
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        // Keyboard subscription
        iced::keyboard::on_key_press(|key, modifiers| {
            Some(Message::KeyPressed(iced::keyboard::Event::KeyPressed {
                key: key.clone(),
                modified_key: key.clone(),
                physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::KeyA),
                location: iced::keyboard::Location::Standard,
                modifiers,
                text: None,
            }))
        })
    }

    fn handle_action(&mut self, action: Action) -> iced::Task<Message> {
        match action {
            Action::ExitApp => {
                // Exit the application
                iced::exit()
            },
            Action::ChangeMode(mode) => {
                // Mode change is handled internally by ModalSystem
                debug!("Changed to {} mode", mode.as_str());
                iced::Task::none()
            },
            Action::Move(direction) => {
                trace!("Move {direction:?}");
                // TODO: Implement movement in the canvas
                iced::Task::none()
            },
            Action::Delete => {
                trace!("Delete");
                iced::Task::none()
            },
            Action::DeleteLine => {
                trace!("Delete line");
                iced::Task::none()
            },
            Action::Yank => {
                trace!("Yank");
                iced::Task::none()
            },
            Action::YankLine => {
                trace!("Yank line");
                iced::Task::none()
            },
            Action::Paste => {
                trace!("Paste");
                iced::Task::none()
            },
            Action::Jump(target) => {
                trace!("Jump to {target:?}");
                iced::Task::none()
            },
            Action::InsertChar => {
                trace!("Insert character");
                // TODO: Get the actual character from the last key press
                iced::Task::none()
            },
            Action::CommandInput => {
                trace!("Command input");
                iced::Task::none()
            },
            Action::ExecuteCommand => {
                trace!("Execute command");
                iced::Task::none()
            },
            Action::ShowActionList => {
                trace!("Show action list");
                // TODO: Display available actions
                iced::Task::none()
            },
            Action::ShowCommandPalette => {
                trace!("Show command palette");
                // TODO: Display command palette
                iced::Task::none()
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
                        // Convert TextEditNode to RopeText content
                        Self::create_rope_text_content(text_edit_node)
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

    /// Convert TextEditNode to RopeText content for rendering
    fn create_rope_text_content(
        text_edit_node: &stoat_core::nodes::TextEditNode,
    ) -> crate::state::NodeContent {
        use crate::state::{CursorPosition, NodeContent};

        // Get buffer information
        let buffer = text_edit_node.buffer();
        let buffer_id = buffer.id();

        // Set up viewport to show first 20 lines
        let total_lines = text_edit_node.line_count();
        let viewport = 0..total_lines.min(20);

        // Extract actual text lines from the rope buffer
        let lines = Self::extract_rope_lines(text_edit_node, viewport.clone());

        // Convert rope cursors to GUI cursor positions
        let cursors: Vec<CursorPosition> = text_edit_node
            .gui_cursor_positions()
            .into_iter()
            .map(|(line, column)| CursorPosition { line, column })
            .collect();

        // For now, create an empty selections array
        // FIXME: Add selection support when text editing operations are implemented
        let selections = Vec::new();

        NodeContent::RopeText {
            buffer_id,
            viewport,
            lines,
            cursors,
            selections,
        }
    }

    /// Extract actual text lines from the rope buffer
    fn extract_rope_lines(
        text_edit_node: &stoat_core::nodes::TextEditNode,
        viewport: std::ops::Range<usize>,
    ) -> Vec<String> {
        // Create a view for the buffer and iterate over lines
        let buffer = text_edit_node.buffer();
        let mut view = stoat_text::view::View::new(buffer);
        view.set_visible_lines(viewport.len());
        if viewport.start > 0 {
            view.set_scroll_line(viewport.start);
        }

        // Collect lines using our rope line iterator
        view.iter_lines(buffer).collect()
    }
}
