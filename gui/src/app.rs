use crate::{canvas, state::RenderState};
use iced::Element;

/// Main application state
pub struct App {
    /// The render state containing all visual data
    render_state: RenderState,
}

/// Application messages (none for now since we're just rendering)
#[derive(Debug, Clone)]
pub enum Message {}

impl App {
    /// Run the application
    pub fn run() -> iced::Result {
        iced::application("Stoat - Node Editor Prototype", Self::update, Self::view)
            .window_size(iced::Size::new(1280.0, 720.0))
            .run_with(Self::new)
    }

    fn new() -> (Self, iced::Task<Message>) {
        (
            Self {
                render_state: RenderState::stub(),
            },
            iced::Task::none(),
        )
    }

    fn update(&mut self, _message: Message) -> iced::Task<Message> {
        // No messages to handle yet
        iced::Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        // Create the canvas with our render state
        iced::widget::canvas(canvas::NodeCanvas::new(&self.render_state))
            .width(iced::Length::Fill)
            .height(iced::Length::Fill)
            .into()
    }
}
