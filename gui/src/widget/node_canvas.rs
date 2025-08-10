use crate::widget::{agentic_chat, AgenticChat, AgenticChatEvent};
use iced::{
    widget::{container, stack},
    Element, Length, Padding, Point, Task, Vector,
};

/// A unique identifier for nodes in the canvas
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);

/// The spatial container that positions widgets in world space
pub struct NodeCanvas {
    pub nodes: Vec<PositionedNode>,
    pub viewport: Viewport,
}

/// A widget positioned at specific world coordinates
pub struct PositionedNode {
    pub id: NodeId,
    pub position: Point,    // World coordinates
    pub widget: NodeWidget, // The actual widget
}

/// The view into the world space
pub struct Viewport {
    pub offset: Vector, // Camera pan position
    pub zoom: f32,      // Zoom level (1.0 = 100%)
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
        Self {
            nodes: Vec::new(),
            viewport: Viewport {
                offset: Vector::new(0.0, 0.0),
                zoom: 1.0,
            },
        }
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

    /// Transform world coordinates to screen coordinates
    fn world_to_screen(&self, world_pos: Point) -> Point {
        Point::new(
            (world_pos.x - self.viewport.offset.x) * self.viewport.zoom,
            (world_pos.y - self.viewport.offset.y) * self.viewport.zoom,
        )
    }

    /// Update a widget in the canvas
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ChatMessage(msg) => {
                // Find and update the first chat widget
                for node in &mut self.nodes {
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
    pub fn view<'a, M>(&'a self) -> Element<'a, M>
    where
        M: 'a + From<Message>,
    {
        // Start with an empty stack
        let mut stack_widgets = vec![];

        for node in &self.nodes {
            let screen_pos = self.world_to_screen(node.position);

            // Get the widget element based on type
            let widget_element: Element<'_, Message> = match &node.widget {
                NodeWidget::Chat(chat) => chat.view().map(Message::ChatMessage),
            };

            // Style the widget with a node-like container
            let styled_widget = container(widget_element)
                .style(|_theme| {
                    let mut style = container::Style::default();
                    style.background = Some(iced::Background::Color(iced::Color::from_rgb(
                        0.15, 0.15, 0.17,
                    )));
                    style.border = iced::Border {
                        color: iced::Color::from_rgb(0.3, 0.3, 0.35),
                        width: 1.0,
                        radius: 8.0.into(),
                    };
                    style
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
