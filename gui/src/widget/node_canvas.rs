use crate::widget::{agentic_chat, AgenticChat, AgenticChatEvent};
use iced::{
    widget::{container, stack},
    Element, Length, Padding, Point, Task,
};
use stoat_core::view_state::ViewState;

/// A unique identifier for nodes in the canvas
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);

/// The spatial container that positions widgets in world space
/// This is now a pure presentation layer that renders from core's CanvasView
#[derive(Default)]
pub struct NodeCanvas {
    pub nodes: Vec<PositionedNode>,
}

/// A widget positioned at specific world coordinates
pub struct PositionedNode {
    pub id: NodeId,
    pub position: Point,    // World coordinates
    pub widget: NodeWidget, // The actual widget
}

/// Types of widgets that can be nodes
pub enum NodeWidget {
    Chat(AgenticChat), // The agentic chat widget
}

/// Messages for node canvas interactions
#[derive(Debug, Clone)]
pub enum Message {
    ChatMessage(agentic_chat::Message),
    ChatEvent(AgenticChatEvent),
}

impl NodeCanvas {
    /// Create a new empty node canvas
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a node to the canvas
    pub fn add_node(&mut self, node: PositionedNode) {
        self.nodes.push(node);
    }

    /// Find a mutable reference to a chat widget by ID
    pub fn find_chat_mut(&mut self, id: NodeId) -> Option<&mut AgenticChat> {
        self.nodes.iter_mut().find_map(|node| {
            if node.id == id {
                match &mut node.widget {
                    NodeWidget::Chat(chat) => Some(chat),
                }
            } else {
                None
            }
        })
    }

    /// Update a widget in the canvas
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ChatMessage(msg) => {
                // Find and update the first chat widget
                if let Some(node) = self.nodes.iter_mut().next() {
                    match &mut node.widget {
                        NodeWidget::Chat(chat) => {
                            let event_task = chat.update(msg);
                            return event_task.map(Message::ChatEvent);
                        },
                    }
                }
                Task::none()
            },
            Message::ChatEvent(_) => {
                // Events are handled at the app level
                Task::none()
            },
        }
    }

    /// Create the view of the canvas with all positioned widgets
    /// Now takes a ViewState from core to render
    pub fn view<'a, M>(&'a self, view_state: &ViewState) -> Element<'a, M>
    where
        M: 'a + From<Message>,
    {
        // Start with an empty stack
        let mut stack_widgets = vec![];

        for node in &self.nodes {
            // Get position from view state
            let core_id = stoat_core::node::NodeId(node.id.0);

            // Get position from view state and convert to screen coordinates
            let screen_pos = if let Some(&pos) = view_state.positions.get(&core_id) {
                let (screen_x, screen_y) = view_state.canvas_to_screen(pos);
                Point::new(screen_x, screen_y)
            } else {
                // Fallback to GUI position if not in view state
                Point::new(node.position.x, node.position.y)
            };

            // Get the widget element based on type
            let widget_element: Element<'_, Message> = match &node.widget {
                NodeWidget::Chat(chat) => chat.view().map(Message::ChatMessage),
            };

            // Check if this node is selected
            let is_selected = view_state.selected == Some(core_id);

            // Style the widget with a node-like container
            let styled_widget = container(widget_element)
                .style(move |_theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color::from_rgb(
                        0.15, 0.15, 0.17,
                    ))),
                    border: iced::Border {
                        color: if is_selected {
                            iced::Color::from_rgb(0.4, 0.6, 0.9) // Blue border for selected
                        } else {
                            iced::Color::from_rgb(0.3, 0.3, 0.35) // Default border
                        },
                        width: if is_selected { 2.0 } else { 1.0 },
                        radius: 8.0.into(),
                    },
                    ..Default::default()
                })
                .padding(10)
                .width(Length::Fixed(400.0)) // Fixed size for now
                .height(Length::Fixed(600.0));

            // Position the widget using padding in a full-size container
            let positioned = container(styled_widget)
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(Padding {
                    top: screen_pos.y,
                    left: screen_pos.x,
                    right: 0.0,
                    bottom: 0.0,
                });

            stack_widgets.push(positioned.into());
        }

        // Stack all positioned widgets
        let stacked = stack(stack_widgets)
            .width(Length::Fill)
            .height(Length::Fill);

        // Wrap in a container that fills the available space
        let element: Element<'a, Message> = container(stacked)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        // Map to the parent message type
        element.map(M::from)
    }
}
